#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;
    let device = devices.first().expect("plug in a device").clone();
    println!("device: {} (device_id={})", device.udid, device.device_id);

    let pair_record_bytes = usbmuxd.read_pair_record(&device.udid).await?;
    let pairing_file = pairing::PairingFile::from_bytes(&pair_record_bytes)?;

    let (info, mut stream) = tunnel::connect(device.device_id, &pairing_file).await?;
    println!("tunnel established: {info:?}");

    // 握手之后收一个包证明隧道真的活着(设备侧通常会主动发点什么,比如
    // Router Advertisement 之类的邻居发现流量)。
    let packet = tunnel::recv_packet(&mut stream).await?;
    println!("received a raw packet: {} bytes, first bytes: {:02x?}", packet.len(), &packet[..packet.len().min(16)]);

    Ok(())
}
