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
        let _ = conn.execute_batch("ALTER TABLE bridges ADD COLUMN previous_token_hash TEXT;");
        let _ = conn.execute_batch("ALTER TABLE bridges ADD COLUMN rotated_at TEXT;");
        Ok(Self {
            db: tokio::sync::Mutex::new(conn),
        })
    }

    pub async fn add(
        &self,
        hostname: &str,
        token: &str,
        tags: &[String],
        group: Option<&str>,
    ) -> Result<()> {
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let prefix = token[..token.len().min(8)].to_string();
        let tags_json = serde_json::to_string(tags)?;
        let now = chrono::Utc::now().to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "INSERT OR REPLACE INTO bridges (token_hash, token_prefix, hostname, tags, created_at, host_group)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![hash, prefix, hostname, tags_json, now, group.unwrap_or("")],
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
        let mut stmt = match db.prepare(
            "SELECT hostname, token_prefix, tags, created_at, revoked_at FROM bridges ORDER BY created_at",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("bridge_store list prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map([], |row| {
            let tags_json: String = row.get(2)?;
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            Ok(BridgeEntry {
                hostname: row.get(0)?,
                token_prefix: row.get(1)?,
                tags,
                created_at: row.get(3)?,
                revoked: row.get::<_, Option<String>>(4)?.is_some(),
            })
        }) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("bridge_store list query failed: {e}");
                return Vec::new();
            }
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    pub async fn token_map(&self) -> HashMap<String, String> {
        let db = self.db.lock().await;
        let mut map: HashMap<String, String> = HashMap::new();

        let mut stmt =
            match db.prepare("SELECT token_hash, hostname FROM bridges WHERE revoked_at IS NULL") {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("bridge_store token_map prepare failed: {e}");
                    return map;
                }
            };
        let rows = match stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("bridge_store token_map query failed: {e}");
                return map;
            }
        };
        for (hash, host) in rows.filter_map(|r| r.ok()) {
            map.insert(hash, host);
        }

        // Include previous tokens within 12h grace period
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(12)).to_rfc3339();
        let mut prev_stmt = match db.prepare(
            "SELECT previous_token_hash, hostname FROM bridges
             WHERE revoked_at IS NULL AND previous_token_hash IS NOT NULL AND rotated_at > ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("bridge_store token_map (previous) prepare failed: {e}");
                return map;
            }
        };
        let prev_rows = match prev_stmt.query_map(rusqlite::params![cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?))
        }) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("bridge_store token_map (previous) query failed: {e}");
                return map;
            }
        };
        for (hash, host) in prev_rows.filter_map(|r| r.ok()) {
            map.entry(hash).or_insert(host);
        }

        map
    }

    pub async fn validate_token(&self, raw_token: &str) -> bool {
        let hash = hex::encode(Sha256::digest(raw_token.as_bytes()));
        let db = self.db.lock().await;
        let current = db
            .query_row(
                "SELECT 1 FROM bridges WHERE token_hash = ?1 AND revoked_at IS NULL",
                rusqlite::params![hash],
                |_| Ok(()),
            )
            .is_ok();
        if current {
            return true;
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(12)).to_rfc3339();
        db.query_row(
            "SELECT 1 FROM bridges WHERE previous_token_hash = ?1 AND revoked_at IS NULL AND rotated_at > ?2",
            rusqlite::params![hash, cutoff],
            |_| Ok(()),
        )
        .is_ok()
    }

    pub async fn rotate_token(&self, hostname: &str, new_token: &str) -> Result<()> {
        let new_hash = hex::encode(Sha256::digest(new_token.as_bytes()));
        let new_prefix = new_token[..new_token.len().min(8)].to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let db = self.db.lock().await;
        db.execute(
            "UPDATE bridges SET previous_token_hash = token_hash, rotated_at = ?1,
             token_hash = ?2, token_prefix = ?3, created_at = ?1
             WHERE hostname = ?4 AND revoked_at IS NULL",
            rusqlite::params![now, new_hash, new_prefix, hostname],
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
        let mut stmt = match db.prepare(
            "SELECT hostname, host_group, tags, labels FROM bridges WHERE revoked_at IS NULL",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("bridge_store get_all_host_meta prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map([], |row| {
            let tags_json: String = row.get(2)?;
            let labels_json: String = row.get(3)?;
            Ok(HostMeta {
                hostname: row.get(0)?,
                group: row.get(1)?,
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                labels: serde_json::from_str(&labels_json).unwrap_or_default(),
            })
        }) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("bridge_store get_all_host_meta query failed: {e}");
                return Vec::new();
            }
        };
        rows.filter_map(|r| r.ok()).collect()
    }
}

pub fn generate_bridge_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("CSPRNG failed");
    hex::encode(bytes)
}

#[cfg(test)]
impl BridgeStore {
    /// Test-only constructor: wraps an existing rusqlite::Connection directly,
    /// bypassing the schema creation in `open()`. Each test is responsible for
    /// setting up the schema (or not) depending on what it needs to verify.
    fn from_conn(conn: rusqlite::Connection) -> Self {
        Self {
            db: tokio::sync::Mutex::new(conn),
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

    /// Create the full bridges table schema (matching `BridgeStore::open`),
    /// used by tests that need a real table to operate on.
    fn create_test_schema(conn: &rusqlite::Connection) {
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
                revoked_at   TEXT,
                host_group   TEXT NOT NULL DEFAULT '',
                previous_token_hash TEXT,
                rotated_at   TEXT
            );",
        )
        .unwrap();
    }

    // ── Empty DB / error-fallback tests ──────────────────────────────

