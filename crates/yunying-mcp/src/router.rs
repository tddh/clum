//! In-memory host registry that loads target host configurations from a YAML file
//! and provides lookup, listing, and counting operations. Supports hot-reload via
//! `reload()` for zero-downtime configuration updates.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use yunying_core::types::HostConfig;

/// Maps host names to their `HostConfig` for fast lookup by tool handlers.
/// Wraps the inner map in a `RwLock` to support atomic configuration reloads
/// without restarting the MCP server.
pub struct HostRouter {
    hosts: RwLock<HashMap<String, HostConfig>>,
    source_path: PathBuf,
}

impl HostRouter {
    /// Loads and parses a YAML hosts file into an in-memory host registry.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read hosts file: {}", path.display()))?;

        let registry: yunying_core::types::HostRegistry =
            serde_yml::from_str(&content).context("failed to parse hosts YAML")?;

        let hosts: HashMap<String, HostConfig> = registry
            .hosts
            .into_iter()
            .map(|h| (h.name.clone(), h))
            .collect();

        let count = hosts.len();
        tracing::info!("loaded {} hosts from {}", count, path.display());
        Ok(Self {
            hosts: RwLock::new(hosts),
            source_path: path.to_path_buf(),
        })
    }

    /// Reloads the host registry from the original source file.
    /// Returns the number of hosts loaded.
    /// On parse error, the existing configuration is preserved.
    pub fn reload(&self) -> Result<usize> {
        let content = std::fs::read_to_string(&self.source_path).with_context(|| {
            format!("failed to read hosts file: {}", self.source_path.display())
        })?;

        let registry: yunying_core::types::HostRegistry =
            serde_yml::from_str(&content).context("failed to parse hosts YAML")?;

        let new_hosts: HashMap<String, HostConfig> = registry
            .hosts
            .into_iter()
            .map(|h| (h.name.clone(), h))
            .collect();

        let count = new_hosts.len();
        {
            let mut hosts = self.hosts.write().expect("host registry lock poisoned");
            *hosts = new_hosts;
        }
        tracing::info!(
            "reloaded {} hosts from {}",
            count,
            self.source_path.display()
        );
        Ok(count)
    }

    /// Returns the `HostConfig` for the given host name, or `None` if not found.
    /// Returns an owned clone — the caller receives a snapshot consistent with
    /// the read lock held at call time.
    pub fn get(&self, name: &str) -> Option<HostConfig> {
        self.hosts
            .read()
            .expect("host registry lock poisoned")
            .get(name)
            .cloned()
    }

    /// Returns all registered hosts as a flat list (owned).
    pub fn list(&self) -> Vec<HostConfig> {
        self.hosts
            .read()
            .expect("host registry lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Returns the total number of registered hosts.
    pub fn len(&self) -> usize {
        self.hosts
            .read()
            .expect("host registry lock poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn write_temp_yaml(content: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "yunying-test-hosts-{}-{}.yaml",
            std::process::id(),
            id
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_load_valid_yaml() {
        let yaml = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1
    group: prod
    tags: [web]
    labels:
      dc: shanghai
  - name: host2
    bridge_addr: 10.0.0.2:9778
    bridge_token: tok2
    group: staging
    tags: [db]
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_get_existing_host() {
        let yaml = r#"
hosts:
  - name: test-host
    bridge_addr: 10.0.0.1:9778
    bridge_token: abc
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        let host = router.get("test-host").expect("host should exist");
        assert_eq!(host.name, "test-host");
        assert_eq!(host.bridge_addr, "10.0.0.1:9778");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_get_non_existing_host() {
        let yaml = r#"
hosts:
  - name: test-host
    bridge_addr: 10.0.0.1:9778
    bridge_token: abc
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert!(router.get("nonexistent").is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_list_returns_all_hosts() {
        let yaml = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1
  - name: host2
    bridge_addr: 10.0.0.2:9778
    bridge_token: tok2
  - name: host3
    bridge_addr: 10.0.0.3:9778
    bridge_token: tok3
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        let list = router.list();
        assert_eq!(list.len(), 3);
        let names: Vec<&str> = list.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"host1"));
        assert!(names.contains(&"host2"));
        assert!(names.contains(&"host3"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_len_returns_correct_count() {
        let yaml = r#"
hosts:
  - name: only
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_invalid_yaml() {
        let yaml = "not: valid: yaml: : broken";
        let path = write_temp_yaml(yaml);
        let result = HostRouter::from_file(&path);
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_empty_hosts() {
        let yaml = "hosts: []";
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 0);
        assert!(router.list().is_empty());
        assert!(router.get("any").is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_missing_file() {
        let path = std::env::temp_dir().join("yunying-nonexistent-file.yaml");
        let result = HostRouter::from_file(&path);
        assert!(result.is_err());
    }

    // ── Reload tests ──

    #[test]
    fn test_reload_updates_hosts() {
        let yaml_v1 = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1
"#;
        let path = write_temp_yaml(yaml_v1);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 1);
        assert!(router.get("host1").is_some());

        // Overwrite file with v2 config
        let yaml_v2 = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1-updated
  - name: host2
    bridge_addr: 10.0.0.2:9778
    bridge_token: tok2
"#;
        std::fs::write(&path, yaml_v2).unwrap();

        let count = router.reload().unwrap();
        assert_eq!(count, 2);
        assert_eq!(router.len(), 2);

        let h1 = router.get("host1").unwrap();
        assert_eq!(h1.bridge_token, "tok1-updated");
        assert!(router.get("host2").is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_reload_preserves_on_parse_error() {
        let yaml = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 1);

        // Write broken YAML — existing data must be preserved
        std::fs::write(&path, "garbage: :: broken").unwrap();

        let result = router.reload();
        assert!(result.is_err());
        // Existing data unchanged
        assert_eq!(router.len(), 1);
        assert!(router.get("host1").is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_reload_handles_empty_hosts() {
        let yaml = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 1);

        // Write empty hosts list
        std::fs::write(&path, "hosts: []").unwrap();

        let count = router.reload().unwrap();
        assert_eq!(count, 0);
        assert_eq!(router.len(), 0);
        assert!(router.get("host1").is_none());

        let _ = std::fs::remove_file(&path);
    }
}
