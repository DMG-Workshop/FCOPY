//! Durable, credential-free descriptions of unfinished desktop/web jobs.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fc_engine::conflict::{ConflictPolicy, ConflictResolver};
use fc_engine::{Algorithm, CopyOptions, TransferRequest, VerifyMode};
use fs4::FileExt;
use serde::{Deserialize, Serialize};

use crate::backend::{PullBackend, RemoteBackend};
use crate::client::{Connector, RemoteSpec};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Destination {
    Directory {
        path: PathBuf,
    },
    File {
        path: PathBuf,
    },
    Upload {
        target: String,
        fingerprint: String,
    },
    Download {
        target: String,
        fingerprint: String,
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedOptions {
    verify: String,
    algorithm: String,
    conflict: String,
    fsync: bool,
    preserve_mtime: bool,
    preserve_permissions: bool,
    preserve_xattrs: bool,
    preserve_acls: bool,
    check_space: bool,
    space_reserve: u64,
    create_parent_dirs: bool,
    preallocate: bool,
    chunk_size: usize,
    pipeline_depth: usize,
    /// Absent in jobs saved before move mode existed, which were copies.
    #[serde(default)]
    move_source: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: u64,
    pub owner: Option<String>,
    pub label: String,
    pub sources: Vec<PathBuf>,
    pub destination: Destination,
    options: SavedOptions,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Resume,
    Retry,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Job {
    pub fn new(
        request: &TransferRequest,
        destination: Destination,
        owner: Option<String>,
        defaults: &CopyOptions,
    ) -> io::Result<Self> {
        let options = request.options.as_ref().unwrap_or(defaults);
        let policy = request
            .conflict_policy
            .or_else(|| options.conflicts.as_ref().map(|r| r.policy))
            .unwrap_or(if options.overwrite {
                ConflictPolicy::Replace
            } else {
                ConflictPolicy::Ask
            });
        let job = Self {
            id: 0,
            owner,
            label: request.display_label(),
            sources: request.sources.clone(),
            destination,
            options: SavedOptions {
                verify: match options.verify {
                    VerifyMode::Off => "off",
                    VerifyMode::SourceHash => "source_hash",
                    VerifyMode::ReadBack => "readback",
                }
                .into(),
                algorithm: match options.algorithm {
                    Algorithm::Xxh3 => "xxh3",
                    Algorithm::Sha256 => "sha256",
                    Algorithm::Md5 => "md5",
                }
                .into(),
                conflict: match policy {
                    ConflictPolicy::Ask => "ask",
                    ConflictPolicy::Replace => "replace",
                    ConflictPolicy::Skip => "skip",
                    ConflictPolicy::KeepBoth => "keep_both",
                    ConflictPolicy::Compare => "compare",
                }
                .into(),
                fsync: options.fsync,
                preserve_mtime: options.preserve_mtime,
                preserve_permissions: options.preserve_permissions,
                preserve_xattrs: options.preserve_xattrs,
                preserve_acls: options.preserve_acls,
                check_space: options.check_space,
                space_reserve: options.space_reserve,
                create_parent_dirs: options.create_parent_dirs,
                preallocate: options.preallocate,
                chunk_size: options.chunk_size,
                pipeline_depth: options.pipeline_depth,
                move_source: options.move_source,
            },
        };
        job.validate()?;
        Ok(job)
    }

    pub fn remote(&self) -> bool {
        matches!(
            self.destination,
            Destination::Upload { .. } | Destination::Download { .. }
        )
    }

    pub fn describe(&self) -> String {
        match &self.destination {
            Destination::Directory { path } | Destination::File { path } => {
                path.display().to_string()
            }
            Destination::Upload { target, .. } => target.clone(),
            Destination::Download { target, path, .. } => {
                format!("{} (from {target})", path.display())
            }
        }
    }

    fn options(&self, action: Action) -> io::Result<CopyOptions> {
        let o = &self.options;
        let policy = if action == Action::Resume {
            ConflictPolicy::Compare
        } else {
            match o.conflict.as_str() {
                "ask" => ConflictPolicy::Ask,
                "replace" => ConflictPolicy::Replace,
                "skip" => ConflictPolicy::Skip,
                "keep_both" => ConflictPolicy::KeepBoth,
                "compare" => ConflictPolicy::Compare,
                _ => return Err(invalid("invalid saved conflict policy")),
            }
        };
        Ok(CopyOptions {
            verify: match o.verify.as_str() {
                "off" => VerifyMode::Off,
                "source_hash" => VerifyMode::SourceHash,
                "readback" => VerifyMode::ReadBack,
                _ => return Err(invalid("invalid saved verification mode")),
            },
            algorithm: match o.algorithm.as_str() {
                "xxh3" => Algorithm::Xxh3,
                "sha256" => Algorithm::Sha256,
                "md5" => Algorithm::Md5,
                _ => return Err(invalid("invalid saved hash algorithm")),
            },
            conflicts: Some(Arc::new(ConflictResolver::new(policy))),
            fsync: o.fsync,
            preserve_mtime: o.preserve_mtime,
            preserve_permissions: o.preserve_permissions,
            preserve_xattrs: o.preserve_xattrs,
            preserve_acls: o.preserve_acls,
            check_space: o.check_space,
            space_reserve: o.space_reserve,
            create_parent_dirs: o.create_parent_dirs,
            preallocate: o.preallocate,
            chunk_size: o.chunk_size,
            pipeline_depth: o.pipeline_depth,
            move_source: o.move_source,
            ..Default::default()
        })
    }

    pub fn validate(&self) -> io::Result<()> {
        self.options(Action::Retry)?;
        if self.sources.is_empty()
            || self.options.pipeline_depth == 0
            || self.options.pipeline_depth > 256
            || !(64 * 1024..=4 * 1024 * 1024).contains(&self.options.chunk_size)
        {
            return Err(invalid("invalid saved job sources or buffer settings"));
        }
        match &self.destination {
            Destination::File { path } => {
                if self.sources.len() != 1 || !path.is_absolute() {
                    return Err(invalid("invalid exact-file destination"));
                }
            }
            Destination::Directory { path } if !path.is_absolute() => {
                return Err(invalid("destination must be absolute"));
            }
            Destination::Upload {
                target,
                fingerprint,
            }
            | Destination::Download {
                target,
                fingerprint,
                ..
            } => {
                crate::profiles::validate("Recovered PC", target, fingerprint)?;
                if let Destination::Download { path, .. } = &self.destination {
                    if !path.is_absolute() {
                        return Err(invalid("download destination must be absolute"));
                    }
                    let spec = RemoteSpec::parse(target).map_err(|e| invalid(e.to_string()))?;
                    for source in &self.sources {
                        let source = source
                            .to_str()
                            .ok_or_else(|| invalid("invalid remote source text"))?;
                        let mut check = spec.clone();
                        check.path = source.replace('\\', "/");
                        RemoteSpec::parse(&check.to_string())
                            .map_err(|e| invalid(e.to_string()))?;
                    }
                }
            }
            _ => {}
        }
        if !matches!(self.destination, Destination::Download { .. })
            && self.sources.iter().any(|p| !p.is_absolute())
        {
            return Err(invalid("local sources must be absolute"));
        }
        Ok(())
    }

    /// Does not connect. Credentials live only inside the returned in-memory backend.
    pub fn request(&self, action: Action, password: String) -> io::Result<TransferRequest> {
        self.validate()?;
        let request = match &self.destination {
            Destination::Directory { path } => {
                TransferRequest::into_dir(self.sources.clone(), path)
            }
            Destination::File { path } => TransferRequest::file_to(&self.sources[0], path),
            Destination::Upload {
                target,
                fingerprint,
            }
            | Destination::Download {
                target,
                fingerprint,
                ..
            } => {
                if password.is_empty() {
                    return Err(invalid("enter the PC password or access key again"));
                }
                let spec = RemoteSpec::parse(target).map_err(|e| invalid(e.to_string()))?;
                let backend = Arc::new(
                    RemoteBackend::new(
                        Connector::new(spec)
                            .pinned(Some(fingerprint.clone()))
                            .password(password),
                    )?
                    .with_restart_files(action == Action::Retry),
                );
                match &self.destination {
                    Destination::Download { path, .. } => TransferRequest::to_backend(
                        self.sources.clone(),
                        Arc::new(PullBackend::new(backend, path.clone())),
                    ),
                    _ => TransferRequest::to_backend(self.sources.clone(), backend),
                }
            }
        };
        Ok(request
            .with_label(&self.label)
            .with_options(self.options(action)?))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    next_id: u64,
    jobs: Vec<Job>,
}

pub struct RecoveryStore {
    path: Option<PathBuf>,
    saved: Saved,
}

impl RecoveryStore {
    pub fn open(path: Option<PathBuf>) -> io::Result<Self> {
        let mut store = Self {
            path,
            saved: Saved {
                version: 1,
                next_id: 1,
                jobs: vec![],
            },
        };
        store.refresh()?;
        Ok(store)
    }

    fn lock(&self) -> io::Result<Option<std::fs::File>> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(path.with_extension("lock"))?;
            FileExt::lock(&file)?;
            Ok(Some(file))
        } else {
            Ok(None)
        }
    }

    fn read(&self) -> io::Result<Saved> {
        let saved = match self.path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(text)) => toml::from_str::<Saved>(&text).map_err(|e| invalid(e.to_string()))?,
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            Some(Err(_)) => Saved {
                version: 1,
                next_id: 1,
                jobs: vec![],
            },
            None => self.saved.clone(),
        };
        let mut ids = std::collections::HashSet::new();
        if saved.version != 1 || saved.next_id == 0 {
            return Err(invalid("invalid recovery version or ID sequence"));
        }
        for job in &saved.jobs {
            job.validate()?;
            if job.id == 0 || job.id >= saved.next_id || !ids.insert(job.id) {
                return Err(invalid("invalid recovery job ID"));
            }
        }
        Ok(saved)
    }

    pub fn refresh(&mut self) -> io::Result<()> {
        let _lock = self.lock()?;
        self.saved = self.read()?;
        Ok(())
    }

    /// Advisory claim released automatically on process exit; never store credentials in it.
    pub fn claim(&mut self, id: u64) -> io::Result<Option<std::fs::File>> {
        let _lock = self.lock()?;
        self.saved = self.read()?;
        if self.get(id).is_none() {
            return Err(invalid("saved job no longer exists"));
        }
        if let Some(path) = &self.path {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(path.with_extension(format!("job-{id}.lock")))?;
            FileExt::try_lock(&file).map_err(|e| {
                io::Error::other(format!(
                    "saved job is already active in another app or cannot be locked: {e}"
                ))
            })?;
            Ok(Some(file))
        } else {
            Ok(None)
        }
    }

    pub fn list(&self) -> &[Job] {
        &self.saved.jobs
    }
    pub fn get(&self, id: u64) -> Option<&Job> {
        self.list().iter().find(|j| j.id == id)
    }

    pub fn add(&mut self, mut job: Job) -> io::Result<u64> {
        job.validate()?;
        let _lock = self.lock()?;
        let mut saved = self.read()?;
        let id = saved.next_id;
        saved.next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid("recovery IDs exhausted"))?;
        job.id = id;
        saved.jobs.push(job);
        self.persist(saved)?;
        Ok(id)
    }

    pub fn remove(&mut self, id: u64) -> io::Result<()> {
        let _lock = self.lock()?;
        let mut saved = self.read()?;
        if !saved.jobs.iter().any(|job| job.id == id) {
            return Err(invalid("saved job no longer exists"));
        }
        saved.jobs.retain(|job| job.id != id);
        self.persist(saved)?;
        // Claims open only under the state lock and recheck existence, so no
        // other process can open this claim after the record is removed.
        if let Some(path) = &self.path {
            if let Err(error) = std::fs::remove_file(path.with_extension(format!("job-{id}.lock")))
            {
                if error.kind() != io::ErrorKind::NotFound {
                    eprintln!("cannot clean up finished recovery claim: {error}");
                }
            }
        }
        Ok(())
    }

    fn persist(&mut self, saved: Saved) -> io::Result<()> {
        if let Some(path) = &self.path {
            let text = toml::to_string(&saved).map_err(io::Error::other)?;
            crate::config::write_private(path, text.as_bytes())?;
        }
        self.saved = saved;
        Ok(())
    }
}

