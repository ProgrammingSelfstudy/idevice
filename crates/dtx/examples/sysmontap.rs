use std::net::Ipv6Addr;
use std::str::FromStr;

use dtx::{AuxValue, DtxClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let target_udid = std::env::args().skip_while(|a| a != "-s").nth(1);
    let device = match target_udid {
        Some(udid) => devices
            .iter()
            .find(|d| d.udid == udid)
            .expect("no connected device with that udid")
            .clone(),
        None => devices.first().expect("plug in a device").clone(),
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
    println!("dtservicehub port: {dtx_port}");

    // 实验:每次都开一条全新的 TCP 连接 + 全新的 sysmontap 频道,看设备是不是
    // "每条连接只吐一条 ack 就半关闭"这种一次性行为——如果是,重连后应该还是
    // 只收到一条就断;如果只是这条连接本身的偶发问题,重连后应该能收到更多。
    for attempt in 0..1 {
        println!("=== attempt {attempt}: opening a fresh connection ===");
        let handle = stack.connect(server_addr, dtx_port).await?;
        let mut dtx = DtxClient::new(&mut stack, handle);

        dtx.perform_handshake().await?;
        println!("capabilities handshake done");

        // 真正的根因:跟 pymobiledevice3 抓包比对才发现,之前一直传一个"退化"
        // 配置(`procAttrs: ["pid"]`,单独一个属性名)——设备自己确认这个名字
        // 合法,但真实客户端(Instruments/pymobiledevice3)从来不会这么问,
        // 而是把 `sysmonProcessAttributes`/`sysmonSystemAttributes` 查到的
        // 全部属性名原样传回去。退化配置会在 ack 之后立刻被设备挂断,完整配置
        // 则完全不会——这不是协议格式问题,是设备对"配置像不像一个真实客户端"
        // 有隐性要求。
        let deviceinfo_channel = dtx
            .make_channel("com.apple.instruments.server.services.deviceinfo")
            .await?;
        dtx.call_method_on(&deviceinfo_channel, Some("sysmonProcessAttributes"), vec![], true)
            .await?;
        let proc_attrs_reply = dtx.read_message_on(&deviceinfo_channel).await?;
        let Some(plist::Value::Array(proc_attrs)) = proc_attrs_reply.data else {
            anyhow::bail!("unexpected sysmonProcessAttributes reply: {:#?}", proc_attrs_reply.data);
        };
        dtx.call_method_on(&deviceinfo_channel, Some("sysmonSystemAttributes"), vec![], true)
            .await?;
        let sys_attrs_reply = dtx.read_message_on(&deviceinfo_channel).await?;
        let Some(plist::Value::Array(sys_attrs)) = sys_attrs_reply.data else {
            anyhow::bail!("unexpected sysmonSystemAttributes reply: {:#?}", sys_attrs_reply.data);
        };
        println!("got {} procAttrs, {} sysAttrs", proc_attrs.len(), sys_attrs.len());

        let channel = dtx
            .make_channel("com.apple.instruments.server.services.sysmontap")
            .await?;
        println!("sysmontap channel opened");

        // setConfig: 参数是一个 NSKeyedArchiver 归档的字典——`ur`=输出频率
        // (毫秒)、`procAttrs`/`sysAttrs`=上面查到的完整属性名列表、
        // `cpuUsage`/`physFootprint`=开关、`sampleInterval`=采样间隔(纳秒,
        // 500ms,照抄 pymobiledevice3 实际抓到的值)。
        let mut config = plist::Dictionary::new();
        config.insert("ur".into(), plist::Value::Integer(1i64.into()));
        config.insert("bm".into(), plist::Value::Integer(0i64.into()));
        config.insert("procAttrs".into(), plist::Value::Array(proc_attrs));
        config.insert("sysAttrs".into(), plist::Value::Array(sys_attrs));
        config.insert("cpuUsage".into(), plist::Value::Boolean(true));
        config.insert("physFootprint".into(), plist::Value::Boolean(true));
        config.insert("sampleInterval".into(), plist::Value::Integer(500_000_000i64.into()));

        dtx.call_method_on(
            &channel,
            Some("setConfig:"),
            vec![AuxValue::archived(plist::Value::Dictionary(config))],
            false,
        )
        .await?;
        println!("setConfig: sent");

        dtx.call_method_on(&channel, Some("start"), vec![], false).await?;
        println!("start sent, waiting for samples...");

        // start 之后设备会先推一条 ack,再是真正的采样行——跟之前挂在这一步的
        // idevice 版本不一样,这次我们自己的通用 Uid 解析器应该能把 Processes/
        // System 这些字段读出来,不用特殊跳过第一条。
        for i in 0..150 {
            let started = std::time::Instant::now();
            let msg = match dtx.read_message_on(&channel).await {
                Ok(msg) => msg,
                Err(e) => {
                    println!("--- message {i} failed after {:?}: {e} ---", started.elapsed());
                    break;
                }
            };
            if msg.expects_reply {
                dtx.reply_to(&msg).await?;
            }
            let Some(data) = &msg.data else {
                println!("--- message {i} ({:?}): no data payload ---", started.elapsed());
                continue;
            };
            // 心跳(`DTTapMessagePlist` 包着的 `{k, heart/tv}`)量很大,压成一行;
            // 真实采样消息顶层是个数组(每次 tick 可能不止一条采样),不是直接
            // 一个字典——之前检测漏了这层,导致明明已经收到含 SystemCPUUsage
            // 的采样数据也没识别出来。
            let tap_msg = data.as_dictionary().and_then(|d| d.get("DTTapMessagePlist")).and_then(|v| v.as_dictionary());
            let sample_dict = data.as_dictionary().or_else(|| data.as_array()?.first()?.as_dictionary());
            if let Some(tap) = tap_msg {
                if i % 20 == 0 {
                    println!("--- message {i} ({:?}): DTTapMessagePlist {tap:?} (heartbeats compressed, showing every 20th) ---", started.elapsed());
                }
            } else if let Some(dict) = sample_dict {
                if dict.contains_key("Processes") {
                    println!("--- message {i} ({:?}): FOUND real per-process sample data! ---", started.elapsed());
                    println!("{data:#?}");
                    return Ok(());
                } else if dict.contains_key("System") || dict.contains_key("SystemCPUUsage") {
                    println!("--- message {i} ({:?}): system-level sample/header (no Processes yet) ---", started.elapsed());
                } else {
                    println!("--- message {i} ({:?}) ---\n{data:#?}", started.elapsed());
                }
            } else {
                println!("--- message {i} ({:?}) ---\n{data:#?}", started.elapsed());
            }
        }

        // dtx 借走了 stack 的可变引用,这个块结束、dtx 被丢弃之后才能拿回来
        // 调 close——发我们自己的 FIN,完成关闭握手,不要留一条对方看来还没
        // 断干净的连接占着,免得设备拒绝下一次重连。
        drop(dtx);
        stack.close(handle).await?;
        println!("=== attempt {attempt}: closed cleanly ===");
    }

    Ok(())
}
