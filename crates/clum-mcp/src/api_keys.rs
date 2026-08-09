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

#[cfg(test)]
impl ApiKeyStore {
    /// Test-only constructor: creates an in-memory SQLite database with the
    /// full api_keys schema, matching what `open()` creates.
    fn open_in_memory() -> Self {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
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
        )
        .unwrap();
        Self {
            db: tokio::sync::Mutex::new(conn),
            cache: RwLock::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: run an async closure synchronously using a new single-threaded runtime.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    // ── add + list roundtrip ─────────────────────────────────────────

    #[test]
    fn test_add_then_list_roundtrip() {
        let store = ApiKeyStore::open_in_memory();
        let raw_key = block_on(store.add("test-agent", None)).unwrap();
        assert!(raw_key.starts_with("yk_test-agent_"));

        let entries = block_on(store.list());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "test-agent");
        assert!(!entries[0].revoked);
    }

    #[test]
    fn test_add_with_group_then_list_shows_group() {
        let store = ApiKeyStore::open_in_memory();
        block_on(store.add("agent-g", Some("prod"))).unwrap();

        let entries = block_on(store.list());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].group.as_deref(), Some("prod"));
    }

    // ── validate ─────────────────────────────────────────────────────

    #[test]
    fn test_validate_valid_key_returns_some() {
        let store = ApiKeyStore::open_in_memory();
        let raw_key = block_on(store.add("alice", None)).unwrap();

        let identity = block_on(store.validate(&raw_key));
        assert!(identity.is_some());
        let id = identity.unwrap();
        assert_eq!(id.name, "alice");
        assert!(id.group.is_none());
    }

    #[test]
    fn test_validate_invalid_key_returns_none() {
        let store = ApiKeyStore::open_in_memory();
        block_on(store.add("alice", None)).unwrap();

        let identity = block_on(store.validate("yk_nonexistent_deadbeef"));
        assert!(identity.is_none());
    }

    // ── validate with cache ──────────────────────────────────────────

    #[test]
    fn test_validate_uses_cache_on_repeated_calls() {
        let store = ApiKeyStore::open_in_memory();
        let raw_key = block_on(store.add("cached-agent", None)).unwrap();

        // First call: misses cache, hits DB
        let id1 = block_on(store.validate(&raw_key));
        assert!(id1.is_some());
        assert_eq!(id1.unwrap().name, "cached-agent");

        // Second call: should use cache
        let id2 = block_on(store.validate(&raw_key));
        assert!(id2.is_some());
        assert_eq!(id2.unwrap().name, "cached-agent");
    }

    // ── group ────────────────────────────────────────────────────────

    #[test]
    fn test_validate_key_with_group_returns_correct_group() {
        let store = ApiKeyStore::open_in_memory();
        let raw_key = block_on(store.add("grouped-agent", Some("staging"))).unwrap();

        let identity = block_on(store.validate(&raw_key));
        assert!(identity.is_some());
        let id = identity.unwrap();
        assert_eq!(id.name, "grouped-agent");
        assert_eq!(id.group.as_deref(), Some("staging"));
    }

    #[test]
    fn test_validate_key_without_group_returns_none_group() {
        let store = ApiKeyStore::open_in_memory();
        let raw_key = block_on(store.add("solo-agent", None)).unwrap();

        let identity = block_on(store.validate(&raw_key));
        assert!(identity.is_some());
        let id = identity.unwrap();
        assert_eq!(id.name, "solo-agent");
        assert!(id.group.is_none());
    }

    // ── rotate ───────────────────────────────────────────────────────

    #[test]
    fn test_rotate_old_key_still_valid_in_grace_period() {
        let store = ApiKeyStore::open_in_memory();
        let old_key = block_on(store.add("rotatable", Some("prod"))).unwrap();

        let new_key = block_on(store.rotate("rotatable")).unwrap();
        assert!(!new_key.is_empty());
        assert_ne!(old_key, new_key);

        // Old key should still validate (24h grace period)
        let old_id = block_on(store.validate(&old_key));
        assert!(
            old_id.is_some(),
            "old key should be valid during grace period"
        );

        // New key should also validate
        let new_id = block_on(store.validate(&new_key));
        assert!(new_id.is_some());
        assert_eq!(new_id.unwrap().group.as_deref(), Some("prod"));
    }

    #[test]
    fn test_rotate_nonexistent_name_does_not_panic() {
        let store = ApiKeyStore::open_in_memory();
        let result = block_on(store.rotate("ghost"));
        assert!(
            result.is_ok(),
            "rotate on non-existent name should not panic"
        );
        // It creates a new key for the non-existent name
        let new_key = result.unwrap();
        let identity = block_on(store.validate(&new_key));
        assert!(identity.is_some());
        assert_eq!(identity.unwrap().name, "ghost");
    }

    // ── revoke ───────────────────────────────────────────────────────

    #[test]
    fn test_revoke_then_validate_returns_none() {
        let store = ApiKeyStore::open_in_memory();
        let raw_key = block_on(store.add("doomed", None)).unwrap();

        // Verify valid before revoke
        assert!(block_on(store.validate(&raw_key)).is_some());

        block_on(store.revoke("doomed")).unwrap();

        // After revoke, validate returns None
        let id = block_on(store.validate(&raw_key));
        assert!(id.is_none(), "revoked key should fail validation");
    }

    #[test]
    fn test_revoke_then_list_shows_revoked() {
        let store = ApiKeyStore::open_in_memory();
        block_on(store.add("flagged", None)).unwrap();

        block_on(store.revoke("flagged")).unwrap();

        let entries = block_on(store.list());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].revoked);
    }

    // ── is_empty ─────────────────────────────────────────────────────

    #[test]
    fn test_is_empty_on_empty_db_returns_true() {
        let store = ApiKeyStore::open_in_memory();
        assert!(block_on(store.is_empty()));
    }

    #[test]
    fn test_is_empty_after_add_returns_false() {
        let store = ApiKeyStore::open_in_memory();
        block_on(store.add("someone", None)).unwrap();
        assert!(!block_on(store.is_empty()));
    }

    // ── empty db list ────────────────────────────────────────────────

    #[test]
    fn test_list_empty_db_returns_empty_vec() {
        let store = ApiKeyStore::open_in_memory();
        let entries = block_on(store.list());
        assert!(entries.is_empty());
    }

    // ── multiple keys ────────────────────────────────────────────────

    #[test]
    fn test_add_multiple_keys_list_returns_all() {
        let store = ApiKeyStore::open_in_memory();
        let k1 = block_on(store.add("alpha", None)).unwrap();
        let k2 = block_on(store.add("beta", Some("dev"))).unwrap();
        assert!(!k1.is_empty());
        assert!(!k2.is_empty());

        let entries = block_on(store.list());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn test_validate_returns_correct_identity_for_multiple_keys() {
        let store = ApiKeyStore::open_in_memory();
        let k1 = block_on(store.add("one", Some("g1"))).unwrap();
        let k2 = block_on(store.add("two", Some("g2"))).unwrap();

        let id1 = block_on(store.validate(&k1)).unwrap();
        assert_eq!(id1.name, "one");
        assert_eq!(id1.group.as_deref(), Some("g1"));

        let id2 = block_on(store.validate(&k2)).unwrap();
        assert_eq!(id2.name, "two");
        assert_eq!(id2.group.as_deref(), Some("g2"));
    }
}
