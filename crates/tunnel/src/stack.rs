//! 把裸 IPv6 隧道包装成一个能开 TCP 连接的栈——smoltcp 负责真正的 TCP 状态机
//! (握手/重传/窗口),这里只管"怎么把隧道的字节喂给 smoltcp、怎么把 smoltcp
//! 想发的字节写回隧道"这层管道工作。
//!
//! 设计上没有做成 jktcp 那种"后台任务 + 多个可克隆句柄"的通用形态——这次用
//! 场很窄(先连 RSD 端口做一次握手,再连 DTX 服务端口,顺序用、不需要真正
//! 并发的多连接),用一个简单得多的模型完全够:一个任务独占 `TunnelStack`,
//! 每个操作(`connect`/`send`/`recv`)内部自己循环"泵"(读隧道→喂 smoltcp→
//! 轮询→把 smoltcp 想发的包写回隧道),直到达到目标状态或超时。

use std::net::Ipv6Addr;
use std::str::FromStr;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{IpAddress, IpCidr, Ipv6Address, Ipv6Cidr};
use tokio::io::{AsyncRead, AsyncWrite, WriteHalf};
use tokio::sync::mpsc;

use crate::smol_device::SmolDevice;
use crate::{raw_packet, TunnelError, TunnelInfo};

const TCP_BUFFER_SIZE: usize = 64 * 1024;
const PUMP_TIMEOUT: Duration = Duration::from_secs(10);
const PUMP_IDLE_SLEEP: Duration = Duration::from_millis(10);

pub struct TunnelStack<S> {
    write_half: WriteHalf<S>,
    rx_from_tunnel: mpsc::UnboundedReceiver<Vec<u8>>,
    device: SmolDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    next_local_port: u16,
}

