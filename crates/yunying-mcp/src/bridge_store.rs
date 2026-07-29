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
            );
            CREATE TABLE IF NOT EXISTS download_tokens (
                token_hash TEXT PRIMARY KEY,
                expires_at TEXT NOT NULL
            );",
        )?;
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

    pub async fn generate_download_token(&self) -> Result<String> {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("CSPRNG failed");
        let token = format!("dl_{}", hex::encode(bytes));
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "INSERT OR REPLACE INTO download_tokens (token_hash, expires_at) VALUES (?1, ?2)",
            rusqlite::params![hash, expires],
        )?;
        Ok(token)
    }

    pub async fn validate_download_token(&self, token: &str) -> bool {
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let db = self.db.lock().await;
        let result: Option<String> = db
            .query_row(
                "SELECT expires_at FROM download_tokens WHERE token_hash = ?1",
                rusqlite::params![hash],
                |row| row.get(0),
            )
            .ok();
        match result {
            Some(expires) => {
                if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&expires) {
                    chrono::Utc::now() < exp
                } else {
                    false
                }
            }
            None => false,
        }
    }
}

pub fn generate_bridge_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("CSPRNG failed");
    hex::encode(bytes)
}
