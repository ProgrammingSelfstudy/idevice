//! 按进程统计网络流量——`com.apple.instruments.server.services.networking`
//! 频道。协议参考 pymobiledevice3 的 `services/dvt/instruments/
//! network_monitor.py`(它自己也没有官方文档,是从这份实现读出来的):
//!
//! 调 `startMonitoring`(不需要回复)之后,设备推三种类型的事件,payload
//! 顶层都是 `[消息类型, 参数数组]`:
//!   - 0 = 网卡发现,这里用不上,跳过。
//!   - 1 = 连接建立:`[本地地址, 远端地址, 网卡序号, pid, 收缓冲区大小,
//!     收缓冲区占用, 连接序号, kind]`——**带 pid**,但不带流量数据。
//!   - 2 = 流量更新:`[rx_packets, rx_bytes, tx_packets, tx_bytes, rx_dups,
//!     rx000, tx_retx, min_rtt, avg_rtt, 连接序号, 时间戳]`——带流量数据,
//!     但只有连接序号,不直接带 pid。
//!
//! 两种事件用"连接序号"串起来:先从类型1事件建立"连接序号 -> pid"的映射,
//! 再用类型2事件按连接序号查回 pid,按 pid 把各个连接的最新累计字节数加起来
//! (每条类型2事件里的 rx_bytes/tx_bytes 是这条连接自己的累计值,不是增量,
//! 所以是"覆盖"最新值而不是累加事件)。
//!
//! 用法: cargo run -p dtx --example netstat -- [-s <udid>]

use std::collections::HashMap;
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

    let channel = dtx
        .make_channel("com.apple.instruments.server.services.networking")
        .await?;
    dtx.call_method_on(&channel, Some("startMonitoring"), vec![], false).await?;
    eprintln!("monitoring started, waiting for connections (ctrl-c to quit)...\n");

    // 连接序号 -> pid(从类型1事件建立)。
    let mut serial_to_pid: HashMap<i64, i64> = HashMap::new();
    // 连接序号 -> 这条连接最新的 (rx_bytes, tx_bytes) 累计值(从类型2事件覆盖更新)。
    let mut serial_stats: HashMap<i64, (i64, i64)> = HashMap::new();
    // 连接序号 -> 最后一次收到它的任何事件(建立或流量更新)的时间——协议本身
    // 没有"连接关闭"这个消息类型,只能靠"太久没更新过"这个启发式来判断一条
    // 连接是不是已经不在用了,不然上面两张表会跟着进程一直跑无限变大。
    let mut last_seen: HashMap<i64, std::time::Instant> = HashMap::new();

    let mut last_refresh = std::time::Instant::now();
    let mut last_gc = std::time::Instant::now();
    const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
    const GC_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

    loop {
        let msg = dtx.read_message_on(&channel).await?;
        if msg.expects_reply {
            dtx.reply_to(&msg).await?;
        }
        let Some(plist::Value::Array(top)) = &msg.data else { continue };
        let [msg_type, args] = &top[..] else { continue };
        let Some(msg_type) = msg_type.as_signed_integer() else { continue };
        let Some(args) = args.as_array() else { continue };

        match msg_type {
            1 => {
                // 连接建立: [local_addr, remote_addr, iface, pid, recv_buf_size, recv_buf_used, serial, kind]
                let pid = args.get(3).and_then(|v| v.as_signed_integer());
                let serial = args.get(6).and_then(|v| v.as_signed_integer());
                if let (Some(pid), Some(serial)) = (pid, serial) {
                    serial_to_pid.insert(serial, pid);
                    last_seen.insert(serial, std::time::Instant::now());
                }
            }
            2 => {
                // 流量更新: [rx_packets, rx_bytes, tx_packets, tx_bytes, rx_dups, rx000, tx_retx, min_rtt, avg_rtt, serial, time]
                let rx_bytes = args.get(1).and_then(|v| v.as_signed_integer()).unwrap_or(0);
                let tx_bytes = args.get(3).and_then(|v| v.as_signed_integer()).unwrap_or(0);
                let serial = args.get(9).and_then(|v| v.as_signed_integer());
                if let Some(serial) = serial {
                    serial_stats.insert(serial, (rx_bytes, tx_bytes));
                    last_seen.insert(serial, std::time::Instant::now());
                }
            }
            _ => continue,
        }

        // 每隔一段时间清一次超过 STALE_AFTER 没更新过的连接序号——GC_EVERY 比
        // 展示刷新的间隔长得多,不需要每次刷新都扫一遍整张表。
        if last_gc.elapsed() >= GC_EVERY {
            last_gc = std::time::Instant::now();
            let stale: Vec<i64> = last_seen
                .iter()
                .filter(|(_, seen)| seen.elapsed() >= STALE_AFTER)
                .map(|(serial, _)| *serial)
                .collect();
            for serial in stale {
                last_seen.remove(&serial);
                serial_to_pid.remove(&serial);
                serial_stats.remove(&serial);
            }
        }

        // 按真实经过的时间刷新,不按消息条数——流量更新事件的到达频率本身就
        // 不稳定,按条数刷新会导致刷新间隔忽快忽慢。
        if last_refresh.elapsed() < REFRESH_EVERY {
            continue;
        }
        last_refresh = std::time::Instant::now();

        // pid < 0 是设备自己用的占位值(比如 -2,代表"监控开始前就已经存在、
        // 没有对应连接建立事件、查不到真实 pid 的连接"),单独拎到最后一行,
        // 不跟真正查到 pid 的进程混在一起排序。
        let mut per_pid: HashMap<i64, (i64, i64)> = HashMap::new();
        let mut unattributed: (i64, i64) = (0, 0);
        for (serial, (rx, tx)) in &serial_stats {
            match serial_to_pid.get(serial) {
                Some(&pid) if pid >= 0 => {
                    let entry = per_pid.entry(pid).or_insert((0, 0));
                    entry.0 += rx;
                    entry.1 += tx;
                }
                _ => {
                    unattributed.0 += rx;
                    unattributed.1 += tx;
                }
            }
        }
        let mut rows: Vec<(i64, i64, i64)> = per_pid.into_iter().map(|(pid, (rx, tx))| (pid, rx, tx)).collect();
        rows.sort_by_key(|(_, rx, tx)| std::cmp::Reverse(rx + tx));

        print!("\x1B[2J\x1B[H");
        println!("{:>7} {:>10} {:>10}", "PID", "RX", "TX");
        for (pid, rx, tx) in rows.iter().take(30) {
            println!("{pid:>7} {:>10} {:>10}", format_bytes(*rx as f64), format_bytes(*tx as f64));
        }
        println!(
            "{:>7} {:>10} {:>10}  (pre-existing connections, no known pid)",
            "-",
            format_bytes(unattributed.0 as f64),
            format_bytes(unattributed.1 as f64)
        );
        println!(
            "\n({} connections tracked, {} with a known pid, ctrl-c to quit)",
            serial_stats.len(),
            rows.len()
        );
    }
}
