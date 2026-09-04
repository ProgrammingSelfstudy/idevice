//! 纯手写的 lockdownd 客户端——iOS 设备管理/服务发现的入口服务,固定端口
//! `62078`,靠 usbmuxd 转发过去(见 `usbmuxd::UsbmuxdClient::connect_to_device`)。
//!
//! `QueryType`/`GetValue` 明文就能查(阶段2 已验证);`StartSession` 之后要把
//! 连接升级成 TLS 才能拿到更完整的字段/后续服务(阶段3,见 `tls.rs`)——升级
//! 前后底层 stream 的具体类型不一样(`UnixStream` vs `TlsStream<UnixStream>`),
//! 所以内部用 `Box<dyn Transport>` 屏蔽这个差异,升级只是换掉这个 trait object,
//! 上层 `get_value`/`query_type` 这些方法完全不用感知连接是不是加密的。

mod frame;
mod tls;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use usbmuxd::UsbmuxdClient;

pub const LOCKDOWND_PORT: u16 = 62078;

/// 屏蔽"明文 UnixStream"和"TLS 包过的流"之间的类型差异。
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

#[derive(Debug, thiserror::Error)]
pub enum LockdownError {
    #[error(transparent)]
    Usbmuxd(#[from] usbmuxd::UsbmuxdError),
    #[error(transparent)]
    Pairing(#[from] pairing::PairingError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to encode request plist: {0}")]
    PlistEncode(#[source] plist::Error),
    #[error("failed to decode response plist: {0}")]
    PlistDecode(#[source] plist::Error),
    #[error("malformed lockdown frame: {0}")]
    MalformedFrame(String),
    #[error("unexpected lockdown response: {0}")]
    UnexpectedResponse(String),
    #[error("device reported an error: {0}")]
    DeviceError(String),
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

pub type Result<T> = std::result::Result<T, LockdownError>;

pub struct LockdownClient {
    stream: Box<dyn Transport>,
    /// lockdownd 请求里都要带一个 "Label" 字段(自我标识,任意字符串,设备不校验
    /// 内容),真机测过给什么都能接受——就叫自己的名字方便设备端日志排查。
    label: String,
}

impl LockdownClient {
    /// 通过 usbmuxd 转发,连到 `device_id` 这台设备的 lockdownd。
    ///
    /// usbmuxd 的 `Connect` 命令会把传进去的那条连接本身消费掉(转发之后那条
    /// 连接就变成跟设备的直连管道了,不能再当 usbmuxd 协议用),所以这里内部
    /// 专门开一条新的 usbmuxd 连接来做这次转发——调用方手上如果还留着另一条
    /// `UsbmuxdClient`(比如刚拿它列过设备),那条不受影响,可以接着用。
    pub async fn connect(device_id: u32) -> Result<Self> {
        let usbmuxd = UsbmuxdClient::connect().await?;
        let stream: UnixStream = usbmuxd.connect_to_device(device_id, LOCKDOWND_PORT).await?;
        Ok(Self {
            stream: Box::new(stream),
            label: "idevice-rs-native".to_string(),
        })
    }

    /// 走 `StartSession` + TLS 升级——升级之后这条连接才能查到配对相关的完整
    /// 字段、才能 `StartService` 拿到需要认证的服务端口。`pairing_file` 一般
    /// 从 `usbmuxd::UsbmuxdClient::read_pair_record` 读出来的字节
    /// `pairing::PairingFile::from_bytes` 解出来。
    pub async fn start_session(&mut self, pairing_file: &pairing::PairingFile) -> Result<()> {
        let mut req = plist::Dictionary::new();
        req.insert("Label".into(), self.label.clone().into());
        req.insert("Request".into(), "StartSession".into());
        req.insert("HostID".into(), pairing_file.host_id.clone().into());
        req.insert("SystemBUID".into(), pairing_file.system_buid.clone().into());
        frame::write_plist(&mut self.stream, req).await?;

        let res = frame::read_plist(&mut self.stream).await?;
        match res.get("EnableSessionSSL") {
            Some(plist::Value::Boolean(true)) => {}
            _ => {
                return Err(LockdownError::UnexpectedResponse(
                    "StartSession response missing EnableSessionSSL=true".into(),
                ));
            }
        }

        let config = tls::build_client_config(pairing_file)?;
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        // 服务端证书反正不校验(见 tls.rs),这个 "Device" 只是 rustls API 要求
        // 必须传一个 ServerName,内容本身不影响握手结果。
        let server_name = rustls_pki_types::ServerName::try_from("Device")
            .expect("\"Device\" 是合法的 DNS-like ServerName 字面量");

        // 把当前的 Box<dyn Transport> 换成 TLS 包过的版本——中间用一个占位的
        // no-op stream 顶一下,不然没法把 `self.stream` "拿出来"再放回去
        // (Box<dyn Trait> 不能直接 move 出 &mut self 后面的字段)。
        let plain = std::mem::replace(&mut self.stream, Box::new(tokio::io::empty()));
        let tls_stream = connector
            .connect(server_name, plain)
            .await
            .map_err(|e| LockdownError::Tls(format!("TLS handshake failed: {e}")))?;
        self.stream = Box::new(tls_stream);

        Ok(())
    }

    /// `key`/`domain` 都传 `None` 会拿到整棵设备信息树。不需要配对——lockdownd
    /// 对一部分基础字段(`ProductVersion`/`ProductType`/`DeviceName` 这些)允许
    /// 未配对连接直接查,这是真机验证过的行为,不是猜的。
    pub async fn get_value(&mut self, key: Option<&str>, domain: Option<&str>) -> Result<plist::Value> {
        let mut req = plist::Dictionary::new();
        req.insert("Label".into(), self.label.clone().into());
        req.insert("Request".into(), "GetValue".into());
        if let Some(key) = key {
            req.insert("Key".into(), key.into());
        }
        if let Some(domain) = domain {
            req.insert("Domain".into(), domain.into());
        }
        frame::write_plist(&mut self.stream, req).await?;

        let mut res = frame::read_plist(&mut self.stream).await?;
        res.remove("Value")
            .ok_or_else(|| LockdownError::UnexpectedResponse("missing Value".into()))
    }

    pub async fn query_type(&mut self) -> Result<String> {
        let mut req = plist::Dictionary::new();
        req.insert("Request".into(), "QueryType".into());
        frame::write_plist(&mut self.stream, req).await?;

        let mut res = frame::read_plist(&mut self.stream).await?;
        match res.remove("Type") {
            Some(plist::Value::String(s)) => Ok(s),
            _ => Err(LockdownError::UnexpectedResponse("missing Type".into())),
        }
    }
}
