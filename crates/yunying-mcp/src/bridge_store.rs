use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

#[allow(dead_code)]
pub struct BridgeEntry {
    pub hostname: String,
    pub token_prefix: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub revoked: bool,
}

pub struct HostMeta {
    pub hostname: String,
    pub group: String,
    pub tags: Vec<String>,
    pub labels: HashMap<String, String>,
}

pub struct BridgeStore {
    db: tokio::sync::Mutex<rusqlite::Connection>,
}

impl BridgeStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("open bridge db: {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bridges (
                token_hash   TEXT PRIMARY KEY,
                token_prefix TEXT NOT NULL,
                hostname     TEXT NOT NULL UNIQUE,
                tags         TEXT NOT NULL DEFAULT '[]',
                labels       TEXT NOT NULL DEFAULT '{}',
                machine_id   TEXT,
                os_info      TEXT,
                created_at   TEXT NOT NULL,
                revoked_at   TEXT
            );",
        )?;
        let _ = conn
            .execute_batch("ALTER TABLE bridges ADD COLUMN host_group TEXT NOT NULL DEFAULT '';");
        Ok(Self {
            db: tokio::sync::Mutex::new(conn),
        })
    }

    pub async fn add(&self, hostname: &str, token: &str, tags: &[String]) -> Result<()> {
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let prefix = token[..token.len().min(8)].to_string();
        let tags_json = serde_json::to_string(tags)?;
        let now = chrono::Utc::now().to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "INSERT OR REPLACE INTO bridges (token_hash, token_prefix, hostname, tags, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![hash, prefix, hostname, tags_json, now],
        )?;
        Ok(())
    }

    pub async fn remove(&self, hostname: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let db = self.db.lock().await;
        db.execute(
            "UPDATE bridges SET revoked_at = ?1 WHERE hostname = ?2 AND revoked_at IS NULL",
            rusqlite::params![now, hostname],
        )?;
        Ok(())
    }

    pub async fn join(&self, hostname: &str) -> Result<String> {
        let token = generate_bridge_token();
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let prefix = token[..8].to_string();

        let db = self.db.lock().await;
        let updated = db.execute(
            "UPDATE bridges SET token_hash = ?1, token_prefix = ?2, revoked_at = NULL WHERE hostname = ?3",
            rusqlite::params![hash, prefix, hostname],
        )?;
        if updated == 0 {
            anyhow::bail!("bridge '{hostname}' not found");
        }
        Ok(token)
    }

    pub async fn list(&self) -> Vec<BridgeEntry> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare(
                "SELECT hostname, token_prefix, tags, created_at, revoked_at FROM bridges ORDER BY created_at",
            )
            .unwrap();
        stmt.query_map([], |row| {
            let tags_json: String = row.get(2)?;
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            Ok(BridgeEntry {
                hostname: row.get(0)?,
                token_prefix: row.get(1)?,
                tags,
                created_at: row.get(3)?,
                revoked: row.get::<_, Option<String>>(4)?.is_some(),
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    pub async fn token_map(&self) -> HashMap<String, String> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare("SELECT token_hash, hostname FROM bridges WHERE revoked_at IS NULL")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    pub async fn rotate_token(&self, hostname: &str, new_token: &str) -> Result<()> {
        let new_hash = hex::encode(Sha256::digest(new_token.as_bytes()));
        let new_prefix = new_token[..new_token.len().min(8)].to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "UPDATE bridges SET token_hash = ?1, token_prefix = ?2, created_at = ?3 WHERE hostname = ?4 AND revoked_at IS NULL",
            rusqlite::params![new_hash, new_prefix, now, hostname],
        )?;
        Ok(())
    }

    pub async fn set_host_meta(
        &self,
        hostname: &str,
        group: Option<&str>,
        tags: Option<&[String]>,
        labels: Option<&HashMap<String, String>>,
    ) -> Result<bool> {
        let db = self.db.lock().await;
        let exists: bool = db
            .query_row(
                "SELECT 1 FROM bridges WHERE hostname = ?1",
                rusqlite::params![hostname],
                |_| Ok(()),
            )
            .is_ok();
        if !exists {
            return Ok(false);
        }
        if let Some(g) = group {
            db.execute(
                "UPDATE bridges SET host_group = ?1 WHERE hostname = ?2",
                rusqlite::params![g, hostname],
            )?;
        }
        if let Some(t) = tags {
            let json = serde_json::to_string(t)?;
            db.execute(
                "UPDATE bridges SET tags = ?1 WHERE hostname = ?2",
                rusqlite::params![json, hostname],
            )?;
        }
        if let Some(l) = labels {
            let json = serde_json::to_string(l)?;
            db.execute(
                "UPDATE bridges SET labels = ?1 WHERE hostname = ?2",
                rusqlite::params![json, hostname],
            )?;
        }
        Ok(true)
    }

    pub async fn get_all_host_meta(&self) -> Vec<HostMeta> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare(
                "SELECT hostname, host_group, tags, labels FROM bridges WHERE revoked_at IS NULL",
            )
            .unwrap();
        stmt.query_map([], |row| {
            let tags_json: String = row.get(2)?;
            let labels_json: String = row.get(3)?;
            Ok(HostMeta {
                hostname: row.get(0)?,
                group: row.get(1)?,
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                labels: serde_json::from_str(&labels_json).unwrap_or_default(),
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }
}

pub fn generate_bridge_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("CSPRNG failed");
    hex::encode(bytes)
}
