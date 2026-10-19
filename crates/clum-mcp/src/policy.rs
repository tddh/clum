//! Role-based authorization policy (P0 of clum-docs/authorization-design.md).
//!
//! Storage lives in the same SQLite file as the audit log. The in-memory
//! snapshot is rebuilt whenever the `policy_meta.revision` counter changes, so
//! `clum-mcp role …` changes take effect without restarting the server.
//!
//! Failure policy:
//! * a snapshot that fails to load keeps the previous snapshot (never degrade
//!   to allow-all);
//! * a principal that has never been materialised in `principals` falls back
//!   to the pre-P0 group rules, so existing keys keep working unchanged;
//! * once a principal exists, its bindings are authoritative — an empty set
//!   means deny (default deny).

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::capability::Capability;

/// Builtin roles are seeded on every open. Legacy roles are managed by the
/// migration and must not be edited from the CLI.
pub const LEGACY_SUPERADMIN_ROLE: &str = "legacy-superadmin";
pub const LEGACY_GROUP_PREFIX: &str = "legacy-group:";

const POLICY_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS principals (
        id         TEXT PRIMARY KEY,
        kind       TEXT NOT NULL,
        name       TEXT NOT NULL UNIQUE,
        status     TEXT NOT NULL DEFAULT 'active',
        created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS roles (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL UNIQUE,
        builtin    INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS grants (
        id               TEXT PRIMARY KEY,
        role_id          TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
        resource_type    TEXT NOT NULL,
        selector_json    TEXT NOT NULL,
        actions_json     TEXT NOT NULL,
        require_approval INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_grants_role ON grants(role_id);

    CREATE TABLE IF NOT EXISTS bindings (
        principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
        role_id      TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
        PRIMARY KEY (principal_id, role_id)
    );

    CREATE TABLE IF NOT EXISTS policy_meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

/// Resource selector. A grant matches when every configured dimension matches;
/// an empty selector never matches (use `{"all": true}` for a global grant).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Selector {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_group"
    )]
    pub group: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub all: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Accept `{"group":"dev"}` as well as `{"group":["dev","staging"]}`.
fn deserialize_group<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    Option::<OneOrMany>::deserialize(deserializer).map(|value| {
        value.map(|value| match value {
            OneOrMany::One(group) => vec![group],
            OneOrMany::Many(groups) => groups,
        })
    })
}

impl Selector {
    pub fn global() -> Self {
        Self {
            all: true,
            ..Self::default()
        }
    }

    pub fn for_group(group: &str) -> Self {
        Self {
            group: Some(vec![group.to_string()]),
            ..Self::default()
        }
    }

    /// Match against a concrete host. `None` means the call has no host
    /// context (for example `host_list`): a host-scoped grant still matches,
    /// because the tool itself applies the group filter on its results. This
    /// preserves the pre-P0 behaviour for non-host tools.
    pub fn matches(&self, host: Option<&HostResource>) -> bool {
        if self.all {
            return true;
        }
        let has_dimension = self.group.is_some() || !self.labels.is_empty() || self.name.is_some();
        if !has_dimension {
            return false;
        }
        let Some(host) = host else {
            return true;
        };

        if let Some(groups) = &self.group {
            match &host.group {
                Some(group) if groups.iter().any(|candidate| candidate == group) => {}
                _ => return false,
            }
        }
        for (key, value) in &self.labels {
            if host.labels.get(key) != Some(value) {
                return false;
            }
        }
        if let Some(pattern) = &self.name {
            // A malformed pattern fails closed.
            let matches = glob::Pattern::new(pattern)
                .map(|pattern| pattern.matches(&host.name))
                .unwrap_or(false);
            if !matches {
                return false;
            }
        }

        self.group.is_some() || !self.labels.is_empty() || self.name.is_some()
    }

