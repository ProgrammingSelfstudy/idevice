//! `CoreDeviceProxy` 隧道——iOS 17+ 给"受信任服务"(Instruments 在内)开的 L3
//! 隧道入口。走的还是 USB/lockdownd 这条老路(不需要 `remote_pairing` 那套
//! 无线配对),`StartService` 问到端口之后就是标准的 CDTunnel 握手。
//!
//! 这层只管"connect + 握手 + 收发裸 IPv6 包",往上接真正的 TCP/IP 协议栈
//! (smoltcp)是下一步,还没做。

mod cdtunnel;
mod raw_packet;

pub use cdtunnel::TunnelInfo;
pub use lockdownd::Transport;

use pairing::PairingFile;

pub const CORE_DEVICE_PROXY_SERVICE: &str = "com.apple.internal.devicecompute.CoreDeviceProxy";

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error(transparent)]
    Usbmuxd(#[from] usbmuxd::UsbmuxdError),
    #[error(transparent)]
    Lockdown(#[from] lockdownd::LockdownError),
    #[error(transparent)]
    Pairing(#[from] pairing::PairingError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("CDTunnel handshake failed: {0}")]
    BadHandshake(String),
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

pub type Result<T> = std::result::Result<T, TunnelError>;

/// 建到 `CoreDeviceProxy` 的裸 IPv6 隧道——返回握手信息(隧道两端地址/RSD 端口)
/// 和一条已经完成 CDTunnel 握手、可以直接收发裸 IPv6 包的连接。
///
/// 要不要在这条连接上再包一层 TLS 是 `StartService` 响应里 `EnableServiceSSL`
/// 说了算,不是固定的——这次重写实测中这台设备(iOS 26.6.1)这里是 `true`,
/// 跟 `idevice` 注释里"USB 场景通常不需要"的说法不一样,两条路径都留着。
pub async fn connect(
    device_id: u32,
    pairing_file: &PairingFile,
) -> Result<(TunnelInfo, Box<dyn Transport>)> {
    let mut lockdown = lockdownd::LockdownClient::connect(device_id).await?;
    lockdown.start_session(pairing_file).await?;
    let (port, ssl) = lockdown.start_service(CORE_DEVICE_PROXY_SERVICE).await?;
    tracing::debug!(port, ssl, "CoreDeviceProxy StartService");

    // 走一条新的 usbmuxd 连接转发到这个端口——跟 lockdownd 本身是完全独立的
    // 两条连接,道理跟 `lockdownd::LockdownClient::connect` 内部另开一条
    // usbmuxd 连接是一样的。
    let usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let raw_stream = usbmuxd.connect_to_device(device_id, port).await?;

    let mut stream: Box<dyn Transport> = if ssl {
        let config = pairing::tls::build_client_config(pairing_file)?;
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let server_name = rustls_pki_types::ServerName::try_from("Device")
            .expect("\"Device\" 是合法的 DNS-like ServerName 字面量");
        let tls_stream = connector
            .connect(server_name, raw_stream)
            .await
            .map_err(|e| TunnelError::Tls(format!("TLS handshake failed: {e}")))?;
        Box::new(tls_stream)
    } else {
        Box::new(raw_stream)
    };

    let info = cdtunnel::handshake(&mut stream).await?;
    Ok((info, stream))
}

pub use raw_packet::{recv_packet, send_packet};
