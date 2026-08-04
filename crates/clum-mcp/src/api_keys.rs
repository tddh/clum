use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

const CACHE_TTL: Duration = Duration::from_secs(600);

#[allow(dead_code)]
pub struct AgentIdentity {
    pub name: String,
    pub key_prefix: String,
    pub group: Option<String>,
}

#[allow(dead_code)]
pub struct ApiKeyInfo {
    pub name: String,
    pub key_prefix: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    pub revoked: bool,
    pub group: Option<String>,
}

#[derive(Clone)]
struct KeyRecord {
    name: String,
    key_prefix: String,
    expires_at: Option<String>,
    revoked_at: Option<String>,
    group: Option<String>,
}

struct CacheEntry {
    record: KeyRecord,
    cached_at: Instant,
}

pub struct ApiKeyStore {
    db: tokio::sync::Mutex<rusqlite::Connection>,
    cache: RwLock<HashMap<String, CacheEntry>>,
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
                last_used_at TEXT,
                group_name   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);
            CREATE INDEX IF NOT EXISTS idx_api_keys_name ON api_keys(name);",
        )?;
        // Migration: add group_name to pre-existing databases
        let _ = conn.execute_batch("ALTER TABLE api_keys ADD COLUMN group_name TEXT;");

        let store = Arc::new(Self {
            db: tokio::sync::Mutex::new(conn),
            cache: RwLock::new(HashMap::new()),
        });

        Ok(store)
    }

    pub async fn add(&self, name: &str, group: Option<&str>) -> Result<String> {
        let raw = generate_key(name);
        let hash = hex::encode(Sha256::digest(raw.as_bytes()));
        let prefix = raw[..raw.len().min(16)].to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO api_keys (id, name, key_hash, key_prefix, created_at, group_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, name, hash, prefix, now, group],
        )?;
        drop(db);

        let mut cache = self.cache.write().await;
        cache.retain(|_, e| e.cached_at.elapsed() <= CACHE_TTL);
        cache.insert(
            hash.clone(),
            CacheEntry {
                record: KeyRecord {
                    name: name.to_string(),
                    key_prefix: prefix,
                    expires_at: None,
                    revoked_at: None,
                    group: group.map(|g| g.to_string()),
                },
                cached_at: Instant::now(),
            },
        );

        Ok(raw)
    }

    pub async fn list(&self) -> Vec<ApiKeyInfo> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare("SELECT name, key_prefix, created_at, last_used_at, expires_at, revoked_at, group_name FROM api_keys ORDER BY created_at")
            .unwrap();
        stmt.query_map([], |row| {
            Ok(ApiKeyInfo {
                name: row.get(0)?,
                key_prefix: row.get(1)?,
                created_at: row.get(2)?,
                last_used_at: row.get(3)?,
                expires_at: row.get(4)?,
                revoked: row.get::<_, Option<String>>(5)?.is_some(),
                group: row.get(6)?,
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
        let group: Option<String> = db
            .query_row(
                "SELECT group_name FROM api_keys WHERE name = ?1 AND revoked_at IS NULL",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        db.execute(
            "UPDATE api_keys SET expires_at = ?1 WHERE name = ?2 AND revoked_at IS NULL AND expires_at IS NULL",
            rusqlite::params![old_expires, name],
        )?;
        drop(db);

        let mut cache = self.cache.write().await;
        cache.retain(|_, e| e.record.name != name);
        drop(cache);

        self.add(name, group.as_deref()).await
    }

    pub async fn revoke(&self, name: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let db = self.db.lock().await;
        db.execute(
            "UPDATE api_keys SET revoked_at = ?1 WHERE name = ?2 AND revoked_at IS NULL",
            rusqlite::params![now, name],
        )?;
        drop(db);

        let mut cache = self.cache.write().await;
        cache.retain(|_, e| e.record.name != name);
        Ok(())
    }

    pub async fn validate(&self, key: &str) -> Option<AgentIdentity> {
        let hash = hex::encode(Sha256::digest(key.as_bytes()));

        let cached = {
            let cache = self.cache.read().await;
            cache
                .get(&hash)
                .filter(|e| e.cached_at.elapsed() <= CACHE_TTL)
                .map(|e| e.record.clone())
        };

        let record = match cached {
            Some(r) => r,
            None => {
                let record = self.fetch_by_hash(&hash).await?;
                let mut cache = self.cache.write().await;
                cache.retain(|_, e| e.cached_at.elapsed() <= CACHE_TTL);
                cache.insert(
                    hash,
                    CacheEntry {
                        record: record.clone(),
                        cached_at: Instant::now(),
                    },
                );
                record
            }
        };

        // Records loaded from the db are not trusted blindly: a key may
        // already be revoked or past its expires_at, so always re-check.
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
            group: record.group.clone(),
        })
    }

    pub async fn is_empty(&self) -> bool {
        let db = self.db.lock().await;
        db.query_row("SELECT EXISTS(SELECT 1 FROM api_keys)", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|c| c == 0)
        .unwrap_or(true)
    }

    async fn fetch_by_hash(&self, hash: &str) -> Option<KeyRecord> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare(
                "SELECT name, key_prefix, expires_at, revoked_at, group_name \
                 FROM api_keys WHERE key_hash = ?1",
            )
            .ok()?;
        stmt.query_row(rusqlite::params![hash], |row| {
            Ok(KeyRecord {
                name: row.get(0)?,
                key_prefix: row.get(1)?,
                expires_at: row.get(2)?,
                revoked_at: row.get(3)?,
                group: row.get(4)?,
            })
        })
        .ok()
    }
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
