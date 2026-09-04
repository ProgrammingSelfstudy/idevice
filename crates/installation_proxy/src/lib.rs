//! `com.apple.mobile.installation_proxy`——设备上应用安装数据库的入口:装包、
//! 卸载、列已装应用。跟 `com.apple.mobile.mobile_image_mounter` 是同一类
//! `*.shim.remote` 服务,连接/握手复用 `rsd::ShimServiceConnection`,这里只
//! 管业务层的 `Command`/`ClientOptions` 请求-响应形状(参考
//! pymobiledevice3 的 `services/installation_proxy.py`)。
//!
//! `Lookup`/`CheckCapabilitiesMatch` 是一问一答;`Browse`/`Install`/
//! `Uninstall` 这些是"流式"的——一条命令换来一串响应,中间可能夹着进度
//! (`PercentComplete`)、`Browse` 专属的分页 (`CurrentList`),直到某条响应
//! 带 `Status: "Complete"` 才算完。

use std::net::Ipv6Addr;

use rsd::ShimServiceConnection;
use tokio::io::{AsyncRead, AsyncWrite};
use tunnel::TunnelStack;

pub const RSD_SERVICE_NAME: &str = "com.apple.mobile.installation_proxy.shim.remote";

#[derive(Debug, thiserror::Error)]
pub enum InstallationProxyError {
    #[error(transparent)]
    Rsd(#[from] rsd::RsdError),
    #[error("device reported an error: {0}")]
    DeviceError(String),
    #[error("response missing expected field: {0}")]
    MissingField(&'static str),
}

pub type Result<T> = std::result::Result<T, InstallationProxyError>;

pub struct InstallationProxyClient<'a, S> {
    conn: ShimServiceConnection<'a, S>,
}

impl<'a, S> InstallationProxyClient<'a, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub async fn connect(stack: &'a mut TunnelStack<S>, server_addr: Ipv6Addr, port: u16) -> Result<Self> {
        let conn = ShimServiceConnection::connect(stack, server_addr, port, "idevice-rs-native").await?;
        Ok(Self { conn })
    }

    /// 列出已装应用——`application_type` 传 `"User"`/`"System"`/`"Any"`。
    /// 响应是分页的:一直读到某条带 `Status: "Complete"` 为止,把每条的
    /// `CurrentList` 拼起来。
    pub async fn browse(&mut self, application_type: &str) -> Result<Vec<plist::Value>> {
        let mut options = plist::Dictionary::new();
        options.insert("ApplicationType".into(), application_type.into());

        let mut cmd = plist::Dictionary::new();
        cmd.insert("Command".into(), "Browse".into());
        cmd.insert("ClientOptions".into(), plist::Value::Dictionary(options));
        self.conn.send_plist(cmd).await?;

        let mut result = Vec::new();
        loop {
            let response = self.conn.recv_plist().await?;
            if let Some(plist::Value::Array(list)) = response.get("CurrentList") {
                result.extend(list.iter().cloned());
            }
            if response.get("Status").and_then(|v| v.as_string()) == Some("Complete") {
                break;
            }
        }
        Ok(result)
    }

    /// `options` 常用键:`BundleIDs`(限定查哪些)、`ApplicationType`、
    /// `ReturnAttributes`(只要哪些字段)。一问一答,结果在 `LookupResult`。
    pub async fn lookup(&mut self, options: plist::Dictionary) -> Result<plist::Value> {
        let mut cmd = plist::Dictionary::new();
        cmd.insert("Command".into(), "Lookup".into());
        cmd.insert("ClientOptions".into(), plist::Value::Dictionary(options));
        self.conn.send_plist(cmd).await?;

        let mut response = self.conn.recv_plist().await?;
        response
            .remove("LookupResult")
            .ok_or(InstallationProxyError::MissingField("LookupResult"))
    }

    /// 安装/升级一个已经通过 AFC 传到设备 `PackagePath` 位置的包——
    /// `cmd` 传 `"Install"`/`"Upgrade"`。装完之前会收到若干条带
    /// `PercentComplete` 的进度响应,最终一条带 `Status: "Complete"`。
    pub async fn send_package(&mut self, cmd: &str, package_path: &str, options: plist::Dictionary) -> Result<()> {
        let mut req = plist::Dictionary::new();
        req.insert("Command".into(), cmd.into());
        req.insert("ClientOptions".into(), plist::Value::Dictionary(options));
        req.insert("PackagePath".into(), package_path.into());
        self.conn.send_plist(req).await?;
        self.watch_completion().await
    }

    /// `cmd` 传 `"Uninstall"`/`"Archive"`/`"Restore"`——按 bundle id 而不是
    /// 按包路径操作已装应用。
    pub async fn send_cmd_for_bundle_identifier(
        &mut self,
        cmd: &str,
        bundle_identifier: &str,
        options: plist::Dictionary,
    ) -> Result<()> {
        let mut req = plist::Dictionary::new();
        req.insert("Command".into(), cmd.into());
        req.insert("ApplicationIdentifier".into(), bundle_identifier.into());
        req.insert("ClientOptions".into(), plist::Value::Dictionary(options));
        self.conn.send_plist(req).await?;
        self.watch_completion().await
    }

    async fn watch_completion(&mut self) -> Result<()> {
        loop {
            let response = self.conn.recv_plist().await?;
            if let Some(err) = response.get("Error") {
                let desc = response
                    .get("ErrorDescription")
                    .and_then(|v| v.as_string())
                    .unwrap_or("");
                return Err(InstallationProxyError::DeviceError(format!("{err:?}: {desc}")));
            }
            if let Some(pct) = response.get("PercentComplete") {
                tracing::info!(percent = ?pct, "installation progress");
            }
            if response.get("Status").and_then(|v| v.as_string()) == Some("Complete") {
                return Ok(());
            }
        }
    }

    pub async fn close(self) -> Result<()> {
        self.conn.close().await?;
        Ok(())
    }
}
