//! Shared types and data structures for the clum MCP server and bridge.
//!
//! This crate provides the foundational types used across the clum ecosystem,
//! including host configuration, session/panel metadata, and audit event records.

use anyhow::Context;

pub mod types;

pub use types::*;

/// Maximum allowed JSON frame size (64 MB)
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Transitional env fallback: if `new_key` is unset and `legacy_key` is
/// present, mirror the value into `new_key` so clap's single-variable `env`
/// lookup keeps working. Must run before argument parsing. Remove together
/// with the legacy YUNYING_* names once the deprecation window ends.
pub fn inject_env_fallback(new_key: &str, legacy_key: &str) {
    if std::env::var_os(new_key).is_none() {
        if let Some(v) = std::env::var_os(legacy_key) {
            std::env::set_var(new_key, v);
            eprintln!("[warn] {legacy_key} is deprecated; use {new_key} instead");
        }
    }
}

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
    use super::inject_env_fallback;

    #[test]
    fn fallback_mirrors_legacy_when_new_unset() {
        // Unique keys per test run to avoid cross-test env interference.
        let (new_key, legacy_key) = ("CLUM_TEST_NEW_A", "CLUM_TEST_LEGACY_A");
        std::env::remove_var(new_key);
        std::env::set_var(legacy_key, "legacy-value");
        inject_env_fallback(new_key, legacy_key);
        assert_eq!(
            std::env::var_os(new_key).map(|v| v.into_string()),
            Some(Ok("legacy-value".to_string()))
        );
        std::env::remove_var(new_key);
        std::env::remove_var(legacy_key);
    }

    #[test]
    fn new_key_wins_over_legacy() {
        let (new_key, legacy_key) = ("CLUM_TEST_NEW_B", "CLUM_TEST_LEGACY_B");
        std::env::set_var(new_key, "new-value");
        std::env::set_var(legacy_key, "legacy-value");
        inject_env_fallback(new_key, legacy_key);
        assert_eq!(
            std::env::var_os(new_key).map(|v| v.into_string()),
            Some(Ok("new-value".to_string()))
        );
        std::env::remove_var(new_key);
        std::env::remove_var(legacy_key);
    }
}
