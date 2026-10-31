//! Desktop PC profiles, separate from the web console's paired-client database.

use std::io;
use std::path::PathBuf;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::client::RemoteSpec;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    /// `[user@]host[:port]:/share/path`, including IPv6 in brackets.
    pub target: String,
    pub fingerprint: String,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub fn validate(name: &str, target: &str, fingerprint: &str) -> io::Result<RemoteSpec> {
    if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(invalid(
            "PC name must contain 1 to 64 characters, without control characters",
        ));
    }
    let spec = RemoteSpec::parse(target).map_err(|e| invalid(e.to_string()))?;
    if spec.host.chars().any(char::is_whitespace)
        || spec
            .user
            .as_ref()
            .is_some_and(|user| user.chars().any(char::is_whitespace))
        || target.chars().any(char::is_control)
    {
        return Err(invalid(
            "address and user must not contain whitespace or control characters",
        ));
    }
    let encoded = fingerprint
        .strip_prefix("SHA256:")
        .ok_or_else(|| invalid("fingerprint must start with SHA256:"))?;
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid("fingerprint must be an unpadded base64 SHA-256 digest"))?;
    if bytes.len() != 32 {
        return Err(invalid("fingerprint must contain a 32-byte SHA-256 digest"));
    }
    Ok(spec)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    next_id: u64,
    profiles: Vec<Profile>,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            next_id: 1,
            profiles: Vec::new(),
        }
    }
}

pub struct ProfileStore {
    path: PathBuf,
    saved: Saved,
}

impl ProfileStore {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let saved: Saved = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Saved::default(),
            Err(e) => return Err(e),
        };
        let mut ids = std::collections::HashSet::new();
        if saved.next_id == 0 {
            return Err(invalid("invalid next profile id"));
        }
        for p in &saved.profiles {
            validate(&p.name, &p.target, &p.fingerprint)?;
            if p.id == 0 || p.id >= saved.next_id || !ids.insert(p.id) {
                return Err(invalid("invalid or duplicate profile id"));
            }
        }
        Ok(Self { path, saved })
    }

    pub fn list(&self) -> &[Profile] {
        &self.saved.profiles
    }

    pub fn get(&self, id: u64) -> Option<&Profile> {
        self.saved.profiles.iter().find(|p| p.id == id)
    }

    /// The caller must explicitly confirm new or changed fingerprints.
    pub fn save(
        &mut self,
        id: Option<u64>,
        name: &str,
        target: &str,
        fingerprint: &str,
    ) -> io::Result<Profile> {
        let (name, target, fingerprint) = (name.trim(), target.trim(), fingerprint.trim());
        let spec = validate(name, target, fingerprint)?;
        let mut saved = self.saved.clone();
        let id = match id {
            Some(id) if self.get(id).is_some() => id,
            Some(_) => return Err(invalid("profile no longer exists; reload the list")),
            None => {
                let id = saved.next_id;
                saved.next_id = id
                    .checked_add(1)
                    .ok_or_else(|| invalid("profile ids exhausted"))?;
                id
            }
        };
        let profile = Profile {
            id,
            name: name.into(),
            target: spec.to_string(),
            fingerprint: fingerprint.into(),
        };
        if let Some(old) = saved.profiles.iter_mut().find(|p| p.id == id) {
            *old = profile.clone();
        } else {
            saved.profiles.push(profile.clone());
        }
        self.persist(saved)?;
        Ok(profile)
    }

    pub fn remove(&mut self, id: u64) -> io::Result<()> {
        if self.get(id).is_none() {
            return Err(invalid("profile no longer exists; reload the list"));
        }
        let mut saved = self.saved.clone();
        saved.profiles.retain(|p| p.id != id);
        self.persist(saved)
    }

    fn persist(&mut self, saved: Saved) -> io::Result<()> {
        let text = toml::to_string_pretty(&saved).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::config::write_private(&self.path, text.as_bytes())?;
        self.saved = saved;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_edit_delete_and_restart_preserve_ids_and_only_profile_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        let mut store = ProfileStore::open(path.clone()).unwrap();
        let fp = crate::crypto::fingerprint(b"test certificate");
        let a = store
            .save(None, "NAS", "alice@[fd00::1]:47801:/backup/photos", &fp)
            .unwrap();
        let b = store
            .save(None, "Laptop", "bob@laptop:/share", &fp)
            .unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        let edited = store
            .save(Some(a.id), "VPN NAS", "alice@nas.example:47802:/other", &fp)
            .unwrap();
        assert_eq!(edited.id, a.id);
        let reopened = ProfileStore::open(path.clone()).unwrap();
        assert_eq!(reopened.get(a.id), Some(&edited));
        let document: toml::Value =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let fields = document["profiles"][0].as_table().unwrap();
        assert_eq!(fields.len(), 4);
        for key in ["id", "name", "target", "fingerprint"] {
            assert!(fields.contains_key(key));
        }
        store.remove(a.id).unwrap();
        store.remove(b.id).unwrap();
        let mut reopened = ProfileStore::open(path).unwrap();
        assert!(reopened.list().is_empty());
        assert_eq!(
            reopened.save(None, "New", "host:/share", &fp).unwrap().id,
            3
        );
    }

    #[test]
    fn invalid_profiles_and_failed_writes_do_not_change_memory_or_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        let mut store = ProfileStore::open(path.clone()).unwrap();
        let fp = crate::crypto::fingerprint(b"cert");
        let p = store.save(None, "NAS", "host:/share", &fp).unwrap();
        let before = std::fs::read(&path).unwrap();
        for (name, target, fingerprint) in [
            ("", "host:/share", fp.as_str()),
            ("NAS", "C:/backup", fp.as_str()),
            ("NAS", "host:/../backup", fp.as_str()),
            ("NAS", "host:/share", "SHA256:invalid"),
        ] {
            assert!(store.save(Some(p.id), name, target, fingerprint).is_err());
        }
        assert!(store.save(Some(99), "NAS", "host:/share", &fp).is_err());
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(
            store
                .save(Some(p.id), "Changed", "host:/share", &fp)
                .is_err()
        );
        assert!(store.remove(p.id).is_err());
        assert_eq!(store.get(p.id), Some(&p));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn damaged_files_are_errors_not_empty_stores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        std::fs::write(&path, "broken = [").unwrap();
        assert!(ProfileStore::open(path.clone()).is_err());
        std::fs::write(
            &path,
            "next_id = 1\nprofiles = []\npassword = 'not-accepted'",
        )
        .unwrap();
        assert!(ProfileStore::open(path).is_err());
    }
}
