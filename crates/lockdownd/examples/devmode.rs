#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let device = devices.first().expect("plug in a device");
    println!("connecting to {} (device_id={})", device.udid, device.device_id);

    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;

    let mut lockdown = lockdownd::LockdownClient::connect(device.device_id).await?;
    lockdown.start_session(&pairing_file).await?;
    println!("TLS session established");

    for (domain, key) in [
        (Some("com.apple.security.mac.amfi"), "DeveloperModeStatus"),
        (None, "DeveloperModeStatus"),
    ] {
        match lockdown.get_value(Some(key), domain).await {
            Ok(value) => println!("domain={domain:?} {key} = {value:?}"),
            Err(e) => println!("domain={domain:?} {key} -> error: {e}"),
        }
    }

    Ok(())
}
