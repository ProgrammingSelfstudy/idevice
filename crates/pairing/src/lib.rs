//! 配对记录的数据结构——纯数据,不碰 TLS/网络,那些留给 `lockdownd` 用这里解析
//! 出来的字段自己去建连接。
//!
//! 字段/格式是拿真机读出来的配对记录实测过的(见 `usbmuxd` crate 的
//! `examples/probe.rs`):9 个 key 的 plist dict,`*Certificate`/`*PrivateKey`
//! 这几个字段都是 PEM 文本(不是 DER 二进制),存成 plist 的 `Data` 类型。

pub mod tls;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("failed to decode pair record plist: {0}")]
    PlistDecode(#[source] plist::Error),
    #[error("pair record missing or has wrong type for field: {0}")]
    MissingField(&'static str),
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

pub type Result<T> = std::result::Result<T, PairingError>;

#[derive(Debug, Clone)]
pub struct PairingFile {
    pub host_id: String,
    pub system_buid: String,
    /// PEM 文本
    pub host_certificate: Vec<u8>,
    /// PEM 文本
    pub host_private_key: Vec<u8>,
    /// PEM 文本
    pub root_certificate: Vec<u8>,
    /// PEM 文本
    pub device_certificate: Vec<u8>,
    pub escrow_bag: Option<Vec<u8>>,
    pub wifi_mac_address: Option<String>,
}

impl PairingFile {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let value: plist::Value = plist::from_bytes(bytes).map_err(PairingError::PlistDecode)?;
        let dict = value
            .into_dictionary()
            .ok_or(PairingError::MissingField("<root is not a dictionary>"))?;

        let string_field = |key: &'static str| -> Result<String> {
            dict.get(key)
                .and_then(|v| v.as_string())
                .map(|s| s.to_string())
                .ok_or(PairingError::MissingField(key))
        };
        let data_field = |key: &'static str| -> Result<Vec<u8>> {
            match dict.get(key) {
                Some(plist::Value::Data(d)) => Ok(d.clone()),
                _ => Err(PairingError::MissingField(key)),
            }
        };

        Ok(Self {
            host_id: string_field("HostID")?,
            system_buid: string_field("SystemBUID")?,
            host_certificate: data_field("HostCertificate")?,
            host_private_key: data_field("HostPrivateKey")?,
            root_certificate: data_field("RootCertificate")?,
            device_certificate: data_field("DeviceCertificate")?,
            escrow_bag: match dict.get("EscrowBag") {
                Some(plist::Value::Data(d)) => Some(d.clone()),
                _ => None,
            },
            wifi_mac_address: dict
                .get("WiFiMACAddress")
                .and_then(|v| v.as_string())
                .map(|s| s.to_string()),
        })
    }
}
