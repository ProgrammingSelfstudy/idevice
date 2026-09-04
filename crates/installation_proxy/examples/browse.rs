use std::net::Ipv6Addr;
use std::str::FromStr;

use installation_proxy::InstallationProxyClient;

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
        .service_port(installation_proxy::RSD_SERVICE_NAME)
        .expect("installation_proxy shim not advertised");

    let mut client = InstallationProxyClient::connect(&mut stack, server_addr, port).await?;
    println!("connected, checked in");

    let apps = client.browse("User").await?;
    println!("found {} user app(s)", apps.len());
    for app in apps.iter().take(10) {
        let bundle_id = app
            .as_dictionary()
            .and_then(|d| d.get("CFBundleIdentifier"))
            .and_then(|v| v.as_string())
            .unwrap_or("<unknown>");
        println!("  {bundle_id}");
    }

    client.close().await?;
    Ok(())
}
