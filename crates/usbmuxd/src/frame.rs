//! usbmuxd 的帧协议——不管底层是 Unix socket(macOS/Linux)还是 TCP
//! (Windows,见 `lib.rs` 顶部注释),线上跑的都是同一套很朴素的定长头 +
//! plist body 协议,协议本身没有官方文档,这里的字段布局是从公开的
//! libimobiledevice 生态里确认过的(Unix socket 这条路径这次重写全程真机
//! 测过,不是照抄凭空猜的;TCP 那条路径见 `lib.rs` 里单独的可信度说明)：
//!
//! ```text
//! [4B LE size(含这16字节头本身)][4B LE version][4B LE message_type][4B LE tag][plist XML]
//! ```
//!
//! - `version`:目前只用 `1`(XML plist)——usbmuxd 也支持二进制 plist(`0`),但没有
//!   必要,XML 好调试。
//! - `message_type`:发请求固定用 `8`(plist 消息);收到的回包也固定是 `8`——不像
//!   `1`(结果码消息)那种老式协议,新版本 usbmuxd 统一走 plist。
//! - `tag`:请求/响应配对用的序号,这里简化成永远发 `1`——usbmuxd 本身不要求严格
//!   递增,只要请求方自己认得出哪个回包对应哪个请求就行,我们是同步收发（发一条等
//!   一条回包才发下一条),用不上真正的乱序匹配。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::UsbmuxdError;

const HEADER_LEN: usize = 16;
pub(crate) const XML_PLIST_VERSION: u32 = 1;
pub(crate) const PLIST_MESSAGE_TYPE: u32 = 8;
const TAG: u32 = 1;

pub(crate) async fn write_plist<W: AsyncWrite + Unpin>(
    writer: &mut W,
    dict: plist::Dictionary,
) -> Result<(), UsbmuxdError> {
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, &plist::Value::Dictionary(dict))
        .map_err(UsbmuxdError::PlistEncode)?;

    let size = (body.len() + HEADER_LEN) as u32;
    let mut packet = Vec::with_capacity(body.len() + HEADER_LEN);
    packet.extend_from_slice(&size.to_le_bytes());
    packet.extend_from_slice(&XML_PLIST_VERSION.to_le_bytes());
    packet.extend_from_slice(&PLIST_MESSAGE_TYPE.to_le_bytes());
    packet.extend_from_slice(&TAG.to_le_bytes());
    packet.extend_from_slice(&body);

    writer.write_all(&packet).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn read_plist<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<plist::Dictionary, UsbmuxdError> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).await?;

    let size = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    if size < HEADER_LEN {
        return Err(UsbmuxdError::MalformedFrame(format!(
            "packet size {size} smaller than header"
        )));
    }
    let body_len = size - HEADER_LEN;

    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body).await?;

    let value: plist::Value =
        plist::from_bytes(&body).map_err(UsbmuxdError::PlistDecode)?;
    value
        .into_dictionary()
        .ok_or_else(|| UsbmuxdError::MalformedFrame("response body is not a dictionary".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let (mut client, mut server) = duplex(4096);

        let mut req = plist::Dictionary::new();
        req.insert("MessageType".into(), "ListDevices".into());
        write_plist(&mut client, req.clone()).await.unwrap();

        let got = read_plist(&mut server).await.unwrap();
        assert_eq!(got.get("MessageType"), req.get("MessageType"));
    }

    #[tokio::test]
    async fn read_rejects_a_header_claiming_a_smaller_size_than_the_header_itself() {
        let (mut client, mut server) = duplex(64);
        // size = 4 (nonsensical: smaller than the 16-byte header it's part of)
        client.write_all(&4u32.to_le_bytes()).await.unwrap();
        client.write_all(&[0u8; 12]).await.unwrap();
        client.flush().await.unwrap();
        drop(client);

        let err = read_plist(&mut server).await.unwrap_err();
        assert!(matches!(err, UsbmuxdError::MalformedFrame(_)));
    }
}
