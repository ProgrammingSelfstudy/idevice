//! 纯手写的 lockdownd 客户端——iOS 设备管理/服务发现的入口服务,固定端口
//! `62078`,靠 usbmuxd 转发过去(见 `usbmuxd::UsbmuxdClient::connect_to_device`)。
//!
//! 这一阶段只做明文能查到的东西(`QueryType`/`GetValue`)——`StartSession`
//! 需要配对证书把连接升级成 TLS,那是阶段 3(配对)之后才补的。

mod frame;

use tokio::net::UnixStream;
use usbmuxd::UsbmuxdClient;

pub const LOCKDOWND_PORT: u16 = 62078;

#[derive(Debug, thiserror::Error)]
pub enum LockdownError {
    #[error(transparent)]
    Usbmuxd(#[from] usbmuxd::UsbmuxdError),
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
}

pub type Result<T> = std::result::Result<T, LockdownError>;

pub struct LockdownClient {
    stream: UnixStream,
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
        let stream = usbmuxd.connect_to_device(device_id, LOCKDOWND_PORT).await?;
        Ok(Self {
            stream,
            label: "idevice-rs-native".to_string(),
        })
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
