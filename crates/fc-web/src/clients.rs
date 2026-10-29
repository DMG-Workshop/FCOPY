//! Paired remote PCs. A profile is the address plus the fingerprint the user
//! confirmed. It never holds a password: the password is used once, at pairing,
//! and is then dropped.

use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    /// `[user@]host[:port]:/share/path`.
    pub target: String,
    /// The `SHA256:…` fingerprint the user confirmed at pairing.
    pub fingerprint: String,
}

/// Saved profiles, optionally persisted as JSON (mode 0600).
pub struct ClientStore {
    profiles: RwLock<Vec<Profile>>,
    path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not save clients: {0}")]
    Io(#[from] std::io::Error),
}

impl ClientStore {
    /// Load from `path` if it exists. `None` keeps profiles in memory only.
    pub fn open(path: Option<PathBuf>) -> Result<Self, StoreError> {
        let profiles = match &path {
            Some(p) if p.exists() => serde_json::from_slice(&std::fs::read(p)?).unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(Self { profiles: RwLock::new(profiles), path })
    }

    pub fn list(&self) -> Vec<Profile> {
        self.profiles.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn get(&self, id: u64) -> Option<Profile> {
        self.profiles.read().unwrap_or_else(|e| e.into_inner()).iter().find(|p| p.id == id).cloned()
    }

    /// Store a new profile. Returns it with its id.
    pub fn add(&self, name: String, target: String, fingerprint: String) -> Result<Profile, StoreError> {
        let mut profiles = self.profiles.write().unwrap_or_else(|e| e.into_inner());
        let id = profiles.iter().map(|p| p.id).max().unwrap_or(0) + 1;
        let profile = Profile { id, name, target, fingerprint };
        profiles.push(profile.clone());
        self.save(&profiles)?;
        Ok(profile)
    }

    pub fn remove(&self, id: u64) -> Result<bool, StoreError> {
        let mut profiles = self.profiles.write().unwrap_or_else(|e| e.into_inner());
        let before = profiles.len();
        profiles.retain(|p| p.id != id);
        let removed = profiles.len() != before;
        if removed {
            self.save(&profiles)?;
        }
        Ok(removed)
    }

    fn save(&self, profiles: &[Profile]) -> Result<(), StoreError> {
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let json = serde_json::to_vec_pretty(profiles).map_err(std::io::Error::other)?;
            fc_remote::config::write_private(path, &json)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_persist_without_secrets_and_ids_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clients.json");
        let store = ClientStore::open(Some(path.clone())).unwrap();
        let a = store.add("nas".into(), "alice@nas:/backup".into(), "SHA256:abc".into()).unwrap();
        let b = store.add("laptop".into(), "bob@laptop:/share".into(), "SHA256:def".into()).unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert!(store.remove(a.id).unwrap());
        assert!(!store.remove(a.id).unwrap());

        let reopened = ClientStore::open(Some(path.clone())).unwrap();
        assert_eq!(reopened.list(), vec![b.clone()]);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("password"));
        assert_eq!(reopened.add("x".into(), "y:/z".into(), "SHA256:9".into()).unwrap().id, 3);
    }

    #[test]
    fn in_memory_store_works_without_a_path() {
        let store = ClientStore::open(None).unwrap();
        store.add("n".into(), "h:/s".into(), "SHA256:1".into()).unwrap();
        assert_eq!(store.list().len(), 1);
    }
}
