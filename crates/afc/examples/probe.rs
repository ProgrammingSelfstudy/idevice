use std::net::Ipv6Addr;
use std::str::FromStr;

use afc::AfcClient;

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
    let port = rsd_info
        .service_port(afc::RSD_SERVICE_NAME)
        .expect("afc shim not advertised");

    let mut client = AfcClient::connect(&mut stack, server_addr, port).await?;
    println!("connected, checked in");

    let dev_info = client.get_device_info().await?;
    println!("device info: {dev_info:#?}");

    let root = client.listdir("/").await?;
    println!("/ -> {root:?}");

    client.makedir("/PublicStaging").await?;
    println!("makedir /PublicStaging: ok (or already existed)");

    let test_path = "/PublicStaging/idevice-rs-native-probe.txt";
    let payload = b"hello from idevice-rs-native afc probe\n".to_vec();
    client.write_file(test_path, &payload).await?;
    println!("wrote {} bytes to {test_path}", payload.len());

    let stat = client.stat(test_path).await?;
    println!("stat: {stat:#?}");

    let read_back = client.read_file(test_path).await?;
    println!("read back {} bytes, matches: {}", read_back.len(), read_back == payload);

    client.remove(test_path).await?;
    println!("removed {test_path}");

    client.close().await?;
    Ok(())
}
