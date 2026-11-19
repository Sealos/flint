//! A throwaway CA and server certificate, generated per server, so tests need no committed key
//! and the kubeconfig can trust exactly this server.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
#[cfg(test)]
use rustls::pki_types::CertificateDer;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

/// The fake server's TLS material.
pub struct Tls {
    /// The CA certificate (PEM) to trust; it signed the server certificate.
    pub ca_pem: String,
    #[cfg(test)]
    pub ca_der: CertificateDer<'static>,
    pub server_config: Arc<rustls::ServerConfig>,
}

/// Generates a CA and a certificate for `localhost` and `127.0.0.1` signed by it.
pub fn generate() -> Result<Tls> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "flint-e2e fake API server CA");
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate()?)?;

    let mut params = CertificateParams::new(vec!["localhost".to_owned()])?;
    params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    params
        .distinguished_name
        .push(DnType::CommonName, "flint-e2e fake API server");
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &ca)?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone(), ca.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .context("server certificate")?;
    // kube speaks HTTP/1.1 only.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Tls {
        ca_pem: ca.pem(),
        #[cfg(test)]
        ca_der: ca.der().clone(),
        server_config: Arc::new(config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    async fn handshake(tls: &Tls, name: ServerName<'static>) -> std::io::Result<String> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(tls.ca_der.clone()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let (a, b) = tokio::io::duplex(4096);
        let acceptor = tokio_rustls::TlsAcceptor::from(tls.server_config.clone());
        let server = tokio::spawn(async move {
            let mut stream = acceptor.accept(b).await?;
            stream.write_all(b"hello").await?;
            stream.shutdown().await
        });
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let mut stream = connector.connect(name, a).await?;
        let mut text = String::new();
        stream.read_to_string(&mut text).await?;
        server.await.unwrap()?;
        Ok(text)
    }

    #[tokio::test]
    async fn clients_trusting_the_ca_accept_localhost_and_the_loopback_ip() {
        let tls = generate().unwrap();
        assert!(tls.ca_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        let ip = ServerName::IpAddress(Ipv4Addr::LOCALHOST.into());
        assert_eq!(handshake(&tls, ip).await.unwrap(), "hello");
        let host = ServerName::try_from("localhost").unwrap();
        assert_eq!(handshake(&tls, host).await.unwrap(), "hello");
        let other = ServerName::try_from("example.com").unwrap();
        assert!(handshake(&tls, other).await.is_err());
    }

    #[tokio::test]
    async fn every_server_gets_its_own_ca() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a.ca_pem, b.ca_pem);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(a.ca_der.clone()).unwrap();
        let ip = ServerName::IpAddress(Ipv4Addr::LOCALHOST.into());
        // b's certificate is not trusted by a client of a.
        let tls = Tls {
            ca_pem: a.ca_pem.clone(),
            ca_der: a.ca_der.clone(),
            server_config: b.server_config.clone(),
        };
        assert!(handshake(&tls, ip).await.is_err());
    }
}
