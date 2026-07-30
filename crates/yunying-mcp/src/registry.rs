use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

#[allow(dead_code)]
pub struct BridgeConn {
    pub conn: quinn::Connection,
    pub hostname: String,
    pub tags: Vec<String>,
    pub labels: HashMap<String, String>,
    pub capabilities: Vec<String>,
    pub version: String,
    pub machine_id: String,
    pub os_info: String,
    pub registered_at: Instant,
    pub last_heartbeat: RwLock<Instant>,
    pub control_send: tokio::sync::Mutex<quinn::SendStream>,
}

impl BridgeConn {
    #[allow(dead_code)]
    pub async fn send_control_frame(&self, msg: &serde_json::Value) -> anyhow::Result<()> {
        let mut send = self.control_send.lock().await;
        let data = serde_json::to_vec(msg)?;
        let len = (data.len() as u32).to_le_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut *send, &len).await?;
        tokio::io::AsyncWriteExt::write_all(&mut *send, &data).await?;
        Ok(())
    }
}

#[allow(dead_code)]
pub struct BridgeInfo {
    pub hostname: String,
    pub tags: Vec<String>,
    pub labels: HashMap<String, String>,
    pub version: String,
    pub os_info: String,
    pub online: bool,
    pub registered_secs_ago: u64,
}

pub struct BridgeRegistry {
    connections: RwLock<HashMap<String, Arc<BridgeConn>>>,
}

impl BridgeRegistry {
    pub fn new() -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register(&self, conn: BridgeConn) -> Result<(), String> {
        let mut map = self.connections.write().await;
        if map.contains_key(&conn.hostname) {
            return Err(format!("hostname '{}' already registered", conn.hostname));
        }
        tracing::info!(
            hostname = %conn.hostname,
            version = %conn.version,
            "bridge registered"
        );
        map.insert(conn.hostname.clone(), Arc::new(conn));
        Ok(())
    }

    pub async fn unregister(&self, hostname: &str) {
        let mut map = self.connections.write().await;
        if map.remove(hostname).is_some() {
            tracing::info!(hostname = %hostname, "bridge unregistered");
        }
    }

    #[allow(dead_code)]
    pub async fn get(&self, hostname: &str) -> Option<Arc<BridgeConn>> {
        self.connections.read().await.get(hostname).cloned()
    }

    pub async fn list(&self) -> Vec<BridgeInfo> {
        let map = self.connections.read().await;
        map.values()
            .map(|c| BridgeInfo {
                hostname: c.hostname.clone(),
                tags: c.tags.clone(),
                labels: c.labels.clone(),
                version: c.version.clone(),
                os_info: c.os_info.clone(),
                online: true,
                registered_secs_ago: c.registered_at.elapsed().as_secs(),
            })
            .collect()
    }

    pub async fn update_heartbeat(&self, hostname: &str) {
        if let Some(conn) = self.connections.read().await.get(hostname) {
            *conn.last_heartbeat.write().await = Instant::now();
        }
    }
}
