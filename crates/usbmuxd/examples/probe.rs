#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = client.list_devices().await?;
    let udid = devices.first().expect("plug in a device").udid.clone();
    println!("udid = {udid}");

    let buid = client.read_buid().await?;
    println!("buid = {buid}");

    let pair_record = client.read_pair_record(&udid).await?;
    println!("pair record: {} bytes", pair_record.len());
    let value: plist::Value = plist::from_bytes(&pair_record)?;
    if let Some(dict) = value.as_dictionary() {
        let keys: Vec<_> = dict.keys().collect();
        println!("pair record keys: {keys:?}");
    }

    Ok(())
}
