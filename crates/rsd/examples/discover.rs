use std::net::Ipv6Addr;
use std::str::FromStr;

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
    println!("tunnel: {info:?}");

    let mut stack = tunnel::TunnelStack::new(stream, &info)?;
    let server_addr = Ipv6Addr::from_str(&info.server_address)?;

    let rsd_info = rsd::connect_and_discover(&mut stack, &server_addr, info.server_rsd_port).await?;
    println!("RSD protocol version: {}", rsd_info.protocol_version);
    println!("RSD UUID: {}", rsd_info.uuid);
    println!("{} services advertised", rsd_info.services.len());

    for name in [
        "com.apple.instruments.dtservicehub",
        "com.apple.mobile.installation_proxy.shim.remote",
    ] {
        match rsd_info.service_port(name) {
            Some(port) => println!("  {name} -> port {port}"),
            None => println!("  {name} -> NOT FOUND"),
        }
    }

    Ok(())
}
