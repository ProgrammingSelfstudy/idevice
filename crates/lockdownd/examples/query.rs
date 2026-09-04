#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let device = devices.first().expect("plug in a device");
    println!("connecting to {} (device_id={})", device.udid, device.device_id);

    let mut lockdown = lockdownd::LockdownClient::connect(device.device_id).await?;

    let query_type = lockdown.query_type().await?;
    println!("QueryType = {query_type}");

    for key in ["ProductVersion", "ProductType", "DeviceName", "UniqueDeviceID"] {
        let value = lockdown.get_value(Some(key), None).await?;
        println!("{key} = {value:?}");
    }

    Ok(())
}
