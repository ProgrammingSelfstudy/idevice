//! CDTunnel 握手——`CoreDeviceProxy` 服务(iOS 17+ 才有,给 Instruments 之类的
//! "受信任服务"建 L3 隧道用)连上之后先做的第一步。协议本身跟设备的具体功能
//! 无关,纯粹是"跟设备商量好隧道两端各自的 IPv6 地址、MTU、RSD 服务发现端口"。
//!
//! ```text
//! [8B magic "CDTunnel"][2B 大端 body_len][JSON body]
//! ```
//! 请求/响应都是这个格式。请求体是 `{"type":"clientHandshakeRequest","mtu":N}`,
//! 响应体里关心的字段是 `clientParameters.address`(隧道我方地址)、
//! `serverAddress`(隧道设备侧地址)、`serverRSDPort`(RSD 服务发现端口)。
//!
//! 握手完成之后,同一条连接上收发的就不再是这套 magic+JSON 帧了,而是裸的
//! IPv6 包(见 `raw_packet.rs`)。

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::TunnelError;

const MAGIC: &[u8] = b"CDTunnel";
const DEFAULT_MTU: u16 = 16000;

#[derive(Debug, Clone)]
pub struct TunnelInfo {
    pub client_address: String,
    pub server_address: String,
    pub mtu: u16,
    pub server_rsd_port: u16,
}

#[derive(Deserialize)]
struct HandshakeResponse {
    #[serde(rename = "clientParameters")]
    client_parameters: ClientParameters,
    #[serde(rename = "serverAddress")]
    server_address: String,
    #[serde(rename = "serverRSDPort")]
    server_rsd_port: u16,
}

#[derive(Deserialize)]
struct ClientParameters {
    address: String,
    mtu: Option<u16>,
}

pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<TunnelInfo, TunnelError> {
    let request = serde_json::json!({
        "type": "clientHandshakeRequest",
        "mtu": DEFAULT_MTU,
    });
    let body = serde_json::to_vec(&request)?;

    stream.write_all(MAGIC).await?;
    stream.write_all(&(body.len() as u16).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;

    let mut magic_buf = [0u8; 8];
    stream.read_exact(&mut magic_buf).await?;
    if magic_buf != MAGIC {
        return Err(TunnelError::BadHandshake("response magic mismatch".into()));
    }

    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;

    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;

    let response: HandshakeResponse = serde_json::from_slice(&body)?;

    Ok(TunnelInfo {
        client_address: response.client_parameters.address,
        server_address: response.server_address,
        mtu: response.client_parameters.mtu.unwrap_or(DEFAULT_MTU),
        server_rsd_port: response.server_rsd_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn handshake_parses_a_well_formed_response() {
        let (mut client, mut server) = duplex(4096);

        let server_task = tokio::spawn(async move {
            let mut magic = [0u8; 8];
            server.read_exact(&mut magic).await.unwrap();
            assert_eq!(&magic, MAGIC);
            let mut len_buf = [0u8; 2];
            server.read_exact(&mut len_buf).await.unwrap();
            let len = u16::from_be_bytes(len_buf) as usize;
            let mut body = vec![0u8; len];
            server.read_exact(&mut body).await.unwrap();
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(req["type"], "clientHandshakeRequest");

            let resp = serde_json::json!({
                "clientParameters": {"address": "fd00::2", "mtu": 16000},
                "serverAddress": "fd00::1",
                "serverRSDPort": 58783,
            });
            let resp_body = serde_json::to_vec(&resp).unwrap();
            server.write_all(MAGIC).await.unwrap();
            server
                .write_all(&(resp_body.len() as u16).to_be_bytes())
                .await
                .unwrap();
            server.write_all(&resp_body).await.unwrap();
            server.flush().await.unwrap();
        });

        let info = handshake(&mut client).await.unwrap();
        assert_eq!(info.client_address, "fd00::2");
        assert_eq!(info.server_address, "fd00::1");
        assert_eq!(info.server_rsd_port, 58783);
        server_task.await.unwrap();
    }
}
