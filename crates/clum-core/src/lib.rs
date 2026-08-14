//! Shared types and data structures for the clum MCP server and bridge.
//!
//! This crate provides the foundational types used across the clum ecosystem,
//! including host configuration, session/panel metadata, and audit event records.

use anyhow::Context;
use rustls::pki_types::pem::PemObject;

pub mod backoff;
pub mod error_code;
pub mod quic;
pub mod rate_limiter;
pub mod types;

pub use types::*;

/// Maximum allowed JSON frame size (64 MB)
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// 1 MB 传输缓冲区。所有 crate 的文件传输/中继缓冲统一使用此常量，
/// 避免跨 crate 重复定义导致缓冲区边界不对齐。
pub const COPY_BUF_SIZE: usize = 1024 * 1024;

/// 输出等待类工具（wait_for_text / wait_stable / wait_exit / wait_for_bytes）的默认超时。
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;

/// collect_until_exit 的默认收集超时。
pub const DEFAULT_COLLECT_TIMEOUT_MS: u64 = 60_000;

/// 命令执行类工具（exec / batch_exec）的默认超时。
pub const DEFAULT_EXEC_TIMEOUT_MS: u64 = 600_000;

/// Build a TLS root certificate store. When `ca_cert_path` is provided, load
/// the custom CA file (self-signed / private PKI). When `None`, fall back to
/// the system WebPKI roots so public-CA certificates (Let's Encrypt, etc.) are
/// trusted without a local CA file.
pub fn build_root_store(ca_cert_path: Option<&str>) -> anyhow::Result<rustls::RootCertStore> {
    match ca_cert_path {
        Some(path) => {
            let pem =
                std::fs::read(path).with_context(|| format!("failed to read CA cert: {path}"))?;
            let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_slice_iter(&pem)
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
