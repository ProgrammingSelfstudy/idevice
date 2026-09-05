//! 选设备、建隧道、连 DTX、握手——sysmon/fps 两个子命令在真正打开各自频道之前
//! 要做的事完全一样，抽到这一个模块里，子命令只管自己的频道协议。

use std::net::Ipv6Addr;
use std::str::FromStr;

use dtx::DtxClient;
use tunnel::{SocketHandle, Transport, TunnelStack};
use usbmuxd::Device;

pub async fn select_device(udid: Option<&str>) -> anyhow::Result<(usbmuxd::UsbmuxdClient, Device)> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;

    let device = match udid {
        Some(udid) => devices
            .iter()
            .find(|d| d.udid == udid)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no connected device with udid {udid}"))?,
        None => match devices.len() {
            0 => anyhow::bail!("no devices connected"),
            1 => devices[0].clone(),
            _ => {
                let list = devices.iter().map(|d| format!("  {}", d.udid)).collect::<Vec<_>>().join("\n");
                anyhow::bail!("multiple devices connected, pick one with -s <udid>:\n{list}");
            }
        },
    };

    Ok((usbmuxd, device))
}

/// 建好隧道 + 连上 `dtservicehub` 端口，返回调用方可以直接用来构造
/// `DtxClient` 的 `(stack, handle)`——`DtxClient` 借用 `stack`，没法从这个函数
/// 内部直接返回，所以握手这一步留给调用方在自己的栈帧里做。
pub async fn open_dtx_socket(
    usbmuxd: &mut usbmuxd::UsbmuxdClient,
    device: &Device,
) -> anyhow::Result<(TunnelStack<Box<dyn Transport>>, SocketHandle)> {
    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;

    let (info, stream) = tunnel::connect(device.device_id, &pairing_file).await?;
    let mut stack = TunnelStack::new(stream, &info)?;
    let server_addr = Ipv6Addr::from_str(&info.server_address)?;

    let rsd_info = rsd::connect_and_discover(&mut stack, &server_addr, info.server_rsd_port).await?;
    let dtx_port = rsd_info
        .service_port("com.apple.instruments.dtservicehub")
        .ok_or_else(|| anyhow::anyhow!("dtservicehub not advertised by this device"))?;

    let handle = stack.connect(server_addr, dtx_port).await?;
    Ok((stack, handle))
}

pub async fn handshake(
    stack: &mut TunnelStack<Box<dyn Transport>>,
    handle: SocketHandle,
) -> anyhow::Result<DtxClient<'_, Box<dyn Transport>>> {
    let mut dtx = DtxClient::new(stack, handle);
    dtx.perform_handshake().await?;
    Ok(dtx)
}
