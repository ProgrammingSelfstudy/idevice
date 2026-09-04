//! CDTunnel 握手完成之后,同一条连接上收发的是裸 IPv6 包——没有额外分帧,包
//! 本身的长度就写在 IPv6 头里(第 4-5 字节,payload length,大端),读的时候
//! 先固定读 40 字节头,再照头里那个长度读 payload。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::TunnelError;

const IPV6_HEADER_LEN: usize = 40;

pub async fn send_packet<S: AsyncWrite + Unpin>(
    stream: &mut S,
    packet: &[u8],
) -> Result<(), TunnelError> {
    stream.write_all(packet).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn recv_packet<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, TunnelError> {
    let mut header = [0u8; IPV6_HEADER_LEN];
    stream.read_exact(&mut header).await?;

    let payload_len = u16::from_be_bytes([header[4], header[5]]) as usize;
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await?;

    let mut packet = Vec::with_capacity(IPV6_HEADER_LEN + payload_len);
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&payload);
    Ok(packet)
}