    pub fn describe(&self) -> String {
        if self.all {
            return "all".to_string();
        }
        let mut parts = Vec::new();
        if let Some(groups) = &self.group {
            parts.push(format!("group={}", groups.join(",")));
        }
        if !self.labels.is_empty() {
            let mut labels: Vec<String> = self
                .labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            labels.sort();
            parts.push(format!("labels[{}]", labels.join(",")));
        }
        if let Some(name) = &self.name {
            parts.push(format!("name={name}"));
        }
        if parts.is_empty() {
            "empty".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// Concrete resource the authorization decision is evaluated against.
#[derive(Debug, Clone, Default)]
pub struct HostResource {
    pub name: String,
    pub group: Option<String>,
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Grant {
    pub id: String,
    pub resource_type: String,
    pub selector: Selector,
    pub actions: Vec<String>,
    pub require_approval: bool,
}

#[derive(Debug, Clone)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub grants: Vec<Grant>,
}

#[derive(Debug, Clone, Default)]
pub struct PolicySnapshot {
    pub revision: u64,
    pub roles: HashMap<String, Role>,
    /// Principal name → role names.
    pub bindings: HashMap<String, Vec<String>>,
    /// Principals that have been materialised. A name absent here still falls
    /// back to the legacy group rules.
    pub known_principals: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct PolicyMatch {
    pub role: String,
    pub grant_id: String,
    pub selector: String,
    pub require_approval: bool,
}

#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub require_approval: bool,
    pub capability: String,
    pub matched: Vec<PolicyMatch>,
    pub reason: String,
}

pub struct PolicyStore {
    db: Mutex<Connection>,
    snapshot: RwLock<Arc<PolicySnapshot>>,
    /// Unix millis of the last revision probe. Bounds the DB read rate to one
    /// per second even when a batch call checks many hosts.
    last_check_ms: AtomicU64,
}

impl PolicyStore {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let conn = Connection::open(path)
            .with_context(|| format!("open policy db: {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Policy rows reveal the whole authorization graph; keep the DB
            // owner-only even when the CLI creates it under a permissive umask.
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(POLICY_SCHEMA)
            .context("create policy schema")?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        seed_builtin_roles(&conn)?;
        migrate_legacy(&conn)?;

        let snapshot = load_snapshot(&conn).context("load policy snapshot")?;
        Ok(Arc::new(Self {
            db: Mutex::new(conn),
            snapshot: RwLock::new(Arc::new(snapshot)),
            last_check_ms: AtomicU64::new(0),
        }))
    }

    pub fn snapshot(&self) -> Arc<PolicySnapshot> {
        self.maybe_reload();
        self.snapshot.read().clone()
    }

    /// Reload the snapshot. On failure the previous snapshot is kept.
    pub fn reload(&self) -> Result<u64> {
        let conn = self.db.lock();
        let snapshot = load_snapshot(&conn)?;
        let revision = snapshot.revision;
        drop(conn);
        *self.snapshot.write() = Arc::new(snapshot);
        Ok(revision)
    }

    fn maybe_reload(&self) {
        // Policy changes are rare; probing the revision counter on every host
        // of a batch call would serialize all tool calls on the DB mutex.
        let now = unix_millis();
        let last = self.last_check_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 1000 {
            return;
        }
        if self
            .last_check_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            // Another thread is already probing; use the current snapshot.
            return;
        }

        let db_revision = {
            let conn = self.db.lock();
            read_revision(&conn).unwrap_or(0)
        };
        let snapshot_revision = self.snapshot.read().revision;
        if db_revision != snapshot_revision {
            if let Err(error) = self.reload() {
                // Fail closed: keep serving the previous snapshot.
                tracing::error!("policy reload failed; keeping previous snapshot: {error:#}");
            }
        }
    }

    /// Evaluate one capability for one principal.
    pub fn check(
        &self,
        principal: &str,
        group_hint: Option<&str>,
        capability: Capability,
        host: Option<&HostResource>,
    ) -> PolicyDecision {
        let snapshot = self.snapshot();
        let cap = capability.as_str();

        let role_names: Vec<String> = match snapshot.bindings.get(principal) {
            Some(roles) if !roles.is_empty() => roles.clone(),
            Some(_) => {
                // Materialised principal with no bindings: default deny.
                return PolicyDecision {
                    allowed: false,
                    require_approval: false,
                    capability: cap.to_string(),
                    matched: Vec::new(),
                    reason: format!("principal '{principal}' has no role bindings"),
                };
            }
            None if snapshot.known_principals.contains(principal) => {
                return PolicyDecision {
                    allowed: false,
                    require_approval: false,
                    capability: cap.to_string(),
                    matched: Vec::new(),
                    reason: format!("principal '{principal}' has no role bindings"),
                };
            }
            None => return legacy_fallback(principal, group_hint, capability, host),
        };

        let mut matched = Vec::new();
        for role_name in &role_names {
            let Some(role) = snapshot.roles.get(role_name) else {
                continue;
            };
            for grant in &role.grants {
                if !grant.actions.iter().any(|action| action == cap) {
                    continue;
                }
                if !grant.selector.matches(host) {
                    continue;
                }
                // A host-less global capability (reload_config) has no target
                // for a group selector to match; requiring `all` prevents a
                // "prod-scoped" grant from silently authorizing it everywhere.
                if host.is_none() && capability.is_global_only() && !grant.selector.all {
                    continue;
                }
                matched.push(PolicyMatch {
                    role: role.name.clone(),
                    grant_id: grant.id.clone(),
                    selector: grant.selector.describe(),
                    require_approval: grant.require_approval,
                });
            }
        }

        if matched.is_empty() {
            PolicyDecision {
                allowed: false,
                require_approval: false,
                capability: cap.to_string(),
                matched,
                reason: format!(
                    "no grant for '{cap}' on {} (roles: {})",
                    host.map(|h| h.name.as_str()).unwrap_or("(no host)"),
                    if role_names.is_empty() {
                        "none".to_string()
                    } else {
                        role_names.join(",")
                    }
                ),
            }
        } else {
            PolicyDecision {
                allowed: true,
                require_approval: matched.iter().any(|m| m.require_approval),
                capability: cap.to_string(),
                matched,
                reason: String::new(),
            }
        }
    }

    // ── management (server CLI) ─────────────────────────────────────

    pub fn grant(
        &self,
        role_name: &str,
        resource_type: &str,
        selector: &Selector,
        actions: &[Capability],
        require_approval: bool,
    ) -> Result<bool> {
        if actions.is_empty() {
            anyhow::bail!("--actions must list at least one capability");
        }
        if !matches!(
            resource_type,
            "host" | "global" | "command" | "file" | "session" | "recording" | "audit" | "policy"
        ) {
            anyhow::bail!("unknown resource type '{resource_type}'");
        }

        let selector_json = serde_json::to_string(selector)?;
        let actions_json = serde_json::to_string(
            &actions
                .iter()
                .map(|capability| capability.as_str())
                .collect::<Vec<_>>(),
        )?;

        let conn = self.db.lock();
        let role_id = ensure_role(&conn, role_name, false)?;
        let builtin: bool = conn.query_row(
            "SELECT builtin FROM roles WHERE id = ?1",
            params![role_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if builtin
            && (role_name.starts_with(LEGACY_GROUP_PREFIX) || role_name == LEGACY_SUPERADMIN_ROLE)
        {
            anyhow::bail!(
                "'{role_name}' is managed by the legacy migration and cannot be edited; bind keys to a normal role instead"
            );
        }

        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM grants
                 WHERE role_id = ?1 AND resource_type = ?2
                   AND selector_json = ?3 AND actions_json = ?4",
                params![role_id, resource_type, selector_json, actions_json],
                |row| row.get(0),
            )
            .optional()?;

        let created = match existing {
            Some(id) if id.starts_with("builtin:") => {
                anyhow::bail!(
                    "grant '{id}' is builtin and cannot be edited; grant the capability on another role and bind the key to it"
                );
            }
            Some(id) => {
                conn.execute(
                    "UPDATE grants SET require_approval = ?1 WHERE id = ?2",
                    params![i64::from(require_approval), id],
                )?;
                false
            }
            None => {
                conn.execute(
                    "INSERT INTO grants
                     (id, role_id, resource_type, selector_json, actions_json, require_approval)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        role_id,
                        resource_type,
                        selector_json,
                        actions_json,
                        i64::from(require_approval)
                    ],
                )?;
                true
            }
        };

        bump_revision(&conn)?;
        drop(conn);
        self.reload()?;
        Ok(created)
    }

    pub fn bind(&self, principal_name: &str, role_name: &str) -> Result<bool> {
        let conn = self.db.lock();
        let principal_id = ensure_principal(&conn, principal_name)?.ok_or_else(|| {
            anyhow::anyhow!(
                "unknown key '{principal_name}': create it with `clum-mcp agent add` first"
            )
        })?;
        let role_id: Option<String> = conn
            .query_row(
                "SELECT id FROM roles WHERE name = ?1",
                params![role_name],
                |row| row.get(0),
            )
            .optional()?;
        let role_id = role_id
            .ok_or_else(|| anyhow::anyhow!("unknown role '{role_name}' (grant to it first)"))?;

        let changed = conn.execute(
            "INSERT OR IGNORE INTO bindings (principal_id, role_id) VALUES (?1, ?2)",
            params![principal_id, role_id],
        )?;
        bump_revision(&conn)?;
        drop(conn);
        self.reload()?;
        Ok(changed > 0)
    }

    pub fn unbind(&self, principal_name: &str, role_name: &str) -> Result<bool> {
        let conn = self.db.lock();
        let principal_id: Option<String> = conn
            .query_row(
                "SELECT id FROM principals WHERE name = ?1",
                params![principal_name],
                |row| row.get(0),
            )
            .optional()?;
        let Some(principal_id) = principal_id else {
            return Ok(false);
        };
        let role_id: Option<String> = conn
            .query_row(
                "SELECT id FROM roles WHERE name = ?1",
                params![role_name],
                |row| row.get(0),
            )
            .optional()?;
        let Some(role_id) = role_id else {
            return Ok(false);
        };
        let changed = conn.execute(
            "DELETE FROM bindings WHERE principal_id = ?1 AND role_id = ?2",
            params![principal_id, role_id],
        )?;
        bump_revision(&conn)?;
        drop(conn);
        self.reload()?;
        Ok(changed > 0)
    }

    pub fn list_roles(&self) -> Vec<Role> {
        let mut roles: Vec<Role> = self.snapshot().roles.values().cloned().collect();
        roles.sort_by(|a, b| a.name.cmp(&b.name));
        roles
    }

    pub fn list_bindings(&self) -> Vec<(String, String)> {
        let snapshot = self.snapshot();
        let mut bindings: Vec<(String, String)> = snapshot
            .bindings
            .iter()
            .flat_map(|(principal, roles)| {
                roles
                    .iter()
                    .map(move |role| (principal.clone(), role.clone()))
            })
            .collect();
        bindings.sort();
        bindings
    }

    /// True when a grant with `selector.all` authorizes the capability.
    /// Host-less tools use this to avoid restricting superadmin/admin to the
    /// currently registered host set (recordings may outlive host entries).
    pub fn allows_all(
        &self,
        principal: &str,
        group_hint: Option<&str>,
        capability: Capability,
    ) -> bool {
        let snapshot = self.snapshot();
        let cap = capability.as_str();
        let role_names: Vec<String> = match snapshot.bindings.get(principal) {
            Some(roles) if !roles.is_empty() => roles.clone(),
            Some(_) => return false,
            None if snapshot.known_principals.contains(principal) => return false,
            // Legacy superadmin (no group) is unrestricted.
            None => return group_hint.is_none(),
        };
        role_names.iter().any(|role_name| {
            snapshot.roles.get(role_name).is_some_and(|role| {
                role.grants
                    .iter()
                    .any(|grant| grant.selector.all && grant.actions.iter().any(|a| a == cap))
            })
        })
    }

    /// Group of an API key, for `policy explain` and legacy fallback display.
    pub fn principal_group(&self, name: &str) -> Option<String> {
        let conn = self.db.lock();
        if !table_exists(&conn, "api_keys").ok()? {
            return None;
        }
        let group: Option<Option<String>> = conn
            .query_row(
                "SELECT group_name FROM api_keys WHERE name = ?1
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .ok()?;
        group.flatten()
    }
}

/// Pre-P0 rules for a principal that has not been materialised yet.
fn legacy_fallback(
    principal: &str,
    group_hint: Option<&str>,
    capability: Capability,
    host: Option<&HostResource>,
) -> PolicyDecision {
    let cap = capability.as_str();
    match group_hint {
        Some(group) => {
            let allowed = crate::capability::legacy_group_actions().contains(&capability)
                && host.is_none_or(|host| host.group.as_deref() == Some(group));
            PolicyDecision {
                allowed,
                require_approval: false,
                capability: cap.to_string(),
                matched: if allowed {
                    vec![PolicyMatch {
                        role: format!("{LEGACY_GROUP_PREFIX}{group}"),
                        grant_id: "(legacy fallback)".to_string(),
                        selector: format!("group={group}"),
                        require_approval: false,
                    }]
                } else {
                    Vec::new()
                },
                reason: if allowed {
                    format!("legacy fallback: principal '{principal}' is in group '{group}'")
                } else {
                    format!(
                        "'{cap}' is not available to group-scoped legacy key '{principal}' (group {group})"
                    )
                },
            }
        }
        None => {
            let allowed = crate::capability::legacy_superadmin_actions().contains(&capability);
            PolicyDecision {
                allowed,
                require_approval: false,
                capability: cap.to_string(),
                matched: if allowed {
                    vec![PolicyMatch {
                        role: LEGACY_SUPERADMIN_ROLE.to_string(),
                        grant_id: "(legacy fallback)".to_string(),
                        selector: "all".to_string(),
                        require_approval: false,
                    }]
                } else {
                    Vec::new()
                },
                reason: if allowed {
                    format!("legacy fallback: '{principal}' is a superadmin key")
                } else {
                    format!("'{cap}' is not available to legacy superadmin key '{principal}'")
                },
            }
        }
    }
}

fn ensure_role(conn: &Connection, name: &str, builtin: bool) -> Result<String> {
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM roles WHERE name = ?1",
            params![name],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO roles (id, name, builtin, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            id,
            name,
            i64::from(builtin),
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(id)
}

/// Materialise a principal from `api_keys`. Returns `None` when the key does
/// not exist (or the table has not been created yet).
fn ensure_principal(conn: &Connection, name: &str) -> Result<Option<String>> {
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM principals WHERE name = ?1",
            params![name],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(Some(id));
    }
    if !table_exists(conn, "api_keys")? {
        return Ok(None);
    }
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM api_keys WHERE name = ?1)",
        params![name],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if !exists {
        return Ok(None);
    }
    insert_principal(conn, name)
}

fn insert_principal(conn: &Connection, name: &str) -> Result<Option<String>> {
    let id = uuid::Uuid::new_v4().to_string();
    let changed = conn.execute(
        "INSERT OR IGNORE INTO principals (id, kind, name, status, created_at)
         VALUES (?1, 'api_key', ?2, 'active', ?3)",
        params![id, name, chrono::Utc::now().to_rfc3339()],
    )?;
    if changed == 0 {
        return conn
            .query_row(
                "SELECT id FROM principals WHERE name = ?1",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    Ok(Some(id))
}

/// Seed the three documented builtin roles. `admin` carries a global grant;
/// `operator` / `readonly` start empty and are scoped by the administrator
/// with `role grant`.
fn seed_builtin_roles(conn: &Connection) -> Result<()> {
    let admin_id = ensure_role(conn, "admin", true)?;
    ensure_role(conn, "operator", true)?;
    ensure_role(conn, "readonly", true)?;

    let actions = serde_json::to_string(
        &crate::capability::admin_actions()
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
    )?;
    let selector = serde_json::to_string(&Selector::global())?;
    conn.execute(
        "INSERT INTO grants
         (id, role_id, resource_type, selector_json, actions_json, require_approval)
         VALUES ('builtin:admin', ?1, 'global', ?2, ?3, 0)
         ON CONFLICT(id) DO UPDATE SET
             resource_type = excluded.resource_type,
             selector_json = excluded.selector_json,
             actions_json = excluded.actions_json",
        params![admin_id, selector, actions],
    )?;
    Ok(())
}

/// Migrate existing API keys:
/// * every distinct key name gets a `principals` row;
/// * a newly materialised key is bound to `legacy-group:<group>` (or
///   `legacy-superadmin` when it has no group);
/// * already-materialised principals are left untouched, so an explicit
///   unbind is never silently restored on restart.
fn migrate_legacy(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "api_keys")? {
        return Ok(());
    }

    let mut stmt =
        conn.prepare("SELECT name, group_name FROM api_keys ORDER BY created_at ASC, rowid ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;

    // Later rows win: `rotate` keeps old rows around, and the newest key is
    // the one that is actually in use.
    let mut latest: HashMap<String, Option<String>> = HashMap::new();
    for row in rows {
        let (name, group) = row?;
        latest.insert(name, group);
    }
    drop(stmt);

    for (name, group) in latest {
        // Only newly materialised principals get a legacy binding.
        if conn
            .query_row(
                "SELECT 1 FROM principals WHERE name = ?1",
                params![name],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            continue;
        }
        let Some(principal_id) = insert_principal(conn, &name)? else {
            continue;
        };

        let role_name = match group {
            Some(group) if !group.is_empty() => {
                ensure_legacy_group_role(conn, &group)?;
                format!("{LEGACY_GROUP_PREFIX}{group}")
            }
            _ => {
                ensure_legacy_superadmin_role(conn)?;
                LEGACY_SUPERADMIN_ROLE.to_string()
            }
        };

        let role_id = ensure_role(conn, &role_name, true)?;
        conn.execute(
            "INSERT OR IGNORE INTO bindings (principal_id, role_id) VALUES (?1, ?2)",
            params![principal_id, role_id],
        )?;
    }

    Ok(())
}

fn ensure_legacy_group_role(conn: &Connection, group: &str) -> Result<()> {
    let role_name = format!("{LEGACY_GROUP_PREFIX}{group}");
    let role_id = ensure_role(conn, &role_name, true)?;
    let actions = serde_json::to_string(
        &crate::capability::legacy_group_actions()
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
    )?;
    let selector = serde_json::to_string(&Selector::for_group(group))?;
    conn.execute(
        "INSERT INTO grants
         (id, role_id, resource_type, selector_json, actions_json, require_approval)
         VALUES (?1, ?2, 'host', ?3, ?4, 0)
         ON CONFLICT(id) DO UPDATE SET
             resource_type = excluded.resource_type,
             selector_json = excluded.selector_json,
             actions_json = excluded.actions_json",
        params![
            format!("builtin:legacy-group:{group}"),
            role_id,
            selector,
            actions
        ],
    )?;
    Ok(())
}

fn ensure_legacy_superadmin_role(conn: &Connection) -> Result<()> {
    let role_id = ensure_role(conn, LEGACY_SUPERADMIN_ROLE, true)?;
    let actions = serde_json::to_string(
        &crate::capability::legacy_superadmin_actions()
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
    )?;
    let selector = serde_json::to_string(&Selector::global())?;
    conn.execute(
        "INSERT INTO grants
         (id, role_id, resource_type, selector_json, actions_json, require_approval)
         VALUES ('builtin:legacy-superadmin', ?1, 'global', ?2, ?3, 0)
         ON CONFLICT(id) DO UPDATE SET
             resource_type = excluded.resource_type,
             selector_json = excluded.selector_json,
             actions_json = excluded.actions_json",
        params![role_id, selector, actions],
    )?;
    Ok(())
}

fn load_snapshot(conn: &Connection) -> Result<PolicySnapshot> {
    let revision = read_revision(conn)?;

    let mut roles: HashMap<String, Role> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, name, builtin FROM roles ORDER BY name ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok(Role {
                id: row.get(0)?,
                name: row.get(1)?,
                builtin: row.get::<_, i64>(2)? != 0,
                grants: Vec::new(),
            })
        })?;
        for role in rows {
            let role = role?;
            roles.insert(role.name.clone(), role);
        }
    }

    {
        let mut stmt = conn.prepare(
            "SELECT id, role_id, resource_type, selector_json, actions_json, require_approval
             FROM grants ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)? != 0,
            ))
        })?;
        for row in rows {
            let (id, role_id, resource_type, selector_json, actions_json, require_approval) = row?;
            let selector: Selector = serde_json::from_str(&selector_json)
                .with_context(|| format!("parse selector of grant {id}: {selector_json}"))?;
            let actions: Vec<String> = serde_json::from_str(&actions_json)
                .with_context(|| format!("parse actions of grant {id}: {actions_json}"))?;
            for action in &actions {
                if Capability::parse(action).is_none() {
                    anyhow::bail!("grant {id} references unknown capability '{action}'");
                }
            }
            let Some(role) = roles.values_mut().find(|role| role.id == role_id) else {
                continue;
            };
            role.grants.push(Grant {
                id,
                resource_type,
                selector,
                actions,
                require_approval,
            });
        }
    }

    let mut bindings: HashMap<String, Vec<String>> = HashMap::new();
    let mut known_principals = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT name FROM principals")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            known_principals.insert(row?);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT p.name, r.name
             FROM bindings b
             JOIN principals p ON p.id = b.principal_id
             JOIN roles r ON r.id = b.role_id
             ORDER BY p.name ASC, r.name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (principal, role) = row?;
            bindings.entry(principal).or_default().push(role);
        }
    }

    Ok(PolicySnapshot {
        revision,
        roles,
        bindings,
        known_principals,
    })
}

