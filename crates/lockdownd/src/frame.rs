//! lockdownd 的帧协议——跟 usbmuxd 那套长得像但字节序不一样,这是个真实存在
//! 的坑(usbmuxd 是小端,lockdownd 是大端),这次重写专门把两层拆成两个 crate、
//! 两套 frame 模块,就是不想让这个字节序差异在共享代码里被"顺手"改错。
//!
//! ```text
//! [4B 大端 body_len(不含这4字节头本身)][plist XML]
//! ```

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::LockdownError;

pub(crate) async fn write_plist<W: AsyncWrite + Unpin>(
    writer: &mut W,
    dict: plist::Dictionary,
) -> Result<(), LockdownError> {
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, &plist::Value::Dictionary(dict))
        .map_err(LockdownError::PlistEncode)?;

    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn read_plist<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<plist::Dictionary, LockdownError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;

    let value: plist::Value = plist::from_bytes(&body).map_err(LockdownError::PlistDecode)?;
    let dict = value
        .into_dictionary()
        .ok_or_else(|| LockdownError::MalformedFrame("response body is not a dictionary".into()))?;

    if let Some(err) = dict.get("Error") {
        let msg = match err {
            plist::Value::String(s) => s.clone(),
            other => format!("{other:?}"),
        };
        return Err(LockdownError::DeviceError(msg));
    }

    Ok(dict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let (mut client, mut server) = duplex(4096);

        let mut req = plist::Dictionary::new();
        req.insert("Request".into(), "QueryType".into());
        write_plist(&mut client, req.clone()).await.unwrap();

        let got = read_plist(&mut server).await.unwrap();
        assert_eq!(got.get("Request"), req.get("Request"));
    }

    #[tokio::test]
    async fn an_error_field_in_the_response_becomes_a_device_error() {
        let (mut client, mut server) = duplex(4096);

        let mut res = plist::Dictionary::new();
        res.insert("Error".into(), "PasswordProtected".into());
        write_plist(&mut client, res).await.unwrap();

        let err = read_plist(&mut server).await.unwrap_err();
        assert!(matches!(err, LockdownError::DeviceError(e) if e == "PasswordProtected"));
    }
}
