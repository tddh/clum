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
        let config: ServerConfig = serde_yml::from_str(&content)
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
                .join(".yunying");
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