fn read_revision(conn: &Connection) -> Result<u64> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM policy_meta WHERE key = 'revision'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.and_then(|value| value.parse().ok()).unwrap_or(0))
}

fn bump_revision(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO policy_meta (key, value) VALUES ('revision', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
        [],
    )?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get::<_, i64>(0),
    )? > 0)
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, group: Option<&str>) -> HostResource {
        HostResource {
            name: name.to_string(),
            group: group.map(str::to_string),
            labels: HashMap::new(),
        }
    }

    fn open_store(keys: &[(&str, Option<&str>)]) -> Arc<PolicyStore> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE api_keys (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, key_hash TEXT NOT NULL,
                key_prefix TEXT NOT NULL, created_at TEXT NOT NULL,
                expires_at TEXT, revoked_at TEXT, last_used_at TEXT, group_name TEXT
            );",
        )
        .unwrap();
        for (index, (name, group)) in keys.iter().enumerate() {
            conn.execute(
                "INSERT INTO api_keys (id, name, key_hash, key_prefix, created_at, group_name)
                 VALUES (?1, ?2, 'h', 'p', ?3, ?4)",
                params![
                    format!("id{index}"),
                    name,
                    format!("2026-01-01T00:00:{index:02}Z"),
                    group
                ],
            )
            .unwrap();
        }
        conn.execute_batch(POLICY_SCHEMA).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        seed_builtin_roles(&conn).unwrap();
        migrate_legacy(&conn).unwrap();
        let snapshot = load_snapshot(&conn).unwrap();
        Arc::new(PolicyStore {
            db: Mutex::new(conn),
            snapshot: RwLock::new(Arc::new(snapshot)),
            last_check_ms: AtomicU64::new(0),
        })
    }

    #[test]
    fn legacy_group_key_keeps_old_behaviour() {
        let store = open_store(&[("agent-dev", Some("dev"))]);
        let dev = host("devhost", Some("dev"));
        let prod = host("prodhost", Some("prod"));

        assert!(
            store
                .check(
                    "agent-dev",
                    Some("dev"),
                    Capability::CommandExec,
                    Some(&dev)
                )
                .allowed
        );
        assert!(
            !store
                .check(
                    "agent-dev",
                    Some("dev"),
                    Capability::CommandExec,
                    Some(&prod)
                )
                .allowed
        );
        assert!(
            !store
                .check("agent-dev", Some("dev"), Capability::ConfigReload, None)
                .allowed
        );
        assert!(
            !store
                .check("agent-dev", Some("dev"), Capability::HostAdmin, Some(&dev))
                .allowed
        );
        // No host context keeps the pre-P0 "tool applies its own group filter"
        // behaviour.
        assert!(
            store
                .check("agent-dev", Some("dev"), Capability::HostRead, None)
                .allowed
        );
    }

    #[test]
    fn legacy_superadmin_keeps_old_behaviour() {
        let store = open_store(&[("root", None)]);
        let prod = host("prodhost", Some("prod"));
        assert!(
            store
                .check("root", None, Capability::CommandExec, Some(&prod))
                .allowed
        );
        assert!(
            store
                .check("root", None, Capability::ConfigReload, None)
                .allowed
        );
        assert!(
            !store
                .check("root", None, Capability::PolicyWrite, None)
                .allowed
        );
    }

    #[test]
    fn allows_all_matches_global_grants_only() {
        let store = open_store(&[("root", None), ("agent-dev", Some("dev"))]);
        assert!(store.allows_all("root", None, Capability::CommandExec));
        assert!(!store.allows_all("agent-dev", Some("dev"), Capability::CommandExec));
    }

    #[test]
    fn unbound_principal_is_denied_after_explicit_bind() {
        let store = open_store(&[("agent-dev", Some("dev"))]);
        let dev = host("devhost", Some("dev"));
        assert!(
            store
                .check(
                    "agent-dev",
                    Some("dev"),
                    Capability::CommandExec,
                    Some(&dev)
                )
                .allowed
        );

        // Explicitly bind to an empty role, then unbind: the principal stays
        // materialised, so it must not fall back to legacy allow.
        store
            .grant(
                "empty",
                "host",
                &Selector::for_group("dev"),
                &[Capability::HostRead],
                false,
            )
            .unwrap();
        store.bind("agent-dev", "empty").unwrap();
        store
            .unbind("agent-dev", &format!("{LEGACY_GROUP_PREFIX}dev"))
            .unwrap();
        let decision = store.check(
            "agent-dev",
            Some("dev"),
            Capability::CommandExec,
            Some(&dev),
        );
        assert!(
            !decision.allowed,
            "explicitly unbound principal must be denied"
        );

        // Unbinding the last role leaves an empty binding set: deny all.
        store.unbind("agent-dev", "empty").unwrap();
        assert!(
            !store
                .check("agent-dev", Some("dev"), Capability::HostRead, Some(&dev))
                .allowed
        );
    }

    #[test]
    fn grant_selector_and_require_approval() {
        let store = open_store(&[("agent-x", Some("dev"))]);
        store
            .grant(
                "operator",
                "host",
                &Selector::for_group("prod"),
                &[Capability::CommandExec],
                true,
            )
            .unwrap();
        store.bind("agent-x", "operator").unwrap();
        store
            .unbind("agent-x", &format!("{LEGACY_GROUP_PREFIX}dev"))
            .unwrap();

        let prod = host("prodhost", Some("prod"));
        let dev = host("devhost", Some("dev"));
        let decision = store.check("agent-x", Some("dev"), Capability::CommandExec, Some(&prod));
        assert!(decision.allowed);
        assert!(decision.require_approval);
        assert!(
            !store
                .check("agent-x", Some("dev"), Capability::CommandExec, Some(&dev))
                .allowed
        );
        assert!(
            !store
                .check("agent-x", Some("dev"), Capability::HostRead, Some(&prod))
                .allowed
        );
    }

    #[test]
    fn selector_matching() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        let selector = Selector {
            group: Some(vec!["dev".to_string(), "staging".to_string()]),
            labels,
            name: Some("web-*".to_string()),
            all: false,
        };

        let mut matching = host("web-01", Some("staging"));
        matching
            .labels
            .insert("env".to_string(), "prod".to_string());
        assert!(selector.matches(Some(&matching)));

        assert!(!selector.matches(Some(&host("db-01", Some("staging")))));
        assert!(!selector.matches(Some(&host("web-01", Some("prod")))));
        assert!(!Selector::default().matches(None));
        assert!(Selector::global().matches(Some(&host("x", None))));
    }

    #[test]
    fn command_read_is_distinct_from_command_exec() {
        let store = open_store(&[("agent-x", Some("dev"))]);
        store
            .grant(
                "operator",
                "host",
                &Selector::for_group("dev"),
                &[Capability::CommandRead],
                false,
            )
            .unwrap();
        store.bind("agent-x", "operator").unwrap();
        store
            .unbind("agent-x", &format!("{LEGACY_GROUP_PREFIX}dev"))
            .unwrap();

        let host = host("devhost", Some("dev"));
        assert!(
            store
                .check("agent-x", Some("dev"), Capability::CommandRead, Some(&host))
                .allowed
        );
        assert!(
            !store
                .check("agent-x", Some("dev"), Capability::CommandExec, Some(&host))
                .allowed
        );
    }

    #[test]
    fn group_scoped_grant_cannot_authorize_global_capability() {
        let store = open_store(&[("agent-x", Some("dev"))]);
        store
            .grant(
                "operator",
                "host",
                &Selector::for_group("dev"),
                &[Capability::ConfigReload],
                false,
            )
            .unwrap();
        store.bind("agent-x", "operator").unwrap();
        store
            .unbind("agent-x", &format!("{LEGACY_GROUP_PREFIX}dev"))
            .unwrap();

        // reload_config has no host context: a group-scoped grant must not
        // authorize it (only an explicit --all grant may).
        assert!(
            !store
                .check("agent-x", Some("dev"), Capability::ConfigReload, None)
                .allowed
        );

        // A global grant does.
        store
            .grant(
                "operator",
                "global",
                &Selector::global(),
                &[Capability::ConfigReload],
                false,
            )
            .unwrap();
        assert!(
            store
                .check("agent-x", Some("dev"), Capability::ConfigReload, None)
                .allowed
        );
    }

    #[test]
    fn reload_failure_keeps_previous_snapshot() {
        let store = open_store(&[("agent-dev", Some("dev"))]);
        {
            let conn = store.db.lock();
            conn.execute(
                "INSERT INTO grants (id, role_id, resource_type, selector_json, actions_json)
                 SELECT 'broken', id, 'host', '{}', '[\"not:a:capability\"]'
                 FROM roles WHERE name = 'operator'",
                [],
            )
            .unwrap();
            bump_revision(&conn).unwrap();
        }
        // maybe_reload fails, but the old snapshot keeps answering.
        let decision = store.check(
            "agent-dev",
            Some("dev"),
            Capability::CommandExec,
            Some(&host("devhost", Some("dev"))),
        );
        assert!(decision.allowed);
    }
}
