//! GPU/FPS——`com.apple.instruments.server.services.graphics.opengl` 频道。
//! 跟 `sysmontap` 完全不同的套路:不用 `setConfig:`,直接调
//! `startSamplingAtTimeInterval:`(传一个 `Double` 参数,`0.0` 表示"有数据就
//! 推,不用固定间隔"),之后设备周期性推数据,payload 里直接就是可读的键值
//! (`CoreAnimationFramesPerSecond` 就是 FPS,`Device Utilization %`/
//! `Renderer Utilization %` 是 GPU 占用)。每条推送都要求回复(`expects_reply`),
//! 跟 `DtxClient::reply_to` 已有的逻辑正好对上,不用改协议层代码。
//!
//! 用法: cargo run -p dtx --example fps -- [-s <udid>]

use std::net::Ipv6Addr;
use std::str::FromStr;

use dtx::{AuxValue, DtxClient};

fn parse_udid_arg() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg == "-s" {
            return it.next();
        }
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;

    let device = match parse_udid_arg() {
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
    eprintln!("device: {}", device.udid);

    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;

    let (info, stream) = tunnel::connect(device.device_id, &pairing_file).await?;
    let mut stack = tunnel::TunnelStack::new(stream, &info)?;
    let server_addr = Ipv6Addr::from_str(&info.server_address)?;

    let rsd_info = rsd::connect_and_discover(&mut stack, &server_addr, info.server_rsd_port).await?;
    let dtx_port = rsd_info
        .service_port("com.apple.instruments.dtservicehub")
        .expect("dtservicehub not advertised");

    let handle = stack.connect(server_addr, dtx_port).await?;
    let mut dtx = DtxClient::new(&mut stack, handle);
    dtx.perform_handshake().await?;

    let channel = dtx
        .make_channel("com.apple.instruments.server.services.graphics.opengl")
        .await?;

    dtx.call_method_on(&channel, Some("startSamplingAtTimeInterval:"), vec![AuxValue::Double(0.0)], true)
        .await?;
    let ack = dtx.read_message_on(&channel).await?;
    if ack.expects_reply {
        dtx.reply_to(&ack).await?;
    }
    eprintln!("sampling started, waiting for frames (ctrl-c to quit)...\n");

    println!("{:>6} {:>6} {:>6} {:>10} FPS", "DEV%", "REND%", "TILE%", "MEM");
    loop {
        let msg = dtx.read_message_on(&channel).await?;
        if msg.expects_reply {
            dtx.reply_to(&msg).await?;
        }
        let Some(dict) = msg.data.as_ref().and_then(|v| v.as_dictionary()) else {
            continue;
        };
        let get_i = |k: &str| dict.get(k).and_then(|v| v.as_signed_integer()).unwrap_or(0);
        let mem = get_i("In use system memory");
        println!(
            "{:>6} {:>6} {:>6} {:>10} {}",
            get_i("Device Utilization %"),
            get_i("Renderer Utilization %"),
            get_i("Tiler Utilization %"),
            format!("{:.1}M", mem as f64 / 1_048_576.0),
            get_i("CoreAnimationFramesPerSecond"),
        );
    }
}
