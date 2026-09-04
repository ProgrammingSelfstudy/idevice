//! RSD(Remote Service Discovery)——iOS 17+ 的 Instruments 连接入口。走的是
//! "XPC 消息包在 HTTP/2 DATA 帧里"这条协议:先建隧道(见 `tunnel` crate)、
//! TCP 连到隧道给的 RSD 端口,然后这一层负责 HTTP/2 连接前言 + XPC 握手,
//! 最后从设备那边问回一份"当前有哪些服务、各自在哪个端口"的清单。

mod http2;
mod http2_frame;
mod shim;
mod xpc_format;

use std::collections::HashMap;
use std::net::Ipv6Addr;

use tokio::io::{AsyncRead, AsyncWrite};
use tunnel::TunnelStack;
use xpc_format::{flags, Dictionary, XpcMessage, XpcObject};

pub use shim::ShimServiceConnection;

const ROOT_CHANNEL: u32 = 1;
const REPLY_CHANNEL: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum RsdError {
    #[error(transparent)]
    Tunnel(#[from] tunnel::TunnelError),
    #[error("HTTP/2 peer reset the stream")]
    StreamReset,
    #[error("HTTP/2 peer sent GOAWAY: {0}")]
    GoAway(String),
    #[error("malformed HTTP/2 frame: {0}")]
    MalformedFrame(String),
    #[error("malformed XPC object: {0}")]
    MalformedXpcObject(String),
    #[error("connection closed before a full message arrived")]
    ConnectionClosed,
    #[error("RSD handshake response missing expected field: {0}")]
    MissingField(&'static str),
    #[error("shim service reported a StartService error: {0}")]
    ShimStartServiceError(String),
}

pub type Result<T> = std::result::Result<T, RsdError>;

#[derive(Debug, Clone)]
pub struct RsdServiceInfo {
    pub port: u16,
    pub entitlement: String,
}

#[derive(Debug, Clone)]
pub struct RsdInfo {
    pub services: HashMap<String, RsdServiceInfo>,
    pub protocol_version: i64,
    pub uuid: String,
}

impl RsdInfo {
    pub fn service_port(&self, name: &str) -> Option<u16> {
        self.services.get(name).map(|s| s.port)
    }
}

/// 走完 RSD 握手,拿到服务清单。`handle` 是已经连到隧道 RSD 端口的 TCP 连接
/// (`TunnelStack::connect` 的返回值)。
pub async fn discover<S>(stack: &mut TunnelStack<S>, handle: tunnel::SocketHandle) -> Result<RsdInfo>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut h2 = http2::Http2Client::new(stack, handle).await?;
    let mut partial: HashMap<u32, Vec<u8>> = HashMap::new();
    let root_id: u64 = 1;

    // --- do_handshake:跟 idevice 的 DTXConnection._perform_handshake 一样的
    // 顺序,这几步的具体含义(除了明显是"开两条流、发个空字典占位"之外)细节
    // 苹果没公开文档,照抄这个顺序实测是通的。
    h2.set_settings(
        vec![
            http2_frame::Setting::MaxConcurrentStreams(100),
            http2_frame::Setting::InitialWindowSize(1_048_576),
        ],
        0,
    )
    .await?;
    h2.window_update(983_041, 0).await?;
    h2.open_stream(ROOT_CHANNEL).await?;

    send_xpc(&mut h2, ROOT_CHANNEL, flags::ALWAYS_SET, Some(XpcObject::Dictionary(Dictionary::new())), root_id).await?;

    h2.open_stream(REPLY_CHANNEL).await?;
    send_xpc(&mut h2, REPLY_CHANNEL, flags::INIT_HANDSHAKE | flags::ALWAYS_SET, None, root_id).await?;

    send_xpc(&mut h2, ROOT_CHANNEL, 0x201, None, root_id).await?;

    // --- send_device_handshake:告诉设备"我是个现代 RemoteXPC 客户端"。
    const REMOTE_XPC_VERSION_FLAGS: u64 = 0x0100_0000_0000_0006;
    let handshake = XpcObject::dict([
        ("MessageType", XpcObject::String("Handshake".into())),
        ("MessagingProtocolVersion", XpcObject::UInt64(7)),
        ("UUID", XpcObject::Uuid(uuid::Uuid::new_v4())),
        (
            "Properties",
            XpcObject::dict([
                ("RemoteXPCVersionFlags", XpcObject::UInt64(REMOTE_XPC_VERSION_FLAGS)),
                ("SensitivePropertiesVisible", XpcObject::Bool(true)),
            ]),
        ),
        ("Services", XpcObject::Dictionary(Dictionary::new())),
    ]);
    send_xpc(&mut h2, ROOT_CHANNEL, flags::DATA_FLAG | flags::ALWAYS_SET, Some(handshake), root_id).await?;

    // --- recv_root:读 ROOT_CHANNEL 直到收到一个非空字典(设备偶尔会先推空字典
    // 当心跳/占位,跳过)。
    let response = loop {
        let msg = recv_xpc_message(&mut h2, ROOT_CHANNEL, &mut partial).await?;
        let Some(object) = msg.object else { continue };
        let Some(dict) = object.as_dictionary() else { continue };
        if dict.is_empty() {
            continue;
        }
        tracing::debug!(keys = ?dict.keys().collect::<Vec<_>>(), "RSD non-empty root dict");
        break object;
    };

    parse_handshake_response(&response)
}

async fn send_xpc<S>(
    h2: &mut http2::Http2Client<'_, S>,
    channel: u32,
    flags: u32,
    object: Option<XpcObject>,
    message_id: u64,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let bytes = XpcMessage::new(flags, object, message_id).encode();
    h2.send(&bytes, channel).await?;
    Ok(())
}

async fn recv_xpc_message<S>(
    h2: &mut http2::Http2Client<'_, S>,
    channel: u32,
    partial: &mut HashMap<u32, Vec<u8>>,
) -> Result<XpcMessage>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        let decoded = {
            let buf = partial.entry(channel).or_default();
            match XpcMessage::parse(buf) {
                Ok(Some((msg, consumed))) => {
                    buf.drain(..consumed);
                    Some(msg)
                }
                Ok(None) => None,
                Err(e) => return Err(e),
            }
        };
        if let Some(msg) = decoded {
            return Ok(msg);
        }
        let chunk = h2.read(channel).await?;
        partial.entry(channel).or_default().extend(chunk);
    }
}

