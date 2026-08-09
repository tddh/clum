//! Shared types and data structures for the clum MCP server and bridge.
//!
//! This crate provides the foundational types used across the clum ecosystem,
//! including host configuration, session/panel metadata, and audit event records.

use anyhow::Context;

pub mod quic;
pub mod rate_limiter;
pub mod types;

pub use types::*;

/// Maximum allowed JSON frame size (64 MB)
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Build a TLS root certificate store. When `ca_cert_path` is provided, load
/// the custom CA file (self-signed / private PKI). When `None`, fall back to
/// the system WebPKI roots so public-CA certificates (Let's Encrypt, etc.) are
/// trusted without a local CA file.
pub fn build_root_store(ca_cert_path: Option<&str>) -> anyhow::Result<rustls::RootCertStore> {
    match ca_cert_path {
        Some(path) => {
            let pem =
                std::fs::read(path).with_context(|| format!("failed to read CA cert: {path}"))?;
            let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_slice())
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("failed to parse CA cert PEM")?;
            let mut store = rustls::RootCertStore::empty();
            store.add_parsable_certificates(certs);
            Ok(store)
        }
        None => {
            let mut store = rustls::RootCertStore::empty();
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Ok(store)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_root_store_none_uses_webpki_roots() {
        let store = build_root_store(None).expect("should build store from webpki roots");
        assert!(!store.is_empty(), "webpki roots store should not be empty");
    }

    #[test]
    fn build_root_store_nonexistent_path_returns_error() {
        let result = build_root_store(Some("/nonexistent/path/definitely/not/here.pem"));
        assert!(result.is_err(), "nonexistent path should return error");
    }

    #[test]
    fn build_root_store_empty_nonexistent_dir_path_returns_error() {
        let result = build_root_store(Some(""));
        assert!(result.is_err(), "empty path should return error");
    }
}