    #[test]
    fn list_empty_db_returns_empty_vec() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let store = BridgeStore::from_conn(conn);
        let result = block_on(store.list());
        assert!(
            result.is_empty(),
            "expected empty Vec for fresh in-memory db"
        );
    }

    #[test]
    fn token_map_empty_db_returns_empty_hashmap() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let store = BridgeStore::from_conn(conn);
        let result = block_on(store.token_map());
        assert!(
            result.is_empty(),
            "expected empty HashMap for fresh in-memory db"
        );
    }

    #[test]
    fn get_all_host_meta_empty_db_returns_empty_vec() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let store = BridgeStore::from_conn(conn);
        let result = block_on(store.get_all_host_meta());
        assert!(
            result.is_empty(),
            "expected empty Vec for fresh in-memory db"
        );
    }

    // ── Behaviour tests (require schema) ─────────────────────────────

    #[test]
    fn add_then_list_finds_record() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add(
            "test-host",
            "test-token-longenough",
            &["tag1".into(), "tag2".into()],
            Some("prod"),
        ))
        .unwrap();

        let entries = block_on(store.list());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hostname, "test-host");
        assert!(!entries[0].revoked);
    }

    #[test]
    fn validate_token_invalid_token_returns_false() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        let valid = block_on(store.validate_token("nonexistent-token-1234"));
        assert!(!valid, "unknown token should return false");
    }

    #[test]
    fn remove_then_list_shows_revoked_flag() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add("test-host", "test-token-longenough", &[], None)).unwrap();
        block_on(store.remove("test-host")).unwrap();

        let entries = block_on(store.list());
        assert_eq!(
            entries.len(),
            1,
            "list() still returns the entry after revoke"
        );
        assert!(entries[0].revoked, "revoked flag should be true");
    }

    // ── add without group ────────────────────────────────────────────

    #[test]
    fn test_add_without_group() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add("test-host", "sometoken1234567890abc", &[], None)).unwrap();

        let metas = block_on(store.get_all_host_meta());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].hostname, "test-host");
        assert_eq!(metas[0].group, "");
    }

    // ── validate token (positive case) ───────────────────────────────

    #[test]
    fn test_validate_token_valid() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add("test-host", "valid-token-1234567890", &[], None)).unwrap();

        let valid = block_on(store.validate_token("valid-token-1234567890"));
        assert!(valid, "token that was just added should validate true");
    }

    // ── rotate then old token stays valid (12h grace) ────────────────

    #[test]
    fn test_rotate_token_old_still_valid() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add("test-host", "oldtoken1234567890abcdef", &[], None)).unwrap();
        block_on(store.rotate_token("test-host", "newtoken1234567890abcdef")).unwrap();

        let old_valid = block_on(store.validate_token("oldtoken1234567890abcdef"));
        let new_valid = block_on(store.validate_token("newtoken1234567890abcdef"));
        assert!(
            old_valid,
            "old token should still be valid within 12h grace period"
        );
        assert!(new_valid, "new token should be valid");
    }

    // ── join generates new token, old becomes invalid ────────────────

    #[test]
    fn test_join_generates_new_token() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add("test-host", "oldtoken1234567890abcdef", &[], None)).unwrap();
        let new_token = block_on(store.join("test-host")).unwrap();

        assert_ne!(
            new_token, "oldtoken1234567890abcdef",
            "join should return a new token"
        );
        // Old token should no longer be valid (join does NOT preserve
        // previous_token_hash — unlike rotate).
        let old_valid = block_on(store.validate_token("oldtoken1234567890abcdef"));
        let new_valid = block_on(store.validate_token(&new_token));
        assert!(!old_valid, "old token should be invalid after join");
        assert!(new_valid, "new token should be valid");
    }

    // ── join nonexistent host ────────────────────────────────────────

    #[test]
    fn test_join_nonexistent_host_fails() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        let result = block_on(store.join("ghost-host"));
        assert!(result.is_err(), "joining a nonexistent host should error");
    }

    // ── set_host_meta updates tags ───────────────────────────────────

    #[test]
    fn test_set_host_meta_updates_tags() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add("test-host", "sometoken1234567890abc", &[], None)).unwrap();

        let updated = block_on(store.set_host_meta(
            "test-host",
            None,
            Some(&["web".into(), "prod".into()]),
            None,
        ))
        .unwrap();
        assert!(updated, "set_host_meta on existing host should return true");

        let metas = block_on(store.get_all_host_meta());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].tags, vec!["web", "prod"]);
    }

    // ── set_host_meta updates group ──────────────────────────────────

    #[test]
    fn test_set_host_meta_updates_group() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        block_on(store.add("test-host", "sometoken1234567890abc", &[], None)).unwrap();

        let updated =
            block_on(store.set_host_meta("test-host", Some("production"), None, None)).unwrap();
        assert!(updated);

        let metas = block_on(store.get_all_host_meta());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].group, "production");
    }

    // ── set_host_meta on nonexistent host returns false ──────────────

    #[test]
    fn test_set_host_meta_nonexistent_returns_false() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        let updated = block_on(store.set_host_meta("no-such-host", Some("g"), None, None)).unwrap();
        assert!(
            !updated,
            "set_host_meta on nonexistent host should return false"
        );
    }

    // ── remove nonexistent host does not panic ───────────────────────

    #[test]
    fn test_remove_nonexistent_no_error() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_test_schema(&conn);
        let store = BridgeStore::from_conn(conn);

        // Must not panic.
        block_on(store.remove("ghost-host")).unwrap();
    }
}
