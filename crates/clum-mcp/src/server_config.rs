use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,

    /// External address for clients/bridges to connect (e.g. "10.220.71.1:9788"). Required.
    pub server_addr: String,

    pub server_cert: Option<String>,
    pub server_key: Option<String>,
    pub ca_cert: Option<String>,
    pub hosts_file: Option<String>,
    pub audit_db: Option<String>,
    pub static_dir: Option<String>,
    pub recordings_dir: Option<String>,

    #[serde(default)]
    pub bridges: Vec<BridgeEntry>,

    #[serde(default = "default_token_ttl_hours")]
    pub token_ttl_hours: u64,

    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u32,

    #[serde(default = "default_audit_max_size_mb")]
    pub audit_max_size_mb: u64,

    #[serde(default = "default_audit_cleanup_interval_secs")]
    pub audit_cleanup_interval_secs: u64,

    #[serde(default = "default_audit_sync_interval_secs")]
    pub audit_sync_interval_secs: u64,

    #[serde(default = "default_recordings_retention_days")]
    pub recordings_retention_days: u32,

    #[serde(default = "default_recordings_max_size_mb")]
    pub recordings_max_size_mb: u64,

    #[serde(default)]
    pub file_transfer: FileTransferConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct FileTransferConfig {
    /// Upload bandwidth per stream in Mbps. 0 = unlimited.
    #[serde(default = "default_upload_bw")]
    pub upload_bandwidth_mbps: u64,
    /// Download bandwidth per stream in Mbps. 0 = unlimited.
    #[serde(default = "default_download_bw")]
    pub download_bandwidth_mbps: u64,
    /// Global upload bandwidth in Mbps across all streams. 0 = unlimited.
    #[serde(default)]
    pub global_upload_bandwidth_mbps: u64,
    /// Global download bandwidth in Mbps across all streams. 0 = unlimited.
    #[serde(default)]
    pub global_download_bandwidth_mbps: u64,
    /// Max concurrent file uploads per host. 0 = unlimited.
    #[serde(default = "default_upload_concurrency")]
    pub max_upload_concurrency: usize,
    /// Max concurrent file downloads per host. 0 = unlimited.
    #[serde(default)]
    pub max_download_concurrency: usize,
}

impl Default for FileTransferConfig {
    fn default() -> Self {
        Self {
            upload_bandwidth_mbps: default_upload_bw(),
            download_bandwidth_mbps: default_download_bw(),
            global_upload_bandwidth_mbps: 0,
            global_download_bandwidth_mbps: 0,
            max_upload_concurrency: default_upload_concurrency(),
            max_download_concurrency: 0,
        }
    }
}

fn default_upload_concurrency() -> usize {
    16
}

fn default_upload_bw() -> u64 {
    0
}

fn default_download_bw() -> u64 {
    0
}

#[allow(dead_code)]
impl FileTransferConfig {
    pub fn upload_config(&self) -> clum_core::rate_limiter::BandwidthConfig {
        clum_core::rate_limiter::BandwidthConfig {
            per_stream: self.upload_bandwidth_mbps * 125_000,
            global: self.global_upload_bandwidth_mbps * 125_000,
        }
    }
    pub fn download_config(&self) -> clum_core::rate_limiter::BandwidthConfig {
        clum_core::rate_limiter::BandwidthConfig {
            per_stream: self.download_bandwidth_mbps * 125_000,
            global: self.global_download_bandwidth_mbps * 125_000,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct BridgeEntry {
    pub hostname: String,
    pub token: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

fn default_listen() -> String {
    "0.0.0.0:9788".to_string()
}

fn default_token_ttl_hours() -> u64 {
    24
}

fn default_audit_retention_days() -> u32 {
    90
}

fn default_audit_max_size_mb() -> u64 {
    500
}

fn default_audit_cleanup_interval_secs() -> u64 {
    600
}

fn default_audit_sync_interval_secs() -> u64 {
    300
}

fn default_recordings_retention_days() -> u32 {
    90
}

fn default_recordings_max_size_mb() -> u64 {
    5000
}

impl ServerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read config {}: {e}", path.display()))?;
        let config: ServerConfig = serde_norway::from_str(&content)
            .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?;
        Ok(config)
    }

    pub fn bridge_token_map(&self) -> HashMap<String, String> {
        self.bridges
            .iter()
            .map(|b| (b.hostname.clone(), b.token.clone()))
            .collect()
    }

    pub fn resolve_audit_db(&self) -> PathBuf {
        expand_opt_path(&self.audit_db).unwrap_or_else(|| {
            let dir = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".clum");
            std::fs::create_dir_all(&dir).ok();
            dir.join("audit.db")
        })
    }

    pub fn resolve_recordings_dir(&self) -> Option<PathBuf> {
        expand_opt_path(&self.recordings_dir)
    }

    pub fn resolve_static_dir(&self) -> Option<PathBuf> {
        expand_opt_path(&self.static_dir)
    }
}

fn expand_opt_path(path: &Option<String>) -> Option<PathBuf> {
    path.as_ref().map(|p| {
        if let Some(rest) = p.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(rest)
        } else {
            PathBuf::from(p)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_transfer_is_unlimited() {
        let cfg = FileTransferConfig::default();
        let up = cfg.upload_config();
        let down = cfg.download_config();
        assert_eq!(up.per_stream, 0, "default upload should be unlimited");
        assert_eq!(down.per_stream, 0, "default download should be unlimited");
        assert_eq!(up.global, 0);
        assert_eq!(down.global, 0);
    }
}
