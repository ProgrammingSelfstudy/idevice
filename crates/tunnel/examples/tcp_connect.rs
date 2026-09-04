use std::net::Ipv6Addr;
use std::str::FromStr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let device = devices.first().expect("plug in a device").clone();
    println!("device: {}", device.udid);

    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;

    let (info, stream) = tunnel::connect(device.device_id, &pairing_file).await?;
    println!("tunnel: {info:?}");

    let mut stack = tunnel::TunnelStack::new(stream, &info)?;
    let server_addr = Ipv6Addr::from_str(&info.server_address)?;

    println!("connecting TCP to [{server_addr}]:{}...", info.server_rsd_port);
    let handle = stack.connect(server_addr, info.server_rsd_port).await?;
    println!("TCP connected! handle={handle:?}");

    let data = stack.recv(handle).await?;
    println!("received {} bytes: {:02x?}", data.len(), &data[..data.len().min(64)]);

    Ok(())
}
