//! Finished-job summaries only: never serialize a backend or its credentials.

use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use fc_engine::{JobState, JobSummary, Target, TransferRequest};
use fs4::FileExt;
use serde::{Deserialize, Serialize};

pub const LIMIT: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    CompletedWithErrors,
    Failed,
    Cancelled,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::CompletedWithErrors => "Completed with errors",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: u64,
    pub owner: Option<String>,
    pub label: String,
    pub sources: Vec<String>,
    pub destination: String,
    pub remote: bool,
    pub queued_at_ms: u64,
    pub finished_at_ms: u64,
    pub elapsed_ms: u64,
    pub outcome: Outcome,
    pub files_copied: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub bytes_copied: u64,
}

/// A whitelist captured before the worker starts, independent of queue job IDs.
pub struct Pending {
    label: String,
    owner: Option<String>,
    sources: Vec<String>,
    destination: String,
    remote: bool,
    queued_at_ms: u64,
}

impl Pending {
    pub fn new(request: &TransferRequest, owner: Option<String>) -> Self {
        Self {
            label: request.display_label(),
            owner,
            sources: request.sources.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
            destination: request.target.describe(),
            remote: matches!(request.target, Target::Backend(_)),
            queued_at_ms: now_ms(),
        }
    }

    pub fn finish(self, state: &JobState, summary: &JobSummary) -> io::Result<Entry> {
        let outcome = match state {
            JobState::Completed => Outcome::Completed,
            JobState::CompletedWithErrors => Outcome::CompletedWithErrors,
            JobState::Failed(_) => Outcome::Failed,
            JobState::Cancelled => Outcome::Cancelled,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "only finished jobs belong in history")),
        };
        Ok(Entry {
            id: 0,
            owner: self.owner,
            label: self.label,
            sources: self.sources,
            destination: self.destination,
            remote: self.remote,
            queued_at_ms: self.queued_at_ms,
            finished_at_ms: now_ms(),
            elapsed_ms: summary.elapsed.as_millis().min(u64::MAX as u128) as u64,
            outcome,
            files_copied: summary.files_copied,
            files_skipped: summary.files_skipped,
            files_failed: summary.files_failed,
            bytes_copied: summary.bytes_copied,
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    next_id: u64,
    entries: Vec<Entry>,
}

impl Default for Saved {
    fn default() -> Self {
        Self { version: 1, next_id: 1, entries: Vec::new() }
    }
}

pub struct HistoryStore {
    path: Option<PathBuf>,
    memory: Saved,
    pending: Vec<Entry>,
}

impl HistoryStore {
    pub fn open(path: Option<PathBuf>) -> io::Result<Self> {
        let mut store = Self { path, memory: Saved::default(), pending: Vec::new() };
        store.list()?;
        Ok(store)
    }

    /// Failed writes remain pending for the next operation to retry.
    pub fn record(&mut self, entry: Entry) -> io::Result<()> {
        self.pending.push(entry);
        if self.pending.len() > LIMIT {
            self.pending.drain(..self.pending.len() - LIMIT);
        }
        self.transaction(|_| ())
    }

    /// Newest first. Disk state is refreshed under the cross-process lock.
    pub fn list(&mut self) -> io::Result<Vec<Entry>> {
        self.transaction(|saved| saved.entries.iter().rev().cloned().collect())
    }

    /// `None` clears all owners; `Some` clears only that owner's entries.
    pub fn clear(&mut self, owner: Option<&str>) -> io::Result<()> {
        self.transaction(|saved| {
            saved.entries.retain(|entry| owner.is_some_and(|owner| entry.owner.as_deref() != Some(owner)));
        })
    }

    fn transaction<T>(&mut self, update: impl FnOnce(&mut Saved) -> T) -> io::Result<T> {
        let _lock = if let Some(path) = &self.path {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(path.with_extension("toml.lock"))?;
            FileExt::lock(&file)?;
            Some(file)
        } else { None };
        let mut saved = match &self.path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => toml::from_str::<Saved>(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => Saved::default(),
                Err(e) => return Err(e),
            },
            None => self.memory.clone(),
        };
        if saved.version != 1 || saved.next_id == 0 || saved.entries.len() > LIMIT
            || saved.entries.iter().any(|e| e.id == 0 || e.id >= saved.next_id)
            || saved.entries.windows(2).any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid transfer history"));
        }
        let before_len = saved.entries.len();
        for entry in &self.pending {
            let mut entry = entry.clone();
            entry.id = saved.next_id;
            saved.next_id = saved.next_id.checked_add(1).ok_or_else(|| io::Error::other("history IDs exhausted"))?;
            saved.entries.push(entry);
        }
        if saved.entries.len() > LIMIT {
            saved.entries.drain(..saved.entries.len() - LIMIT);
        }
        let result = update(&mut saved);
        if !self.pending.is_empty() || saved.entries.len() != before_len {
            if let Some(path) = &self.path {
                let text = toml::to_string(&saved).map_err(io::Error::other)?;
                crate::config::write_private(path, text.as_bytes())?;
            }
        }
        self.memory = saved;
        self.pending.clear();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(owner: &str) -> Entry {
        let request = TransferRequest::file_to("source", "destination").with_label("Example");
        Pending::new(&request, Some(owner.into())).finish(
            &JobState::Completed,
            &JobSummary { files_copied: 2, bytes_copied: 42, ..Default::default() },
        ).unwrap()
    }

    #[test]
    fn history_restarts_ids_retention_and_scoped_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.toml");
        let mut store = HistoryStore::open(Some(path.clone())).unwrap();
        // One transaction keeps this test focused on retention rather than fsync speed.
        store.pending = (0..LIMIT + 3).map(|n| entry(if n % 2 == 0 { "alice" } else { "bob" })).collect();
        assert_eq!(store.list().unwrap().len(), LIMIT);
        let mut reopened = HistoryStore::open(Some(path)).unwrap();
        let entries = reopened.list().unwrap();
        assert_eq!(entries.first().unwrap().id, (LIMIT + 3) as u64);
        assert_eq!(entries.last().unwrap().id, 4);
        reopened.clear(Some("alice")).unwrap();
        assert!(reopened.list().unwrap().iter().all(|e| e.owner.as_deref() == Some("bob")));
        reopened.clear(None).unwrap();
        reopened.record(entry("alice")).unwrap();
        assert_eq!(reopened.list().unwrap()[0].id, (LIMIT + 4) as u64);
    }

    #[test]
    fn history_multiple_instances_do_not_lose_each_others_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.toml");
        let mut a = HistoryStore::open(Some(path.clone())).unwrap();
        let mut b = HistoryStore::open(Some(path)).unwrap();
        a.record(entry("alice")).unwrap();
        b.record(entry("bob")).unwrap();
        a.record(entry("alice")).unwrap();
        assert_eq!(b.list().unwrap().len(), 3);
        let paths = [a.path.clone(), b.path.clone()];
        let writers: Vec<_> = paths.into_iter().map(|path| std::thread::spawn(move || {
            let mut store = HistoryStore::open(path).unwrap();
            for _ in 0..5 { store.record(entry("concurrent")).unwrap(); }
        })).collect();
        for writer in writers { writer.join().unwrap(); }
        assert_eq!(a.list().unwrap().len(), 13);
    }

    #[test]
    fn history_corruption_and_failed_writes_are_errors_and_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.toml");
        let mut store = HistoryStore::open(Some(path.clone())).unwrap();
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(store.record(entry("alice")).is_err());
        std::fs::remove_dir(path.with_extension("tmp")).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.list().unwrap().len(), 1, "retry does not duplicate");
        std::fs::write(&path, "broken").unwrap();
        assert!(store.record(entry("bob")).is_err());
        assert!(HistoryStore::open(Some(path.clone())).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
    }

    #[test]
    fn history_is_an_explicit_summary_without_credentials_or_raw_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.toml");
        let mut store = HistoryStore::open(Some(path.clone())).unwrap();
        let pending = Pending::new(&TransferRequest::file_to("a", "b"), None);
        let e = pending.finish(&JobState::Failed("secret-error-text".into()), &JobSummary::default()).unwrap();
        store.record(e).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("secret-error-text") && !text.contains("password") && !text.contains("fingerprint"));
        assert_eq!(store.list().unwrap()[0].outcome, Outcome::Failed);
    }

    #[test]
    fn history_memory_mode_preserves_all_terminal_outcomes_and_summary_counts() {
        let mut store = HistoryStore::open(None).unwrap();
        for state in [JobState::Completed, JobState::CompletedWithErrors, JobState::Failed("not saved".into()), JobState::Cancelled] {
            let pending = Pending::new(&TransferRequest::file_to("a", "b"), None);
            store.record(pending.finish(&state, &JobSummary {
                files_copied: 2, files_skipped: 3, files_failed: 4, bytes_copied: 42,
                elapsed: std::time::Duration::from_millis(1_234), ..Default::default()
            }).unwrap()).unwrap();
        }
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries.iter().map(|e| e.outcome).collect::<Vec<_>>(), vec![
            Outcome::Cancelled, Outcome::Failed, Outcome::CompletedWithErrors, Outcome::Completed,
        ]);
        assert_eq!((entries[0].files_copied, entries[0].files_skipped, entries[0].files_failed, entries[0].bytes_copied, entries[0].elapsed_ms), (2, 3, 4, 42, 1_234));
        let pending = Pending::new(&TransferRequest::file_to("a", "b"), None);
        assert!(pending.finish(&JobState::Running, &Default::default()).is_err());
        store.clear(None).unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}
