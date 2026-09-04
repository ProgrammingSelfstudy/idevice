//! 完整装包流程:读本地 `.ipa` -> 走 AFC 传到设备 `/PublicStaging` -> 走
//! installation_proxy 发 `Install` 命令 -> 装完清理暂存文件。
//!
//! 用法: cargo run -p installation_proxy --example install -- /path/to/App.ipa

use std::net::Ipv6Addr;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use afc::AfcClient;
use installation_proxy::InstallationProxyClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let ipa_path = std::env::args().nth(1).expect("usage: install <path-to-ipa>");
    let ipa_bytes = std::fs::read(&ipa_path)?;
    println!("read {} bytes from {ipa_path}", ipa_bytes.len());

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
    let afc_port = rsd_info
        .service_port(afc::RSD_SERVICE_NAME)
        .expect("afc shim not advertised");
    let instproxy_port = rsd_info
        .service_port(installation_proxy::RSD_SERVICE_NAME)
        .expect("installation_proxy shim not advertised");

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let remote_path = format!("/PublicStaging/idevice-rs-native-{nanos}.ipa");

    {
        let mut afc = AfcClient::connect(&mut stack, server_addr, afc_port).await?;
        afc.makedir("/PublicStaging").await?;
        afc.write_file(&remote_path, &ipa_bytes).await?;
        afc.close().await?;
    }
    println!("uploaded to {remote_path}");

    let install_result = {
        let mut instproxy = InstallationProxyClient::connect(&mut stack, server_addr, instproxy_port).await?;
        let result = instproxy
            .send_package("Install", &remote_path, plist::Dictionary::new())
            .await;
        instproxy.close().await?;
        result
    };

    // 不管装没装成功都尝试把暂存文件清理掉——真机验证过装成功之后这个路径
    // 经常已经不是原来那个普通文件了(`installd` 处理完会动它),这里删除
    // 失败跟 pymobiledevice3 自己清理时的 `force=True` 一个道理,只记日志、
    // 不当成致命错误。
    {
        let mut afc = AfcClient::connect(&mut stack, server_addr, afc_port).await?;
        if let Err(e) = afc.remove(&remote_path).await {
            println!("cleanup of {remote_path} failed (probably already gone/replaced by installd): {e}");
        } else {
            println!("cleaned up {remote_path}");
        }
        afc.close().await?;
    }

    install_result?;
    println!("install complete");
    Ok(())
}
