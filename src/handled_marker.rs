use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PendingHandledMarker {
    Mention { api_url: String },
    PullRequest { html_url: String },
}

pub trait PendingHandledMarkerStore: Send + Sync {
    fn record(&self, marker: &PendingHandledMarker) -> Result<()>;
    fn contains(&self, marker: &PendingHandledMarker) -> Result<bool>;
    fn remove(&self, marker: &PendingHandledMarker) -> Result<()>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryPendingHandledMarkerStore {
    ledger: Arc<Mutex<PendingHandledMarkerLedger>>,
}

impl PendingHandledMarkerStore for MemoryPendingHandledMarkerStore {
    fn record(&self, marker: &PendingHandledMarker) -> Result<()> {
        self.ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .insert(marker);
        Ok(())
    }

    fn contains(&self, marker: &PendingHandledMarker) -> Result<bool> {
        Ok(self
            .ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .contains(marker))
    }

    fn remove(&self, marker: &PendingHandledMarker) -> Result<()> {
        self.ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .remove(marker);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FilePendingHandledMarkerStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl FilePendingHandledMarkerStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }
}

impl PendingHandledMarkerStore for FilePendingHandledMarkerStore {
    fn record(&self, marker: &PendingHandledMarker) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        let mut ledger = PendingHandledMarkerLedger::read(&self.path)?;
        ledger.insert(marker);
        ledger.write(&self.path)
    }

    fn contains(&self, marker: &PendingHandledMarker) -> Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        Ok(PendingHandledMarkerLedger::read(&self.path)?.contains(marker))
    }

    fn remove(&self, marker: &PendingHandledMarker) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        let mut ledger = PendingHandledMarkerLedger::read(&self.path)?;
        ledger.remove(marker);
        ledger.write(&self.path)
    }
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct PendingHandledMarkerLedger {
    mention_api_urls: BTreeSet<String>,
    pull_request_html_urls: BTreeSet<String>,
}

impl PendingHandledMarkerLedger {
    fn read(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).with_context(|| {
                format!(
                    "failed to parse pending handled marker ledger {}",
                    path.display()
                )
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn insert(&mut self, marker: &PendingHandledMarker) {
        match marker {
            PendingHandledMarker::Mention { api_url } => {
                self.mention_api_urls.insert(api_url.clone());
            }
            PendingHandledMarker::PullRequest { html_url } => {
                self.pull_request_html_urls.insert(html_url.clone());
            }
        }
    }

    fn contains(&self, marker: &PendingHandledMarker) -> bool {
        match marker {
            PendingHandledMarker::Mention { api_url } => self.mention_api_urls.contains(api_url),
            PendingHandledMarker::PullRequest { html_url } => {
                self.pull_request_html_urls.contains(html_url)
            }
        }
    }

    fn remove(&mut self, marker: &PendingHandledMarker) {
        match marker {
            PendingHandledMarker::Mention { api_url } => {
                self.mention_api_urls.remove(api_url);
            }
            PendingHandledMarker::PullRequest { html_url } => {
                self.pull_request_html_urls.remove(html_url);
            }
        }
    }

    fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let temp_path = path.with_file_name(format!(
            ".{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("maid-pending-handled-markers.json")
        ));
        let contents = serde_json::to_vec_pretty(self).context("failed to encode marker ledger")?;
        fs::write(&temp_path, contents)
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        fs::rename(&temp_path, path).with_context(|| {
            format!(
                "failed to replace {} with {}",
                path.display(),
                temp_path.display()
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_persists_pending_markers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pending-handled-markers.json");
        let marker = PendingHandledMarker::PullRequest {
            html_url: "https://github.com/o/r/pull/1".to_string(),
        };

        let store = FilePendingHandledMarkerStore::new(&path);
        store.record(&marker).unwrap();

        let reloaded = FilePendingHandledMarkerStore::new(&path);
        assert!(reloaded.contains(&marker).unwrap());

        reloaded.remove(&marker).unwrap();
        assert!(!store.contains(&marker).unwrap());
    }
}
