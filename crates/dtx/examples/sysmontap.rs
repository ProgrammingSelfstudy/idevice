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
    let device = devices.first().expect("plug in a device").clone();
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

    let handle = stack.connect(server_addr, dtx_port).await?;
    let mut dtx = DtxClient::new(&mut stack, handle);

    let channel = dtx
        .make_channel("com.apple.instruments.server.services.sysmontap")
        .await?;
    println!("sysmontap channel opened");

    // setConfig: 参数是一个 NSKeyedArchiver 归档的字典——跟真机联调时
    // idevice 版本用的字段名一致(`ur`=采样间隔毫秒,`procAttrs`/`sysAttrs`=
    // 要哪些字段,`cpuUsage`/`physFootprint`=开关,`sampleInterval`=纳秒)。
    let mut config = plist::Dictionary::new();
    config.insert("ur".into(), plist::Value::Integer(1000i64.into()));
    config.insert("bm".into(), plist::Value::Integer(0i64.into()));
    config.insert(
        "procAttrs".into(),
        plist::Value::Array(vec![
            plist::Value::String("pid".into()),
            plist::Value::String("name".into()),
        ]),
    );
    config.insert("sysAttrs".into(), plist::Value::Array(vec![]));
    config.insert("cpuUsage".into(), plist::Value::Boolean(true));
    config.insert("physFootprint".into(), plist::Value::Boolean(true));
    config.insert("sampleInterval".into(), plist::Value::Integer(1_000_000_000i64.into()));

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
    for i in 0..20 {
        let started = std::time::Instant::now();
        let msg = match dtx.read_message_on(&channel).await {
            Ok(msg) => msg,
            Err(e) => {
                println!("--- message {i} failed after {:?}: {e} ---", started.elapsed());
                break;
            }
        };
        println!(
            "--- message {i} (waited {:?}) identifier={} conversation_index={} channel={} expects_reply={} ---",
            started.elapsed(),
            msg.identifier,
            msg.conversation_index,
            msg.channel,
            msg.expects_reply,
        );
        if msg.expects_reply {
            dtx.reply_to(&msg).await?;
            println!("(replied)");
        }
        let Some(data) = &msg.data else {
            println!("(no data payload)");
            continue;
        };
        println!("{data:#?}");
        if let Some(dict) = data.as_dictionary()
            && (dict.contains_key("Processes") || dict.contains_key("System") || dict.contains_key("SystemCPUUsage"))
        {
            println!("FOUND real sysmontap sample data!");
            break;
        }
    }

    Ok(())
}
