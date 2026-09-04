//! `StartSession` 之后把明文连接升级成 TLS——用配对记录里的证书做双向认证,
//! 但**不验证服务端证书**(这是故意的,不是漏掉了):设备呈上来的证书是配对时
//! 生成的自签证书,压根没有一条能验证到公共信任锚的链,标准的"验证服务端证书
//! 匹配域名/链条完整"这一套在这里根本用不上——真正的信任来自"服务端能用配对
//! 时留下的私钥完成 TLS 握手"这件事本身,域名/证书链检查在这个场景里没有意义。
//! 这个做法不是我们自己想出来的捷径,是这条协议本来的设计就是这样(参考过
//! `idevice` 的 `sni.rs` 实现确认了这一点)。

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::pem::PemObject;

use crate::LockdownError;

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
        // rustls 0.23 默认签名算法集合就够用——配对场景走的是设备自己生成的
        // 证书,不需要额外限制/放宽支持的签名算法。
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

pub(crate) fn build_client_config(
    pairing_file: &pairing::PairingFile,
) -> Result<ClientConfig, LockdownError> {
    // `install_default()` 只需要成功一次,重复调用会返回 Err(已经装过了)——
    // 用 `.ok()` 吞掉,不当错误处理。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let host_cert = CertificateDer::from_pem_slice(&pairing_file.host_certificate)
        .map_err(|e| LockdownError::Tls(format!("parse HostCertificate PEM: {e}")))?;
    let host_key = PrivateKeyDer::from_pem_slice(&pairing_file.host_private_key)
        .map_err(|e| LockdownError::Tls(format!("parse HostPrivateKey PEM: {e}")))?;
    let root_cert = CertificateDer::from_pem_slice(&pairing_file.root_certificate)
        .map_err(|e| LockdownError::Tls(format!("parse RootCertificate PEM: {e}")))?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store
        .add(root_cert)
        .map_err(|e| LockdownError::Tls(format!("add root cert to store: {e}")))?;

    // 先建一个正常的、带真实 root store 的配置——只是为了满足
    // `with_client_auth_cert` 这个 builder 状态机要求走完流程,root_store 本身
    // 马上就会被下面的 `dangerous().set_certificate_verifier(...)` 整个绕过,
    // 不会真的拿它去验证链。这是照着 idevice 的 `sni.rs` 里验证过能跑通的顺序
    // 抄的,不是自己拍的。
    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(vec![host_cert], host_key)
        .map_err(|e| LockdownError::Tls(format!("build TLS client config: {e}")))?;

    config
        .dangerous()
        .set_certificate_verifier(Arc::new(AcceptAnyServerCert));

    // 设备端最早的 iOS 版本用的是很老的 TLS(见 idevice 的 legacy 参数)——目前
    // 这次重写只在最新真机(iOS 26)上测过,不支持老设备就先不管,等真遇到再补。
    config.alpn_protocols.clear();

    Ok(config)
}
