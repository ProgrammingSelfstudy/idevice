//! 探测 `com.apple.mobile.mobile_image_mounter.shim.remote`——这类
//! `*.shim.remote` 服务是经典 lockdownd 服务原样代理到隧道上的,帧格式跟
//! lockdownd 一样(`[4B 大端 body_len][XML plist]`),不是 XPC——这里直接手写
//! 一份最小的同款帧收发,不依赖 lockdownd crate 里那个 `pub(crate)` 的 frame
//! 模块。
//!
//! 第一次尝试直接发 `LookupImage` 连接就被设备无声挂断——查了
//! pymobiledevice3(`remote/remote_service_discovery.py` 的
//! `RemoteServiceDiscoveryService.start_lockdown_service`)才发现:连上端口后
//! 必须先做一次 `RSDCheckin` 握手(发 `{Label, ProtocolVersion:"2",
//! Request:"RSDCheckin"}`,收一条 echo,再收一条设备**主动推**的
//! `{Request:"StartService"}` 确认),之后这条连接才算真正"启动"、能接受业务
//! 命令——跟 DTX 那边连接建立后必须先做 `_notifyOfPublishedCapabilities:`
//! 握手是同一个套路,只是这次是 lockdown-style 服务自己的握手,不是 DTX 的。
//!
//! 目的:确认 Developer Mode 已开启(见 `lockdownd/examples/devmode.rs`)之后,
//! sysmontap "ack 后即断"这个谜团的下一个假设——开发者磁盘镜像是否已挂载。

use std::net::Ipv6Addr;
use std::str::FromStr;

use tunnel::{SocketHandle, TunnelStack};

async fn send_plist<S>(stack: &mut TunnelStack<S>, handle: SocketHandle, dict: plist::Dictionary) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, &plist::Value::Dictionary(dict))?;
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend(body);
    stack.send_all(handle, &out).await?;
    Ok(())
}

/// `recv_buf` 是调用方持有的、跨调用累积的缓冲区——两条消息可能背靠背挤在
/// 同一个 TCP 段里一起到达,只消费掉当前这一条的字节,剩下的必须留给下一次
/// 调用,不能像"每次都开一个全新局部 buf"那样把多收到的部分静默丢掉。
async fn recv_plist<S>(
    stack: &mut TunnelStack<S>,
    handle: SocketHandle,
    recv_buf: &mut Vec<u8>,
) -> anyhow::Result<plist::Dictionary>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    while recv_buf.len() < 4 {
        let chunk = stack.recv(handle).await?;
        if chunk.is_empty() {
            anyhow::bail!("connection closed before length header arrived");
        }
        recv_buf.extend(chunk);
    }
    let len = u32::from_be_bytes(recv_buf[0..4].try_into().unwrap()) as usize;
    while recv_buf.len() < 4 + len {
        let chunk = stack.recv(handle).await?;
        if chunk.is_empty() {
            anyhow::bail!("connection closed before full body arrived");
        }
        recv_buf.extend(chunk);
    }
    let value: plist::Value = plist::from_bytes(&recv_buf[4..4 + len])?;
    recv_buf.drain(..4 + len);
    value
        .into_dictionary()
        .ok_or_else(|| anyhow::anyhow!("response body is not a dictionary"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let device = devices.first().expect("plug in a device").clone();
    println!("device: {}", device.udid);

    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;

    let (info, stream) = tunnel::connect(device.device_id, &pairing_file).await?;
    let mut stack = tunnel::TunnelStack::new(stream, &info)?;
    let server_addr = Ipv6Addr::from_str(&info.server_address)?;

    let rsd_info = rsd::connect_and_discover(&mut stack, &server_addr, info.server_rsd_port).await?;
    let port = rsd_info
        .service_port("com.apple.mobile.mobile_image_mounter.shim.remote")
        .expect("mobile_image_mounter shim not advertised");
    println!("mobile_image_mounter port: {port}");

    let handle = stack.connect(server_addr, port).await?;
    println!("TCP handshake completed");
    let mut recv_buf = Vec::new();

    // 第一步:RSDCheckin 握手——不做这步,设备会在收到别的请求时无声关闭连接。
    let mut checkin = plist::Dictionary::new();
    checkin.insert("Label".into(), "idevice-rs-native".into());
    checkin.insert("ProtocolVersion".into(), "2".into());
    checkin.insert("Request".into(), "RSDCheckin".into());
    send_plist(&mut stack, handle, checkin).await?;

    let checkin_reply = recv_plist(&mut stack, handle, &mut recv_buf).await?;
    println!("RSDCheckin reply: {checkin_reply:#?}");

    let start_service_reply = recv_plist(&mut stack, handle, &mut recv_buf).await?;
    println!("StartService reply: {start_service_reply:#?}");
    if let Some(err) = start_service_reply.get("Error") {
        anyhow::bail!("device reported StartService error: {err:?}");
    }

    // 第二步:真正的业务命令。
    let mut req = plist::Dictionary::new();
    req.insert("Command".into(), "LookupImage".into());
    req.insert("ImageType".into(), "Personalized".into());
    send_plist(&mut stack, handle, req).await?;

    let response = recv_plist(&mut stack, handle, &mut recv_buf).await?;
    println!("LookupImage response: {response:#?}");

    stack.close(handle).await?;
    Ok(())
}