pub fn absolute(path: &Path) -> io::Result<PathBuf> {
    std::path::absolute(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(root: &Path) -> Job {
        let request = TransferRequest::into_dir(vec![root.join("source")], root.join("dest"));
        Job::new(
            &request,
            Destination::Directory {
                path: root.join("dest"),
            },
            Some("alice".into()),
            &CopyOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn all_directions_round_trip_without_credentials_or_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.toml");
        let mut store = RecoveryStore::open(Some(path.clone())).unwrap();
        let job = local(dir.path());
        store.add(job.clone()).unwrap();
        let mut exact = job.clone();
        exact.destination = Destination::File {
            path: dir.path().join("exact"),
        };
        store.add(exact).unwrap();
        let fingerprint = crate::crypto::fingerprint(b"trusted certificate");
        let target = "alice@127.0.0.1:9:/backup";
        for download in [false, true] {
            let mut remote = job.clone();
            remote.destination = if download {
                remote.sources = vec![PathBuf::from("backup/data")];
                Destination::Download {
                    target: target.into(),
                    fingerprint: fingerprint.clone(),
                    path: dir.path().join("out"),
                }
            } else {
                Destination::Upload {
                    target: target.into(),
                    fingerprint: fingerprint.clone(),
                }
            };
            assert!(remote.request(Action::Resume, String::new()).is_err());
            // Port 9 has no agent: reconstructing a request must not connect.
            drop(
                remote
                    .request(Action::Resume, "never-save-this-password".into())
                    .unwrap(),
            );
            store.add(remote).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for forbidden in [
            "never-save-this-password",
            "password",
            "token",
            "access_key",
        ] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} must not be persisted"
            );
        }
        let reopened = RecoveryStore::open(Some(path)).unwrap();
        assert_eq!(reopened.list().len(), 4);
        assert_eq!(reopened.list()[3].sources, [PathBuf::from("backup/data")]);
        assert_eq!(reopened.list()[2].describe(), target);
    }

    #[test]
    fn options_and_original_policy_survive_but_resume_always_compares() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = local(dir.path());
        job.options.conflict = "keep_both".into();
        job.options.algorithm = "sha256".into();
        job.options.verify = "readback".into();
        job.options.preserve_acls = true;
        job.options.space_reserve = 12345;
        let retry = job.request(Action::Retry, String::new()).unwrap();
        let options = retry.options.unwrap();
        assert_eq!(options.conflicts.unwrap().policy, ConflictPolicy::KeepBoth);
        assert_eq!(options.verify, VerifyMode::ReadBack);
        assert_eq!(options.algorithm, Algorithm::Sha256);
        assert!(options.preserve_acls);
        assert_eq!(options.space_reserve, 12345);
        assert_eq!(
            job.request(Action::Resume, String::new())
                .unwrap()
                .options
                .unwrap()
                .conflicts
                .unwrap()
                .policy,
            ConflictPolicy::Compare
        );
    }

    #[test]
    fn a_saved_move_comes_back_as_a_move_and_older_jobs_stay_copies() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = local(dir.path());
        assert!(!job.request(Action::Retry, String::new()).unwrap().options.unwrap().move_source);

        job.options.move_source = true;
        let saved = toml::to_string(&job).unwrap();
        let back: Job = toml::from_str(&saved).unwrap();
        assert!(back.request(Action::Retry, String::new()).unwrap().options.unwrap().move_source);

        // A job saved before move mode existed has no such field, and was a copy.
        let older = saved.replace("move_source = true\n", "");
        assert_ne!(older, saved, "the field was present to remove");
        let back: Job = toml::from_str(&older).unwrap();
        assert!(!back.request(Action::Retry, String::new()).unwrap().options.unwrap().move_source);
    }

    #[test]
    fn concurrent_stores_merge_updates_and_claims_prevent_duplicate_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.toml");
        let mut first = RecoveryStore::open(Some(path.clone())).unwrap();
        let mut second = RecoveryStore::open(Some(path)).unwrap();
        let id = first.add(local(dir.path())).unwrap();
        let next = second.add(local(dir.path())).unwrap();
        assert!(next > id);
        first.refresh().unwrap();
        assert_eq!(first.list().len(), 2);
        let claim = first.claim(id).unwrap();
        assert!(second.claim(id).is_err());
        drop(claim);
        let claim = second.claim(id).unwrap();
        second.remove(id).unwrap();
        drop(claim);
        assert!(
            first.claim(id).is_err(),
            "removed records must never be reconstructed from a stale snapshot"
        );
        first.remove(next).unwrap();
        assert!(second.add(local(dir.path())).unwrap() > next);
    }

    #[test]
    fn corrupt_unknown_or_unsafe_jobs_are_errors_not_empty_queues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.toml");
        for text in [
            "broken = [",
            "version = 2\nnext_id = 1\njobs = []",
            "version = 1\nnext_id = 1\njobs = []\npassword = 'secret'",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(RecoveryStore::open(Some(path.clone())).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        let mut job = local(dir.path());
        job.sources = vec![PathBuf::from("relative")];
        assert!(job.validate().is_err());
        job.sources = vec![dir.path().join("source")];
        job.destination = Destination::Upload {
            target: "host:/../escape".into(),
            fingerprint: crate::crypto::fingerprint(b"certificate"),
        };
        assert!(job.validate().is_err());
    }

    #[test]
    fn failed_writes_preserve_disk_memory_and_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.toml");
        let mut store = RecoveryStore::open(Some(path.clone())).unwrap();
        let id = store.add(local(dir.path())).unwrap();
        let before = std::fs::read(&path).unwrap();
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(store.add(local(dir.path())).is_err());
        assert!(store.remove(id).is_err());
        assert_eq!(store.list().len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir(path.with_extension("tmp")).unwrap();
        assert_eq!(store.add(local(dir.path())).unwrap(), id + 1);
    }

    #[test]
    fn discard_does_not_touch_data_or_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RecoveryStore::open(Some(dir.path().join("jobs.toml"))).unwrap();
        let job = local(dir.path());
        std::fs::create_dir(dir.path().join("dest")).unwrap();
        let copied = dir.path().join("dest/copied");
        let partial = dir.path().join("dest/data.fcpart");
        std::fs::write(&copied, b"completed").unwrap();
        std::fs::write(&partial, b"partial").unwrap();
        let id = store.add(job).unwrap();
        store.remove(id).unwrap();
        assert_eq!(std::fs::read(copied).unwrap(), b"completed");
        assert_eq!(std::fs::read(partial).unwrap(), b"partial");
    }
}