fn parse_handshake_response(response: &XpcObject) -> Result<RsdInfo> {
    let dict = response
        .as_dictionary()
        .ok_or(RsdError::MissingField("<response is not a dictionary>"))?;

    let services_dict = dict
        .get("Services")
        .and_then(|v| v.as_dictionary())
        .ok_or(RsdError::MissingField("Services"))?;

    let mut services = HashMap::with_capacity(services_dict.len());
    for (name, entry) in services_dict {
        let Some(entry) = entry.as_dictionary() else { continue };
        let Some(entitlement) = entry.get("Entitlement").and_then(|v| v.as_string()) else {
            continue;
        };
        let Some(port) = entry
            .get("Port")
            .and_then(|v| v.as_string())
            .and_then(|s| s.parse::<u16>().ok())
        else {
            continue;
        };
        services.insert(
            name.clone(),
            RsdServiceInfo {
                port,
                entitlement: entitlement.to_string(),
            },
        );
    }

    let protocol_version = dict
        .get("MessagingProtocolVersion")
        .and_then(|v| v.as_i64())
        .ok_or(RsdError::MissingField("MessagingProtocolVersion"))?;
    // 设备回的 UUID 是原生 XPC UUID 类型(16 字节),不是字符串——跟我们自己
    // 发请求时用的类型一样,但一开始漏想了"读回来的也可能是这个类型",错
    // 当成字符串解析直接找不到字段。两种都认,保险一点。
    let uuid = match dict.get("UUID") {
        Some(XpcObject::Uuid(u)) => u.to_string(),
        Some(XpcObject::String(s)) => s.clone(),
        _ => return Err(RsdError::MissingField("UUID")),
    };

    Ok(RsdInfo {
        services,
        protocol_version,
        uuid,
    })
}

/// 方便调用方直接传 `server_address`/`server_rsd_port`(来自
/// `tunnel::TunnelInfo`)而不用自己解析 IP、自己调 `TunnelStack::connect`。
pub async fn connect_and_discover<S>(
    stack: &mut TunnelStack<S>,
    server_address: &Ipv6Addr,
    rsd_port: u16,
) -> Result<RsdInfo>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let handle = stack.connect(*server_address, rsd_port).await?;
    discover(stack, handle).await
}
