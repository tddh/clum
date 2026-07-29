use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,

    pub server_cert: Option<String>,
    pub server_key: Option<String>,
    pub ca_cert: Option<String>,
    pub hosts_file: Option<String>,
    pub audit_db: Option<String>,

    #[serde(default)]
    pub bridges: Vec<BridgeEntry>,

    #[serde(default = "default_token_ttl_hours")]
    pub token_ttl_hours: u64,
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
    "0.0.0.0:9778".to_string()
}

fn default_token_ttl_hours() -> u64 {
    24
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

    #[allow(dead_code)]
    pub fn resolve_audit_db(&self) -> PathBuf {
        self.audit_db
            .as_ref()
            .map(|p| {
                if let Some(rest) = p.strip_prefix("~/") {
                    dirs::home_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(rest)
                } else {
                    PathBuf::from(p)
                }
            })
            .unwrap_or_else(|| {
                let dir = dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".yunying");
                std::fs::create_dir_all(&dir).ok();
                dir.join("audit.db")
            })
    }
}
