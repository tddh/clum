//! TLS server config loading: reads a PEM certificate chain and private key
//! from disk and builds a `rustls::ServerConfig` for the bridge listener.

use anyhow::{Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let cert_pem = std::fs::read(path)
        .with_context(|| format!("failed to read cert file: {}", path.display()))?;

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse cert PEM")?;

    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path.display());
    }

    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let key_pem = std::fs::read(path)
        .with_context(|| format!("failed to read key file: {}", path.display()))?;

    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| anyhow::anyhow!("failed to parse key PEM {}: {e}", path.display()))?;

    Ok(key)
}

/// QUIC TLS server config — reuses same certificate/key as TCP TLS.
pub fn load_quic_server_config(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> anyhow::Result<quinn::ServerConfig> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let rustls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("failed to build QUIC TLS server config: {}", e))?;

    let quic_crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(std::sync::Arc::new(rustls_config))
            .map_err(|e| anyhow::anyhow!("failed to create QUIC crypto config: {}", e))?;

    let mut server_config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(quic_crypto));
    let transport = std::sync::Arc::get_mut(&mut server_config.transport)
        .ok_or_else(|| anyhow::anyhow!("transport Arc is shared, cannot mutate"))?;
    transport.max_concurrent_bidi_streams(256u32.into());
    transport.stream_receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    transport.send_window(16 * 1024 * 1024);
    transport.receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    transport.congestion_controller_factory(std::sync::Arc::new(
        quinn::congestion::BbrConfig::default(),
    ));

    Ok(server_config)
}
