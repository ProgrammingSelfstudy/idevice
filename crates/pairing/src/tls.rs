//! 用配对记录里的证书把一条明文连接升级成 TLS——`lockdownd::StartSession` 和
//! `tunnel` 建 `CoreDeviceProxy` 连接都要用到,放在这里两边共用,不重复写。
//!
//! **不验证服务端证书**(故意的,不是漏掉了):设备呈上来的证书是配对时生成的
//! 自签证书,没有一条能验证到公共信任锚的链,"验证证书链/域名匹配"这一套在这
//! 个场景里没有意义——真正的信任来自"对端能用配对时留下的私钥完成 TLS 握手"
//! 这件事本身。这不是我们自己想的捷径,是这条协议原本的设计(参考过 `idevice`
//! 的 `sni.rs` 确认过这个结论)。

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::pem::PemObject;

use crate::{PairingError, PairingFile};

#[derive(Debug)]
struct AcceptAnyServerCert;

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

pub fn build_client_config(pairing_file: &PairingFile) -> Result<ClientConfig, PairingError> {
    // `install_default()` 只需要成功一次,重复调用会返回 Err(已经装过了)——
    // 用 `let _ =` 吞掉,不当错误处理。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let host_cert = CertificateDer::from_pem_slice(&pairing_file.host_certificate)
        .map_err(|e| PairingError::Tls(format!("parse HostCertificate PEM: {e}")))?;
    let host_key = PrivateKeyDer::from_pem_slice(&pairing_file.host_private_key)
        .map_err(|e| PairingError::Tls(format!("parse HostPrivateKey PEM: {e}")))?;
    let root_cert = CertificateDer::from_pem_slice(&pairing_file.root_certificate)
        .map_err(|e| PairingError::Tls(format!("parse RootCertificate PEM: {e}")))?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store
        .add(root_cert)
        .map_err(|e| PairingError::Tls(format!("add root cert to store: {e}")))?;

    // 先建一个正常的、带真实 root store 的配置——只是为了满足
    // `with_client_auth_cert` 这个 builder 状态机要求走完流程,root_store 本身
    // 马上就会被下面的 `dangerous().set_certificate_verifier(...)` 整个绕过。
    // 顺序照着 idevice 的 `sni.rs` 里验证过能跑通的抄。
    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(vec![host_cert], host_key)
        .map_err(|e| PairingError::Tls(format!("build TLS client config: {e}")))?;

    config
        .dangerous()
        .set_certificate_verifier(Arc::new(AcceptAnyServerCert));
    config.alpn_protocols.clear();

    Ok(config)
}
