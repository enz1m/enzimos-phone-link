//! TLS support for the signaling server.
//!
//! Provides `rustls`-based TLS acceptor configuration. The server accepts
//! PEM-encoded certificate chains and private keys.

use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// TLS configuration paths.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Path to the PEM-encoded certificate chain.
    pub cert_path: String,
    /// Path to the PEM-encoded private key.
    pub key_path: String,
}

/// Load a TLS acceptor from PEM certificate and key files.
pub async fn load_acceptor(config: &TlsConfig) -> Result<TlsAcceptor, String> {
    let certs = load_certs(&config.cert_path).await?;
    let key = load_key(&config.key_path).await?;

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS configuration error: {e}"))?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

async fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let data = tokio::fs::read(Path::new(path))
        .await
        .map_err(|e| format!("could not read certificate file {path}: {e}"))?;

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("could not parse certificates from {path}: {e}"))?;

    if certs.is_empty() {
        return Err(format!("no certificates found in {path}"));
    }

    Ok(certs)
}

async fn load_key(path: &str) -> Result<PrivateKeyDer<'static>, String> {
    let data = tokio::fs::read(Path::new(path))
        .await
        .map_err(|e| format!("could not read key file {path}: {e}"))?;

    PrivateKeyDer::from_pem_slice(&data)
        .map_err(|e| format!("could not parse private key from {path}: {e}"))
}

/// SHA-256 fingerprint (lowercase hex) of the first certificate in a PEM file.
///
/// Clients pin this value at pairing time, so a self-signed host identity is
/// as trustworthy as the pairing confirmation that delivered it.
pub fn certificate_fingerprint(cert_path: &str) -> Result<String, String> {
    let data = std::fs::read(cert_path)
        .map_err(|e| format!("could not read certificate file {cert_path}: {e}"))?;
    let cert = CertificateDer::pem_slice_iter(&data)
        .next()
        .ok_or_else(|| format!("no certificates found in {cert_path}"))?
        .map_err(|e| format!("could not parse certificate from {cert_path}: {e}"))?;
    Ok(crate::identity::sha256_hex(cert.as_ref()))
}

/// Ensure a persistent self-signed host certificate exists under `dir`.
///
/// The key is created with mode 0600 and never regenerated while present:
/// changing it would invalidate every phone's pinned identity.
pub fn ensure_self_signed(dir: &Path, common_name: &str) -> Result<TlsConfig, String> {
    let cert_path = dir.join("host-cert.pem");
    let key_path = dir.join("host-key.pem");
    if !(cert_path.exists() && key_path.exists()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        let mut params = rcgen::CertificateParams::new(vec!["enzremote.local".to_string()])
            .map_err(|e| format!("certificate parameters: {e}"))?;
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            format!("enzremote {common_name}"),
        );
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(2125, 1, 1);
        let key = rcgen::KeyPair::generate().map_err(|e| format!("key generation failed: {e}"))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| format!("certificate generation failed: {e}"))?;
        crate::identity::write_private(&key_path, key.serialize_pem().as_bytes())
            .map_err(|e| format!("could not write {}: {e}", key_path.display()))?;
        crate::identity::write_private(&cert_path, cert.pem().as_bytes())
            .map_err(|e| format!("could not write {}: {e}", cert_path.display()))?;
        eprintln!("tls: generated host identity in {}", dir.display());
    }
    Ok(TlsConfig {
        cert_path: cert_path.to_string_lossy().into_owned(),
        key_path: key_path.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn self_signed_identity_is_stable_and_loadable() {
        let dir = std::env::temp_dir().join(format!("enzremote-tls-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = ensure_self_signed(&dir, "test").unwrap();
        let first = certificate_fingerprint(&config.cert_path).unwrap();
        assert_eq!(first.len(), 64);
        let again = ensure_self_signed(&dir, "test").unwrap();
        assert_eq!(certificate_fingerprint(&again.cert_path).unwrap(), first);
        assert!(load_acceptor(&config).await.is_ok());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&config.key_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
