//! 查 `com.apple.instruments.server.services.deviceinfo` 频道上的
//! `sysmonProcessAttributes`/`sysmonSystemAttributes`——设备自己权威地告诉你
//! `sysmontap` 支持哪些逐进程/系统级属性名,不用像 sysmontap 示例里那样瞎猜
//! (跟 pymobiledevice3 的 `Sysmontap.create()` 工厂方法做法一致)。
//!
//! 顺便也是第一次测试"真正需要等回复"的调用(之前 sysmontap 那边全是
//! fire-and-forget 的 `expects_reply=false`)。

use std::net::Ipv6Addr;
use std::str::FromStr;

use dtx::DtxClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let t0 = std::time::Instant::now();
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let device = devices.first().expect("plug in a device").clone();
    println!("device: {} ({:?})", device.udid, t0.elapsed());

    let t1 = std::time::Instant::now();
    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;

    let (info, stream) = tunnel::connect(device.device_id, &pairing_file).await?;
    let mut stack = tunnel::TunnelStack::new(stream, &info)?;
    let server_addr = Ipv6Addr::from_str(&info.server_address)?;
    println!("tunnel established ({:?})", t1.elapsed());

    let t2 = std::time::Instant::now();
    let rsd_info = rsd::connect_and_discover(&mut stack, &server_addr, info.server_rsd_port).await?;
    let dtx_port = rsd_info
        .service_port("com.apple.instruments.dtservicehub")
        .expect("dtservicehub not advertised");
    println!("RSD discovery done ({:?})", t2.elapsed());

    let t3 = std::time::Instant::now();
    let handle = stack.connect(server_addr, dtx_port).await?;
    let mut dtx = DtxClient::new(&mut stack, handle);
    dtx.perform_handshake().await?;
    println!("DTX capabilities handshake done ({:?})", t3.elapsed());

    let t4 = std::time::Instant::now();
    let channel = dtx
        .make_channel("com.apple.instruments.server.services.deviceinfo")
        .await?;
    println!("deviceinfo channel opened ({:?})", t4.elapsed());

    for selector in ["sysmonProcessAttributes", "sysmonSystemAttributes"] {
        let t = std::time::Instant::now();
        dtx.call_method_on(&channel, Some(selector), vec![], true).await?;
        let reply = dtx.read_message_on(&channel).await?;
        let _ = &reply.data;
        println!("{selector} done ({:?})", t.elapsed());
    }

    // 一次性的"进程列表快照"——不用像 sysmontap 那样先 setConfig/start 走
    // 持续采样流(那条路目前卡在设备主动断连的谜团上),`runningProcesses`
    // 是普通的单发单收 DTX 方法调用,回一个完整的进程数组。
    let t5 = std::time::Instant::now();
    dtx.call_method_on(&channel, Some("runningProcesses"), vec![], true).await?;
    let reply = dtx.read_message_on(&channel).await?;
    println!("runningProcesses call+decode done ({:?})", t5.elapsed());
    if let Some(plist::Value::Array(processes)) = &reply.data {
        println!("runningProcesses -> {} process(es)", processes.len());
        for p in processes.iter().take(15) {
            let dict = p.as_dictionary();
            let pid = dict.and_then(|d| d.get("pid")).and_then(|v| v.as_signed_integer());
            let name = dict.and_then(|d| d.get("name")).and_then(|v| v.as_string());
            println!("  pid={pid:?} name={name:?}");
        }
    } else {
        println!("runningProcesses -> {:#?}", reply.data);
    }

    let t6 = std::time::Instant::now();
    stack.close(handle).await?;
    println!("stack.close() done ({:?})", t6.elapsed());
    println!("total: {:?}", t0.elapsed());
    Ok(())
}
