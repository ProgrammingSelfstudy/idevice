//! 纯手写的 usbmuxd 客户端——连本机 usbmuxd,列设备、读/存配对记录、转发到
//! 设备上某个端口。不依赖 `idevice`/`libimobiledevice`,协议细节见 `frame.rs`
//! 顶部注释。
//!
//! usbmuxd 本身的传输层两个平台不一样:macOS/Linux 是 Unix socket
//! (`/var/run/usbmuxd`);Windows 上 Apple Mobile Device Service 走的是本机
//! TCP 端口(libusbmuxd/pymobiledevice3 在 Windows 上都是连这个端口,不是
//! 猜的)。`MuxStream` 按平台选一个具体类型,两边都实现 `AsyncRead +
//! AsyncWrite`,`frame.rs` 的编解码代码完全不用关心跑在哪个传输上面。
//!
//! 这条 Windows 路径还没有真机(或者至少真的跑在 Windows 上的 Apple Mobile
//! Device Service)验证过——端口号、连接细节都是照 libusbmuxd/
//! pymobiledevice3 的实现抄的,不是这次重写自己测出来的,跟 usbmuxd 协议帧
//! 本身（真机验证过)不是一个可信度级别。
//!
//! 目前只处理 USB 直连场景(这次重写的真机测试环境就是 USB 接的),网络配对的
//! 设备地址解析(macOS sockaddr 变长编码)先不做,遇到就归到 `Connection::Unknown`。

mod frame;

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use tokio::net::UnixStream as MuxStream;
#[cfg(windows)]
use tokio::net::TcpStream as MuxStream;

#[cfg(unix)]
const DEFAULT_SOCKET_PATH: &str = "/var/run/usbmuxd";
/// Apple Mobile Device Service 在 Windows 上监听的本机 TCP 端口——
/// libusbmuxd(`socket.c` 的 WIN32 分支)和 pymobiledevice3 的 Windows 传输
/// 都是连这个端口,数字本身不是随便挑的。
#[cfg(windows)]
const DEFAULT_TCP_PORT: u16 = 27015;

#[derive(Debug, thiserror::Error)]
pub enum UsbmuxdError {
    #[error("failed to connect to usbmuxd at {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to encode request plist: {0}")]
    PlistEncode(#[source] plist::Error),
    #[error("failed to decode response plist: {0}")]
    PlistDecode(#[source] plist::Error),
    #[error("malformed usbmuxd frame: {0}")]
    MalformedFrame(String),
    #[error("unexpected usbmuxd response: {0}")]
    UnexpectedResponse(String),
    #[error("device {0} not found")]
    DeviceNotFound(String),
    #[error("usbmuxd connect failed: bad command")]
    BadCommand,
    #[error("usbmuxd connect failed: bad device")]
    BadDevice,
    #[error("usbmuxd connect failed: connection refused by device")]
    ConnectionRefused,
    #[error("usbmuxd connect failed: bad usbmux protocol version")]
    BadVersion,
}

pub type Result<T> = std::result::Result<T, UsbmuxdError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Usb,
    /// 网络配对设备——地址解析还没做,先只留一个占位。
    Network,
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct Device {
    pub udid: String,
    pub device_id: u32,
    pub connection: Connection,
}

pub struct UsbmuxdClient {
    stream: MuxStream,
}

impl UsbmuxdClient {
    #[cfg(unix)]
    pub async fn connect() -> Result<Self> {
        Self::connect_to(DEFAULT_SOCKET_PATH).await
    }

    #[cfg(windows)]
    pub async fn connect() -> Result<Self> {
        Self::connect_to_port(DEFAULT_TCP_PORT).await
    }

    #[cfg(unix)]
    pub async fn connect_to(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let stream =
            MuxStream::connect(path)
                .await
                .map_err(|source| UsbmuxdError::Connect {
                    path: path.display().to_string(),
                    source,
                })?;
        Ok(Self { stream })
    }

    #[cfg(windows)]
    pub async fn connect_to_port(port: u16) -> Result<Self> {
        let addr = format!("127.0.0.1:{port}");
        let stream = MuxStream::connect(&addr)
            .await
            .map_err(|source| UsbmuxdError::Connect { path: addr, source })?;
        // usbmuxd 的帧协议是同步收发的短消息(发一条等一条回包),TCP_NODELAY
        // 关掉 Nagle 合并小包的延迟,行为上更接近 Unix socket 那条路径。
        stream.set_nodelay(true).ok();
        Ok(Self { stream })
    }

    pub async fn list_devices(&mut self) -> Result<Vec<Device>> {
        let mut req = plist::Dictionary::new();
        req.insert("MessageType".into(), "ListDevices".into());
        req.insert("ClientVersionString".into(), "idevice-rs-native".into());
        req.insert("kLibUSBMuxVersion".into(), 3.into());
        frame::write_plist(&mut self.stream, req).await?;

        let res = frame::read_plist(&mut self.stream).await?;
        let raw_list = res
            .get("DeviceList")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                UsbmuxdError::UnexpectedResponse("missing DeviceList array".into())
            })?;

        let mut devices = Vec::with_capacity(raw_list.len());
        for entry in raw_list {
            let entry = entry.as_dictionary().ok_or_else(|| {
                UsbmuxdError::UnexpectedResponse("DeviceList entry is not a dictionary".into())
            })?;
            let device_id = entry
                .get("DeviceID")
                .and_then(|v| v.as_unsigned_integer())
                .ok_or_else(|| UsbmuxdError::UnexpectedResponse("missing DeviceID".into()))?
                as u32;
            let properties = entry
                .get("Properties")
                .and_then(|v| v.as_dictionary())
                .ok_or_else(|| UsbmuxdError::UnexpectedResponse("missing Properties".into()))?;
            let udid = properties
                .get("SerialNumber")
                .and_then(|v| v.as_string())
                .ok_or_else(|| {
                    UsbmuxdError::UnexpectedResponse("missing Properties.SerialNumber".into())
                })?
                .to_string();
            let connection_type = properties
                .get("ConnectionType")
                .and_then(|v| v.as_string())
                .unwrap_or("");
            let connection = match connection_type {
                "USB" => Connection::Usb,
                "Network" => Connection::Network,
                other => Connection::Unknown(other.to_string()),
            };

            devices.push(Device {
                udid,
                device_id,
                connection,
            });
        }
        Ok(devices)
    }

