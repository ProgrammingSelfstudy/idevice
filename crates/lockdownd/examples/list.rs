//! 列出当前连接的所有设备——跟 `tidevice list`/`idevice_id -l` 类似的表格
//! 输出。UDID/连接方式从 usbmuxd 直接问,不需要配对;设备名/型号/系统版本
//! 走 lockdownd 明文查询——`ProductVersion`/`ProductType`/`DeviceName` 这几个
//! 基础字段允许未配对连接直接查(见 `lockdownd` crate 顶部注释),用不着走
//! 配对 + TLS 那一整套。

use usbmuxd::Connection;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut usbmuxd = usbmuxd::UsbmuxdClient::connect().await?;
    let devices = usbmuxd.list_devices().await?;

    if devices.is_empty() {
        println!("no devices connected");
        return Ok(());
    }

    println!(
        "{:<26} {:<20} {:<14} {:<10} {:<8}",
        "UDID", "NAME", "MODEL", "VERSION", "CONN"
    );
    for device in &devices {
        let conn = match &device.connection {
            Connection::Usb => "USB".to_string(),
            Connection::Network => "Network".to_string(),
            Connection::Unknown(s) => s.clone(),
        };

        let mut lockdown = match lockdownd::LockdownClient::connect(device.device_id).await {
            Ok(l) => l,
            Err(e) => {
                println!("{:<26} <failed to connect: {e}>", device.udid);
                continue;
            }
        };

        let name = string_value(&mut lockdown, "DeviceName").await;
        let model = string_value(&mut lockdown, "ProductType").await;
        let version = string_value(&mut lockdown, "ProductVersion").await;

        println!("{:<26} {:<20} {:<14} {:<10} {:<8}", device.udid, name, model, version, conn);
    }

    Ok(())
}

async fn string_value(lockdown: &mut lockdownd::LockdownClient, key: &str) -> String {
    lockdown
        .get_value(Some(key), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(str::to_string))
        .unwrap_or_else(|| "?".to_string())
}
