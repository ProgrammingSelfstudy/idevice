//! 经典 lockdown 服务原样代理到隧道上的那批 `*.shim.remote` 服务的公共连接
//! 逻辑——`com.apple.mobile.installation_proxy.shim.remote`、
//! `com.apple.mobile.mobile_image_mounter.shim.remote`、将来的 AFC 之类都是
//! 这个模式:先按 lockdownd 那套 `[4B 大端 body_len][XML plist]` 帧格式连上去,
//! 但连上端口之后不能直接发业务命令——必须先做一次 `RSDCheckin` 握手(发
//! `{Label, ProtocolVersion:"2", Request:"RSDCheckin"}`,收一条 echo,再收一条
//! 设备主动推的 `{Request:"StartService"}` 确认),之后这条连接才算真正
//! "启动"。最初在 `mobile_image_mounter` 上验证通的,抽出来给其他 shim 服务
//! 复用,不然每个服务各自都要抄一遍这段握手 + 帧收发。

use tokio::io::{AsyncRead, AsyncWrite};
use tunnel::{SocketHandle, TunnelStack};

use crate::RsdError;

/// 一条已经完成 `RSDCheckin` 握手、可以直接发业务 plist 命令的 shim 服务连接。
pub struct ShimServiceConnection<'a, S> {
    stack: &'a mut TunnelStack<S>,
    handle: SocketHandle,
    /// 跨调用累积的接收缓冲区——两条消息可能背靠背挤在同一个 TCP 段里一起
    /// 到达,只消费当前这条的字节,剩下的必须留给下一次调用(`image_mounter`
    /// 那次调试踩过"每次读都开新局部 buf 导致悄悄丢字节"的坑)。
    recv_buf: Vec<u8>,
}

impl<'a, S> ShimServiceConnection<'a, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// 连到 `port`(从 `RsdInfo::service_port` 查来的)、完成 `RSDCheckin` 握手。
    pub async fn connect(
        stack: &'a mut TunnelStack<S>,
        server_addr: std::net::Ipv6Addr,
        port: u16,
        label: &str,
    ) -> Result<Self, RsdError> {
        let handle = stack.connect(server_addr, port).await?;
        let mut conn = Self {
            stack,
            handle,
            recv_buf: Vec::new(),
        };

        let mut checkin = plist::Dictionary::new();
        checkin.insert("Label".into(), label.into());
        checkin.insert("ProtocolVersion".into(), "2".into());
        checkin.insert("Request".into(), "RSDCheckin".into());
        conn.send_plist(checkin).await?;

        let checkin_reply = conn.recv_plist().await?;
        if checkin_reply.get("Request").and_then(|v| v.as_string()) != Some("RSDCheckin") {
            return Err(RsdError::MissingField("Request=RSDCheckin"));
        }

        let start_service_reply = conn.recv_plist().await?;
        if start_service_reply.get("Request").and_then(|v| v.as_string()) != Some("StartService") {
            return Err(RsdError::MissingField("Request=StartService"));
        }
        if let Some(err) = start_service_reply.get("Error") {
            return Err(RsdError::ShimStartServiceError(format!("{err:?}")));
        }

        Ok(conn)
    }

    pub async fn send_plist(&mut self, dict: plist::Dictionary) -> Result<(), RsdError> {
        let mut body = Vec::new();
        plist::to_writer_xml(&mut body, &plist::Value::Dictionary(dict))
            .map_err(|e| RsdError::MalformedXpcObject(format!("failed to encode plist: {e}")))?;
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend(body);
        self.stack.send_all(self.handle, &out).await?;
        Ok(())
    }

    pub async fn recv_plist(&mut self) -> Result<plist::Dictionary, RsdError> {
        while self.recv_buf.len() < 4 {
            let chunk = self.stack.recv(self.handle).await?;
            if chunk.is_empty() {
                return Err(RsdError::ConnectionClosed);
            }
            self.recv_buf.extend(chunk);
        }
        let len = u32::from_be_bytes(self.recv_buf[0..4].try_into().unwrap()) as usize;
        while self.recv_buf.len() < 4 + len {
            let chunk = self.stack.recv(self.handle).await?;
            if chunk.is_empty() {
                return Err(RsdError::ConnectionClosed);
            }
            self.recv_buf.extend(chunk);
        }
        let value: plist::Value = plist::from_bytes(&self.recv_buf[4..4 + len])
            .map_err(|e| RsdError::MalformedXpcObject(format!("failed to decode plist: {e}")))?;
        self.recv_buf.drain(..4 + len);
        value
            .into_dictionary()
            .ok_or_else(|| RsdError::MalformedXpcObject("response body is not a dictionary".into()))
    }

    /// 关闭底层 TCP 连接(发我们自己的 FIN,完成关闭握手)。
    pub async fn close(self) -> Result<(), RsdError> {
        self.stack.close(self.handle).await?;
        Ok(())
    }

    /// 握手做完之后,有些服务(比如 AFC)后续走的不是 plist 帧,而是自己的
    /// 二进制协议——把底层 `stack`/`handle` 连同"握手阶段可能顺手多收到的、
    /// 还没消费的字节"一起交还给调用方,让它换一套协议继续用同一条连接。
    pub fn into_raw(self) -> (&'a mut TunnelStack<S>, SocketHandle, Vec<u8>) {
        (self.stack, self.handle, self.recv_buf)
    }
}
