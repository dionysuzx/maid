use crate::domain::{CommentMention, PullRequest};
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

impl PendingHandledMarker {
    pub fn for_mention(mention: &CommentMention) -> Self {
        Self::Mention {
            api_url: mention.api_url.clone(),
        }
    }

    pub fn for_pull_request(pr: &PullRequest) -> Self {
        Self::PullRequest {
            html_url: pr.html_url.clone(),
        }
    }
}

pub trait PendingHandledMarkerStore: Send + Sync {
    fn record(&self, marker: &PendingHandledMarker) -> Result<()>;
    fn contains(&self, marker: &PendingHandledMarker) -> Result<bool>;
    fn remove(&self, marker: &PendingHandledMarker) -> Result<()>;
    fn pending(&self) -> Result<Vec<PendingHandledMarker>>;
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

    fn pending(&self) -> Result<Vec<PendingHandledMarker>> {
        Ok(self
            .ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .pending())
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

    fn pending(&self) -> Result<Vec<PendingHandledMarker>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        Ok(PendingHandledMarkerLedger::read(&self.path)?.pending())
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

    fn pending(&self) -> Vec<PendingHandledMarker> {
        self.mention_api_urls
            .iter()
            .cloned()
            .map(|api_url| PendingHandledMarker::Mention { api_url })
            .chain(
                self.pull_request_html_urls
                    .iter()
                    .cloned()
                    .map(|html_url| PendingHandledMarker::PullRequest { html_url }),
            )
            .collect()
    }

    fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;

        let mut file = tempfile::Builder::new()
            .prefix(".maid-pending-handled-markers.")
            .tempfile_in(parent)
            .with_context(|| {
                format!(
                    "failed to create temp marker ledger in {}",
                    parent.display()
                )
            })?;

        serde_json::to_writer_pretty(&mut file, self).context("failed to encode marker ledger")?;
        file.as_file()
            .sync_all()
            .with_context(|| format!("failed to sync {}", path.display()))?;
        file.into_temp_path()
            .persist(path)
            .map_err(|err| err.error)
            .with_context(|| format!("failed to replace {}", path.display()))?;
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
        assert_eq!(reloaded.pending().unwrap(), vec![marker.clone()]);

        reloaded.remove(&marker).unwrap();
        assert!(!store.contains(&marker).unwrap());
        assert!(reloaded.pending().unwrap().is_empty());
    }
}