impl<S> TunnelStack<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// `stream` 是 CDTunnel 握手之后那条已经在收发裸 IPv6 包的连接;这里会把
    /// 它拆成读/写两半——读半交给一个后台任务不停地把包塞进 channel(这样
    /// smoltcp 那套"现在有没有包"的轮询式 API 才能非阻塞地问一句"有新包吗"),
    /// 写半留在这个结构体里,发送时直接写。
    pub fn new(stream: S, info: &TunnelInfo) -> Result<Self, TunnelError> {
        let (read_half, write_half) = tokio::io::split(stream);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(reader_task(read_half, tx));

        let client_addr = parse_v6(&info.client_address)?;

        // 真机验证过的坑:CDTunnel 握手协商出来的 `info.mtu`(默认 16000)只是
        // "单次原始 IPv6 包写入这条隧道连接允许多大",不代表底层真实链路真能
        // 稳定送达那么大的包——装大文件(AFC 上传 .ipa)时用 16000 算出的
        // ~15940 字节 TCP 分段,前几个包发出去后设备再也没回过任何 ACK,只有
        // 反复超时重传,最终整个操作失败;把 smoltcp 自己算分段用的 MTU 单独
        // 降到 IPv6 强制要求的最小值 1280(不影响隧道帧本身的读写,那边线上
        // 每个 IPv6 包的长度是自描述的,收发双方各自决定怎么分段跟对方无关),
        // 同样的 100MB 上传就能完整跑完、正常收到 ACK。
        let mut device = SmolDevice::new(1280);
        let config = Config::new(smoltcp::wire::HardwareAddress::Ip);
        let mut iface = Interface::new(config, &mut device, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::Ipv6(Ipv6Cidr::new(client_addr, 64)))
                .expect("fresh address list has room for one entry");
        });

        Ok(Self {
            write_half,
            rx_from_tunnel: rx,
            device,
            iface,
            sockets: SocketSet::new(vec![]),
            next_local_port: 49152, // 随便挑的一个"私有/动态端口"起点,够用就行
        })
    }

    /// 灌一次:把已经到手的隧道包塞进 smoltcp、轮询一次协议栈状态机、把
    /// smoltcp 想发的包写回隧道。不等待新数据——新数据到没到由调用方的循环
    /// 自己决定要不要再等一等。
    async fn pump_once(&mut self) -> Result<(), TunnelError> {
        while let Ok(packet) = self.rx_from_tunnel.try_recv() {
            self.device.rx_queue.push_back(packet);
        }

        self.iface
            .poll(SmolInstant::from_millis(smol_now_ms()), &mut self.device, &mut self.sockets);

        while let Some(packet) = self.device.tx_queue.pop_front() {
            raw_packet::send_packet(&mut self.write_half, &packet).await?;
        }
        Ok(())
    }

    /// 循环泵,直到 `pred` 为真或者超时。`pred` 每次泵完都会被叫一次。
    async fn pump_until(
        &mut self,
        mut pred: impl FnMut(&mut SocketSet<'static>) -> bool,
    ) -> Result<(), TunnelError> {
        let deadline = tokio::time::Instant::now() + PUMP_TIMEOUT;
        loop {
            self.pump_once().await?;
            if pred(&mut self.sockets) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(TunnelError::Timeout);
            }
            // 要么等一小段时间再重试,要么隧道那边先来了新包就立刻醒——两个
            // 都行,取先到的那个,不用死等一个固定间隔。
            tokio::select! {
                _ = tokio::time::sleep(PUMP_IDLE_SLEEP) => {}
                maybe = self.rx_from_tunnel.recv() => {
                    if let Some(packet) = maybe {
                        self.device.rx_queue.push_back(packet);
                    }
                }
            }
        }
    }

    /// 开一条到 `addr:port` 的 TCP 连接,等到三次握手完成(或者超时/被拒)才
    /// 返回。
    pub async fn connect(&mut self, addr: Ipv6Addr, port: u16) -> Result<SocketHandle, TunnelError> {
        let rx_buffer = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER_SIZE]);
        let tx_buffer = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER_SIZE]);
        let socket = tcp::Socket::new(rx_buffer, tx_buffer);
        let handle = self.sockets.add(socket);

        let local_port = self.next_local_port;
        self.next_local_port = self.next_local_port.wrapping_add(1).max(49152);

        let remote = (IpAddress::Ipv6(addr), port);
        {
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            socket
                .connect(self.iface.context(), remote, local_port)
                .map_err(|e| TunnelError::Tcp(format!("connect() rejected: {e}")))?;
        }

        self.pump_until(|sockets| {
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            socket.is_active() || socket.state() == tcp::State::Closed
        })
        .await?;

        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.is_active() {
            return Err(TunnelError::Tcp(format!(
                "connection failed, final state {:?}",
                socket.state()
            )));
        }
        Ok(handle)
    }

    /// 发完 `data` 里所有字节才返回(内部按 smoltcp 发送缓冲区的容量分批发)。
    pub async fn send_all(&mut self, handle: SocketHandle, data: &[u8]) -> Result<(), TunnelError> {
        let mut sent = 0;
        while sent < data.len() {
            self.pump_until(|sockets| sockets.get_mut::<tcp::Socket>(handle).can_send())
                .await?;
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            let n = socket
                .send_slice(&data[sent..])
                .map_err(|e| TunnelError::Tcp(format!("send failed: {e}")))?;
            sent += n;
        }
        // 发送缓冲区里的数据实际上路是 poll 驱动的,不是 send_slice 调用本身——
        // 多泵一次,让刚塞进缓冲区的数据尽快被发出去,不要留到下次别的操作
        // 顺便触发。
        self.pump_once().await?;
        Ok(())
    }

    /// 等到至少有一点数据可读(或者对端关闭/出错)就返回——不是"读满 buf 才
    /// 返回"那种语义,调用方自己决定要不要再调一次攒够想要的量。
    pub async fn recv(&mut self, handle: SocketHandle) -> Result<Vec<u8>, TunnelError> {
        self.pump_until(|sockets| {
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            socket.can_recv() || !socket.may_recv()
        })
        .await?;

        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.may_recv() && !socket.can_recv() {
            tracing::debug!(state = ?socket.state(), "recv: peer closed, no more data");
            return Ok(Vec::new()); // 对端已经关闭,没有更多数据了
        }
        let mut buf = vec![0u8; TCP_BUFFER_SIZE];
        let n = socket
            .recv_slice(&mut buf)
            .map_err(|e| TunnelError::Tcp(format!("recv failed: {e}")))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// 主动关闭:发我们自己这头的 FIN,等四次挥手真正走完(到
    /// `TimeWait`——双方的 FIN 都已经发出并且被对方确认了,这已经是设备
    /// 那边能观察到的"连接彻底关闭"状态)就把 socket 从集合里摘掉。用在
    /// 检测到对端已经半关闭(CLOSE-WAIT)之后——不主动完成关闭握手的话,
    /// 设备那边可能一直认为这条连接还占着,拒绝/晾着后续的新连接。
    ///
    /// 故意不等到 `Closed`:真机测过四次挥手本身几毫秒就能走完,但 smoltcp
    /// 要等 `TimeWait` 自己的计时器(约 10 秒,TCP 规范里防止旧连接延迟包
    /// 干扰新连接用的等待期)才会把状态从 `TimeWait` 自动推进到
    /// `Closed`——握手早就结束了,没必要在这里傻等那个计时器,每次
    /// `close()` 平白多花 10 秒。
    pub async fn close(&mut self, handle: SocketHandle) -> Result<(), TunnelError> {
        {
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            socket.close();
        }
        let result = self
            .pump_until(|sockets| {
                let state = sockets.get_mut::<tcp::Socket>(handle).state();
                matches!(state, tcp::State::TimeWait | tcp::State::Closed)
            })
            .await;
        self.sockets.remove(handle);
        result
    }
}

async fn reader_task<R: AsyncRead + Unpin>(mut read_half: R, tx: mpsc::UnboundedSender<Vec<u8>>) {
    loop {
        match raw_packet::recv_packet(&mut read_half).await {
            Ok(packet) => {
                if tx.send(packet).is_err() {
                    return; // TunnelStack 已经被丢弃,没人要这些包了
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "tunnel reader task exiting");
                return;
            }
        }
    }
}

fn parse_v6(s: &str) -> Result<Ipv6Address, TunnelError> {
    let addr = Ipv6Addr::from_str(s)
        .map_err(|e| TunnelError::BadHandshake(format!("invalid IPv6 address {s:?}: {e}")))?;
    Ok(addr)
}

fn smol_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
