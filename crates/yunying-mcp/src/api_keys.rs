use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

#[allow(dead_code)]
pub struct AgentIdentity {
    pub name: String,
    pub key_prefix: String,
}

#[allow(dead_code)]
pub struct ApiKeyInfo {
    pub name: String,
    pub key_prefix: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    pub revoked: bool,
}

struct KeyRecord {
    name: String,
    key_hash: String,
    key_prefix: String,
    expires_at: Option<String>,
    revoked_at: Option<String>,
}

pub struct ApiKeyStore {
    db: tokio::sync::Mutex<rusqlite::Connection>,
    cache: Arc<RwLock<Vec<KeyRecord>>>,
}

impl ApiKeyStore {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("open api key db: {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_keys (
                id           TEXT PRIMARY KEY,
                name         TEXT NOT NULL,
                key_hash     TEXT NOT NULL,
                key_prefix   TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                expires_at   TEXT,
                revoked_at   TEXT,
                last_used_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);
            CREATE INDEX IF NOT EXISTS idx_api_keys_name ON api_keys(name);",
        )?;

        let cache = Arc::new(RwLock::new(load_records(&conn)));

        let store = Arc::new(Self {
            db: tokio::sync::Mutex::new(conn),
            cache: Arc::clone(&cache),
        });

        Ok(store)
    }

    pub async fn add(&self, name: &str) -> Result<String> {
        let raw = generate_key(name);
        let hash = hex::encode(Sha256::digest(raw.as_bytes()));
        let prefix = raw[..raw.len().min(16)].to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO api_keys (id, name, key_hash, key_prefix, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, name, hash, prefix, now],
        )?;
        drop(db);

        self.reload_cache().await;
        Ok(raw)
    }

    pub async fn list(&self) -> Vec<ApiKeyInfo> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare("SELECT name, key_prefix, created_at, last_used_at, expires_at, revoked_at FROM api_keys ORDER BY created_at")
            .unwrap();
        stmt.query_map([], |row| {
            Ok(ApiKeyInfo {
                name: row.get(0)?,
                key_prefix: row.get(1)?,
                created_at: row.get(2)?,
                last_used_at: row.get(3)?,
                expires_at: row.get(4)?,
                revoked: row.get::<_, Option<String>>(5)?.is_some(),
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    pub async fn rotate(&self, name: &str) -> Result<String> {
        let now = chrono::Utc::now();
        let old_expires = (now + chrono::Duration::hours(24)).to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "UPDATE api_keys SET expires_at = ?1 WHERE name = ?2 AND revoked_at IS NULL AND expires_at IS NULL",
            rusqlite::params![old_expires, name],
        )?;
        drop(db);

        self.add(name).await
    }

    pub async fn revoke(&self, name: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let db = self.db.lock().await;
        db.execute(
            "UPDATE api_keys SET revoked_at = ?1 WHERE name = ?2 AND revoked_at IS NULL",
            rusqlite::params![now, name],
        )?;
        drop(db);
        self.reload_cache().await;
        Ok(())
    }

    pub async fn validate(&self, key: &str) -> Option<AgentIdentity> {
        let hash = hex::encode(Sha256::digest(key.as_bytes()));
        let cache = self.cache.read().await;

        let record = cache.iter().find(|r| r.key_hash == hash)?;

        if record.revoked_at.is_some() {
            return None;
        }
        if let Some(expires) = &record.expires_at {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires) {
                if chrono::Utc::now() > exp {
                    return None;
                }
            }
        }

        Some(AgentIdentity {
            name: record.name.clone(),
            key_prefix: record.key_prefix.clone(),
        })
    }

    pub async fn is_empty(&self) -> bool {
        self.cache.read().await.is_empty()
    }

    async fn reload_cache(&self) {
        let db = self.db.lock().await;
        let records = load_records(&db);
        drop(db);
        *self.cache.write().await = records;
    }
}

fn load_records(conn: &rusqlite::Connection) -> Vec<KeyRecord> {
    let mut stmt = conn
        .prepare("SELECT name, key_hash, key_prefix, expires_at, revoked_at FROM api_keys")
        .unwrap();
    stmt.query_map([], |row| {
        Ok(KeyRecord {
            name: row.get(0)?,
            key_hash: row.get(1)?,
            key_prefix: row.get(2)?,
            expires_at: row.get(3)?,
            revoked_at: row.get(4)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

fn generate_key(name: &str) -> String {
    use std::fmt::Write;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("CSPRNG failed");
    let mut hex_str = String::with_capacity(64);
    for b in bytes {
        write!(hex_str, "{b:02x}").unwrap();
    }
    format!("yk_{name}_{hex_str}")
}
