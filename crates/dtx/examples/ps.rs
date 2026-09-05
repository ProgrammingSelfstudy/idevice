//! 列进程——`deviceinfo` 频道的 `runningProcesses` 一次性快照,不走
//! `sysmontap` 那套持续采样流(那边目前卡在设备主动断连的谜团上)。
//!
//! 用法: cargo run -p dtx --example ps -- [-s <udid>]
//! 接了多台设备又没传 `-s` 就报错列出所有 udid,跟 adb `-s <serial>` 一个
//! 思路。

use std::net::Ipv6Addr;
use std::str::FromStr;

use dtx::DtxClient;

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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

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
    println!("device: {}", device.udid);

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
        .make_channel("com.apple.instruments.server.services.deviceinfo")
        .await?;

    dtx.call_method_on(&channel, Some("runningProcesses"), vec![], true).await?;
    let reply = dtx.read_message_on(&channel).await?;

    let Some(plist::Value::Array(processes)) = &reply.data else {
        anyhow::bail!("unexpected runningProcesses reply shape: {:#?}", reply.data);
    };

    let mut rows: Vec<(i64, i64, String)> = processes
        .iter()
        .filter_map(|p| {
            let d = p.as_dictionary()?;
            let pid = d.get("pid")?.as_signed_integer()?;
            let ppid = d
                .get("ppid")
                .or_else(|| d.get("parentUniqueID"))
                .and_then(|v| v.as_signed_integer())
                .unwrap_or(-1);
            let name = d.get("name").and_then(|v| v.as_string()).unwrap_or("?").to_string();
            Some((pid, ppid, name))
        })
        .collect();
    rows.sort_by_key(|(pid, ..)| *pid);

    println!("{:>7} {:>7} NAME", "PID", "PPID");
    for (pid, ppid, name) in &rows {
        let ppid_str = if *ppid < 0 { "?".to_string() } else { ppid.to_string() };
        println!("{pid:>7} {ppid_str:>7} {name}");
    }
    println!("({} process(es))", rows.len());

    stack.close(handle).await?;
    Ok(())
}
