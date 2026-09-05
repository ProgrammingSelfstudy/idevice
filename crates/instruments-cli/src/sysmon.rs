//! `sysmontap`——逐进程 CPU/内存/线程数。`--json` 模式下按行输出，供上层
//! (比如 Go 的子进程调用方)按行解析；不带 `--json` 时打印跟人看的表格，
//! 方便手动联调，行为跟原来 `crates/dtx/examples/sysmon.rs` 一致。

use dtx::DtxClient;
use serde::Serialize;
use tunnel::Transport;

#[derive(Serialize)]
struct ProcessSample {
    pid: i64,
    name: String,
    #[serde(rename = "cpuUsage")]
    cpu_usage: f64,
    /// 原始字节数——不像 pymobiledevice3 那样拼成带单位的字符串，交给调用方
    /// 自己换算，避免字符串解析这道额外的、可能出错的工序。
    #[serde(rename = "physFootprintBytes")]
    phys_footprint_bytes: f64,
    #[serde(rename = "threadCount")]
    thread_count: i64,
}

pub struct Options {
    pub process_filter: Option<String>,
    pub json: bool,
    pub interval_ms: i64,
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

pub async fn run(
    dtx: &mut DtxClient<'_, Box<dyn Transport>>,
    opts: &Options,
) -> anyhow::Result<()> {
    let deviceinfo_channel = dtx.make_channel("com.apple.instruments.server.services.deviceinfo").await?;

    dtx.call_method_on(&deviceinfo_channel, Some("sysmonProcessAttributes"), vec![], true).await?;
    let proc_attrs_reply = dtx.read_message_on(&deviceinfo_channel).await?;
    let Some(plist::Value::Array(proc_attrs_values)) = proc_attrs_reply.data else {
        anyhow::bail!("unexpected sysmonProcessAttributes reply: {:#?}", proc_attrs_reply.data);
    };
    let proc_attrs: Vec<String> =
        proc_attrs_values.iter().filter_map(|v| v.as_string().map(str::to_string)).collect();

    dtx.call_method_on(&deviceinfo_channel, Some("sysmonSystemAttributes"), vec![], true).await?;
    let sys_attrs_reply = dtx.read_message_on(&deviceinfo_channel).await?;
    let Some(plist::Value::Array(sys_attrs_values)) = sys_attrs_reply.data else {
        anyhow::bail!("unexpected sysmonSystemAttributes reply: {:#?}", sys_attrs_reply.data);
    };

    let pid_idx = proc_attrs
        .iter()
        .position(|s| s == "pid")
        .ok_or_else(|| anyhow::anyhow!("device didn't advertise a 'pid' process attribute"))?;
    let name_idx = proc_attrs.iter().position(|s| s == "name");
    let cpu_idx = proc_attrs.iter().position(|s| s == "cpuUsage");
    let mem_idx = proc_attrs.iter().position(|s| s == "physFootprint");
    let thread_idx = proc_attrs.iter().position(|s| s == "threadCount");

    let channel = dtx.make_channel("com.apple.instruments.server.services.sysmontap").await?;

    let mut config = plist::Dictionary::new();
    config.insert("ur".into(), plist::Value::Integer(1i64.into()));
    config.insert("bm".into(), plist::Value::Integer(0i64.into()));
    config.insert("procAttrs".into(), plist::Value::Array(proc_attrs_values));
    config.insert("sysAttrs".into(), plist::Value::Array(sys_attrs_values));
    config.insert("cpuUsage".into(), plist::Value::Boolean(true));
    config.insert("physFootprint".into(), plist::Value::Boolean(true));
    config.insert("sampleInterval".into(), plist::Value::Integer((opts.interval_ms * 1_000_000).into()));

    dtx.call_method_on(&channel, Some("setConfig:"), vec![dtx::AuxValue::archived(plist::Value::Dictionary(config))], false)
        .await?;
    dtx.call_method_on(&channel, Some("start"), vec![], false).await?;
    if !opts.json {
        eprintln!("waiting for the first real sample (device sends a header + heartbeats first, usually a couple seconds)...");
    }

    loop {
        let msg = dtx.read_message_on(&channel).await?;
        if msg.expects_reply {
            dtx.reply_to(&msg).await?;
        }
        let Some(data) = &msg.data else { continue };
        let Some(samples) = data.as_array() else { continue };

        for sample in samples {
            let Some(dict) = sample.as_dictionary() else { continue };
            let Some(processes) = dict.get("Processes").and_then(|v| v.as_dictionary()) else { continue };

            let mut rows: Vec<ProcessSample> = processes
                .values()
                .filter_map(|v| {
                    let values = v.as_array()?;
                    let pid = values.get(pid_idx)?.as_signed_integer()?;
                    let name = name_idx.and_then(|i| values.get(i)).and_then(|v| v.as_string()).unwrap_or("?").to_string();
                    let cpu_usage = cpu_idx.and_then(|i| values.get(i)).and_then(as_f64).unwrap_or(0.0);
                    let phys_footprint_bytes = mem_idx.and_then(|i| values.get(i)).and_then(as_f64).unwrap_or(0.0);
                    let thread_count = thread_idx.and_then(|i| values.get(i)).and_then(|v| v.as_signed_integer()).unwrap_or(0);
                    Some(ProcessSample { pid, name, cpu_usage, phys_footprint_bytes, thread_count })
                })
                .collect();
            rows.sort_by(|a, b| b.cpu_usage.partial_cmp(&a.cpu_usage).unwrap_or(std::cmp::Ordering::Equal));

            if let Some(filter) = &opts.process_filter {
                rows.retain(|r| &r.name == filter);
            }

            emit(&rows, opts.json);
        }
    }
}

fn emit(rows: &[ProcessSample], json: bool) {
    if json {
        // 按进程名过滤时通常只剩一条——每采样周期打一行，调用方逐行 parse
        // 就行，不用像 pymobiledevice3 的 `--human` 输出那样跨行拼 JSON。
        // 不过滤时把当前批次全部进程打成一个数组，调用方自己挑要看哪个。
        match rows.len() {
            0 => {}
            1 => println!("{}", serde_json::to_string(&rows[0]).expect("ProcessSample serializes")),
            _ => println!("{}", serde_json::to_string(rows).expect("Vec<ProcessSample> serializes")),
        }
        return;
    }

    print!("\x1B[2J\x1B[H");
    println!("{:>7} {:<28} {:>6} {:>10} {:>5}", "PID", "NAME", "CPU%", "MEM", "THR");
    for row in rows.iter().take(30) {
        println!(
            "{:>7} {:<28} {:>6.1} {:>10} {:>5}",
            row.pid,
            row.name,
            row.cpu_usage,
            format_bytes(row.phys_footprint_bytes),
            row.thread_count
        );
    }
    println!("\n({} processes total, ctrl-c to quit)", rows.len());
}
