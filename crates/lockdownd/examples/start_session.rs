#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let device = devices.first().expect("plug in a device").clone();
    println!("device: {} (device_id={})", device.udid, device.device_id);

    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;
    println!("pairing file parsed: host_id={}", pairing_file.host_id);

    let mut lockdown = lockdownd::LockdownClient::connect(device.device_id).await?;
    lockdown.start_session(&pairing_file).await?;
    println!("TLS session established");

    // 明文查询早就通了(阶段2);现在验证 TLS 会话之后还能继续正常查值,证明
    // 升级之后的 stream 收发正常,不是握手成功了但读写坏掉。
    for key in ["ProductVersion", "WiFiAddress", "DeviceClass"] {
        let value = lockdown.get_value(Some(key), None).await?;
        println!("{key} = {value:?}");
    }

    Ok(())
}
