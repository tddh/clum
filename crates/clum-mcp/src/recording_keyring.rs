//! Server-side recording keyring: versioned X25519 keys + transparent decrypt.

use std::collections::HashMap;
use std::path::Path;

use clum_core::crypto::{decrypt_recording, is_encrypted, RecordingKey};

pub struct RecordingKeyring {
    current: RecordingKey,
    keys: HashMap<String, RecordingKey>,
}

impl RecordingKeyring {
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let current = RecordingKey::load_or_create(&dir.join("current.key"))?;
        let mut keys = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("key") {
                    continue;
                }
                if let Ok(k) = RecordingKey::load_or_create(&path) {
                    keys.insert(k.key_id(), k);
                }
            }
        }
        keys.insert(current.key_id(), current.clone());
        Ok(Self { current, keys })
    }

    pub fn current(&self) -> &RecordingKey {
        &self.current
    }

    pub fn lookup(&self, key_id: &str) -> Option<RecordingKey> {
        self.keys.get(key_id).cloned()
    }

    pub fn decrypt_or_passthrough(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if is_encrypted(data) {
            decrypt_recording(data, &|id| self.lookup(id))
        } else {
            Ok(data.to_vec())
        }
    }
}
