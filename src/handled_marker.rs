use crate::domain::CommentMention;
use crate::json_file;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// A mention whose response was posted but whose handled reaction may be missing.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PendingHandledMarker {
    pub api_url: String,
}

impl PendingHandledMarker {
    pub fn for_mention(mention: &CommentMention) -> Self {
        Self {
            api_url: mention.api_url.clone(),
        }
    }
}

pub trait PendingHandledMarkerStore: Send + Sync {
    fn record(&self, marker: &PendingHandledMarker) -> Result<()>;
    fn contains(&self, marker: &PendingHandledMarker) -> Result<bool>;
    fn remove(&self, marker: &PendingHandledMarker) -> Result<()>;
    fn pending(&self) -> Result<Vec<PendingHandledMarker>>;
    /// Whether this ledger recorded a posted auto-review before the reviewed-head ledger existed.
    fn has_legacy_pull_request(&self, html_url: &str) -> Result<bool>;
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

    fn has_legacy_pull_request(&self, html_url: &str) -> Result<bool> {
        Ok(self
            .ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .pull_request_html_urls
            .contains(html_url))
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
        let mut ledger = json_file::read::<PendingHandledMarkerLedger>(&self.path)?;
        ledger.insert(marker);
        json_file::write(&self.path, &ledger)
    }

    fn contains(&self, marker: &PendingHandledMarker) -> Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        Ok(json_file::read::<PendingHandledMarkerLedger>(&self.path)?.contains(marker))
    }

    fn remove(&self, marker: &PendingHandledMarker) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        let mut ledger = json_file::read::<PendingHandledMarkerLedger>(&self.path)?;
        ledger.remove(marker);
        json_file::write(&self.path, &ledger)
    }

    fn pending(&self) -> Result<Vec<PendingHandledMarker>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        Ok(json_file::read::<PendingHandledMarkerLedger>(&self.path)?.pending())
    }

    fn has_legacy_pull_request(&self, html_url: &str) -> Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        Ok(json_file::read::<PendingHandledMarkerLedger>(&self.path)?
            .pull_request_html_urls
            .contains(html_url))
    }
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct PendingHandledMarkerLedger {
    mention_api_urls: BTreeSet<String>,
    /// Kept so writes do not drop the posted auto-reviews it recorded; nothing adds to it.
    #[serde(default)]
    pull_request_html_urls: BTreeSet<String>,
}

impl PendingHandledMarkerLedger {
    fn insert(&mut self, marker: &PendingHandledMarker) {
        self.mention_api_urls.insert(marker.api_url.clone());
    }

    fn contains(&self, marker: &PendingHandledMarker) -> bool {
        self.mention_api_urls.contains(&marker.api_url)
    }

    fn remove(&mut self, marker: &PendingHandledMarker) {
        self.mention_api_urls.remove(&marker.api_url);
    }

    fn pending(&self) -> Vec<PendingHandledMarker> {
        self.mention_api_urls
            .iter()
            .cloned()
            .map(|api_url| PendingHandledMarker { api_url })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_persists_pending_markers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pending-handled-markers.json");
        let marker = PendingHandledMarker {
            api_url: "https://api.github.com/repos/o/r/issues/comments/2".to_string(),
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

    #[test]
    fn keeps_legacy_pull_request_markers_across_writes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pending-handled-markers.json");
        std::fs::write(
            &path,
            r#"{"mention_api_urls":[],"pull_request_html_urls":["https://github.com/o/r/pull/1"]}"#,
        )
        .unwrap();
        let store = FilePendingHandledMarkerStore::new(&path);

        store
            .record(&PendingHandledMarker {
                api_url: "https://api.github.com/c/2".to_string(),
            })
            .unwrap();

        assert!(
            store
                .has_legacy_pull_request("https://github.com/o/r/pull/1")
                .unwrap()
        );
    }
}