    /// 读 usbmuxd 缓存的配对记录原始 plist 字节——不解析成结构化类型,那是
    /// `lockdownd`/`pairing` crate 的事,这层只管取字节。
    pub async fn read_pair_record(&mut self, udid: &str) -> Result<Vec<u8>> {
        let mut req = plist::Dictionary::new();
        req.insert("MessageType".into(), "ReadPairRecord".into());
        req.insert("PairRecordID".into(), udid.into());
        frame::write_plist(&mut self.stream, req).await?;

        let res = frame::read_plist(&mut self.stream).await?;
        match res.get("PairRecordData") {
            Some(plist::Value::Data(d)) => Ok(d.clone()),
            _ => Err(UsbmuxdError::UnexpectedResponse(
                "missing PairRecordData".into(),
            )),
        }
    }

    pub async fn save_pair_record(&mut self, udid: &str, record: Vec<u8>) -> Result<()> {
        let mut req = plist::Dictionary::new();
        req.insert("MessageType".into(), "SavePairRecord".into());
        req.insert("PairRecordID".into(), udid.into());
        req.insert("PairRecordData".into(), plist::Value::Data(record));
        frame::write_plist(&mut self.stream, req).await?;
        // usbmuxd 对 SavePairRecord 也回一个 Result 消息,这里不细分错误码,
        // 只要收到回包不报 IO 错就算数——错误码细分留给真机测出问题再补。
        frame::read_plist(&mut self.stream).await?;
        Ok(())
    }

    pub async fn read_buid(&mut self) -> Result<String> {
        let mut req = plist::Dictionary::new();
        req.insert("MessageType".into(), "ReadBUID".into());
        frame::write_plist(&mut self.stream, req).await?;

        let res = frame::read_plist(&mut self.stream).await?;
        match res.get("BUID") {
            Some(plist::Value::String(s)) => Ok(s.clone()),
            _ => Err(UsbmuxdError::UnexpectedResponse("missing BUID".into())),
        }
    }

    /// 让 usbmuxd 把这条连接转发到设备上的某个端口——转发一旦建立,这条底层
    /// 连接(macOS/Linux 是 Unix socket,Windows 是 TCP)后续收发的字节就是
    /// 直接跟设备对话(不再是 usbmuxd 协议本身),所以这个方法消费掉 `self`,
    /// 把底层 stream 整个交出去给上一层(lockdownd)继续用。
    ///
    /// `port` 传主机字节序——usbmuxd 协议本身要大端,这里在内部转,调用方不用
    /// 关心字节序这个坑(之前读 `idevice` 源码时确认过这一步很容易漏,`PortNumber`
    /// 字段要求的是网络字节序,不是常见的"这是个整数字段随便传")。
    pub async fn connect_to_device(
        mut self,
        device_id: u32,
        port: u16,
    ) -> Result<MuxStream> {
        let port_be = port.to_be();

        let mut req = plist::Dictionary::new();
        req.insert("MessageType".into(), "Connect".into());
        req.insert("DeviceID".into(), (device_id as i64).into());
        req.insert("PortNumber".into(), (port_be as i64).into());
        frame::write_plist(&mut self.stream, req).await?;

        let res = frame::read_plist(&mut self.stream).await?;
        match res.get("Number").and_then(|v| v.as_unsigned_integer()) {
            Some(0) => Ok(self.stream),
            Some(1) => Err(UsbmuxdError::BadCommand),
            Some(2) => Err(UsbmuxdError::BadDevice),
            Some(3) => Err(UsbmuxdError::ConnectionRefused),
            Some(6) => Err(UsbmuxdError::BadVersion),
            _ => Err(UsbmuxdError::UnexpectedResponse(
                "missing/unknown Number field in Connect response".into(),
            )),
        }
    }
}
