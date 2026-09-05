//! 真正能看的 sysmon——逐进程 CPU/内存/线程数,持续刷新的表格。
//!
//! 用法: cargo run -p dtx --example sysmon -- [-s <udid>]
//!
//! `setConfig:` 里的 `procAttrs` 顺序就是设备后续推 `Processes` 数据时每个
//! pid 对应的数值数组的顺序——这个顺序是我们自己发的,不是苹果规定死的,
//! 所以只要发送和解析两边用同一份顺序(下面直接复用查 `sysmonProcessAttributes`
//! 拿到的那份),就不需要关心它到底是什么顺序。

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

fn as_f64(v: &plist::Value) -> Option<f64> {
    v.as_real().or_else(|| v.as_signed_integer().map(|i| i as f64))
}

fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
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

    let deviceinfo_channel = dtx
        .make_channel("com.apple.instruments.server.services.deviceinfo")
        .await?;

    dtx.call_method_on(&deviceinfo_channel, Some("sysmonProcessAttributes"), vec![], true)
        .await?;
    let proc_attrs_reply = dtx.read_message_on(&deviceinfo_channel).await?;
    let Some(plist::Value::Array(proc_attrs_values)) = proc_attrs_reply.data else {
        anyhow::bail!("unexpected sysmonProcessAttributes reply: {:#?}", proc_attrs_reply.data);
    };
    let proc_attrs: Vec<String> = proc_attrs_values
        .iter()
        .filter_map(|v| v.as_string().map(str::to_string))
        .collect();

    dtx.call_method_on(&deviceinfo_channel, Some("sysmonSystemAttributes"), vec![], true)
        .await?;
    let sys_attrs_reply = dtx.read_message_on(&deviceinfo_channel).await?;
    let Some(plist::Value::Array(sys_attrs_values)) = sys_attrs_reply.data else {
        anyhow::bail!("unexpected sysmonSystemAttributes reply: {:#?}", sys_attrs_reply.data);
    };

    let pid_idx = proc_attrs
        .iter()
        .position(|s| s == "pid")
        .expect("device didn't advertise a 'pid' process attribute");
    let name_idx = proc_attrs.iter().position(|s| s == "name");
    let cpu_idx = proc_attrs.iter().position(|s| s == "cpuUsage");
    let mem_idx = proc_attrs.iter().position(|s| s == "physFootprint");
    let thread_idx = proc_attrs.iter().position(|s| s == "threadCount");

    let channel = dtx
        .make_channel("com.apple.instruments.server.services.sysmontap")
        .await?;

    let mut config = plist::Dictionary::new();
    config.insert("ur".into(), plist::Value::Integer(1i64.into()));
    config.insert("bm".into(), plist::Value::Integer(0i64.into()));
    config.insert("procAttrs".into(), plist::Value::Array(proc_attrs_values));
    config.insert("sysAttrs".into(), plist::Value::Array(sys_attrs_values));
    config.insert("cpuUsage".into(), plist::Value::Boolean(true));
    config.insert("physFootprint".into(), plist::Value::Boolean(true));
    config.insert("sampleInterval".into(), plist::Value::Integer(500_000_000i64.into()));

    dtx.call_method_on(
        &channel,
        Some("setConfig:"),
        vec![dtx::AuxValue::archived(plist::Value::Dictionary(config))],
        false,
    )
    .await?;
    dtx.call_method_on(&channel, Some("start"), vec![], false).await?;
    eprintln!("waiting for the first real sample (device sends a header + heartbeats first, usually a couple seconds)...");

    loop {
        let msg = dtx.read_message_on(&channel).await?;
        if msg.expects_reply {
            dtx.reply_to(&msg).await?;
        }
        let Some(data) = &msg.data else { continue };
        // 心跳/ack 是 `{"DTTapMessagePlist": {...}}` 这种字典,不是数组,直接跳过。
        let Some(samples) = data.as_array() else { continue };

        for sample in samples {
            let Some(dict) = sample.as_dictionary() else { continue };
            let Some(processes) = dict.get("Processes").and_then(|v| v.as_dictionary()) else { continue };

            let mut rows: Vec<(i64, String, f64, f64, i64)> = processes
                .values()
                .filter_map(|v| {
                    let values = v.as_array()?;
                    let pid = values.get(pid_idx)?.as_signed_integer()?;
                    let name = name_idx
                        .and_then(|i| values.get(i))
                        .and_then(|v| v.as_string())
                        .unwrap_or("?")
                        .to_string();
                    let cpu = cpu_idx.and_then(|i| values.get(i)).and_then(as_f64).unwrap_or(0.0);
                    let mem = mem_idx.and_then(|i| values.get(i)).and_then(as_f64).unwrap_or(0.0);
                    let threads = thread_idx
                        .and_then(|i| values.get(i))
                        .and_then(|v| v.as_signed_integer())
                        .unwrap_or(0);
                    Some((pid, name, cpu, mem, threads))
                })
                .collect();
            rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

            print!("\x1B[2J\x1B[H"); // 清屏,做成 top 那样原地刷新的效果
            println!("{:>7} {:<28} {:>6} {:>10} {:>5}", "PID", "NAME", "CPU%", "MEM", "THR");
            for (pid, name, cpu, mem, threads) in rows.iter().take(30) {
                println!("{pid:>7} {name:<28} {cpu:>6.1} {:>10} {threads:>5}", format_bytes(*mem));
            }
            println!("\n({} processes total, ctrl-c to quit)", rows.len());
        }
    }
}
