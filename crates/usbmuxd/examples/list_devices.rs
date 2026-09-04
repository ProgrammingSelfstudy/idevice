#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = client.list_devices().await?;
    for d in &devices {
        println!("{:?}", d);
    }
    if devices.is_empty() {
        println!("(no devices)");
    }
    Ok(())
}
