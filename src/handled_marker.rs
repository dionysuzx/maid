use crate::domain::{CommentMention, PullRequest};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnflushedPost {
    pub pr: String,
    pub sha: String,
    pub model: String,
    pub prompt_hash: String,
    pub effort: String,
    pub started_unix: u64,
    pub finished_unix: u64,
    pub comment_url: String,
}

pub trait PendingHandledMarkerStore: Send + Sync {
    fn record(&self, marker: &PendingHandledMarker) -> Result<()>;
    fn contains(&self, marker: &PendingHandledMarker) -> Result<bool>;
    fn remove(&self, marker: &PendingHandledMarker) -> Result<()>;
    fn pending(&self) -> Result<Vec<PendingHandledMarker>>;
    fn remember_post(&self, post: &UnflushedPost) -> Result<()>;
    fn unflushed_posts(&self) -> Result<Vec<UnflushedPost>>;
    fn forget_post(&self, pr: &str, sha: &str) -> Result<()>;
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

    fn remember_post(&self, post: &UnflushedPost) -> Result<()> {
        self.ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .remember_post(post);
        Ok(())
    }

    fn unflushed_posts(&self) -> Result<Vec<UnflushedPost>> {
        Ok(self
            .ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .unflushed_posts())
    }

    fn forget_post(&self, pr: &str, sha: &str) -> Result<()> {
        self.ledger
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?
            .forget_post(pr, sha);
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

    fn pending(&self) -> Result<Vec<PendingHandledMarker>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        Ok(PendingHandledMarkerLedger::read(&self.path)?.pending())
    }

    fn remember_post(&self, post: &UnflushedPost) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        let mut ledger = PendingHandledMarkerLedger::read(&self.path)?;
        ledger.remember_post(post);
        ledger.write(&self.path)
    }

    fn unflushed_posts(&self) -> Result<Vec<UnflushedPost>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        Ok(PendingHandledMarkerLedger::read(&self.path)?.unflushed_posts())
    }

    fn forget_post(&self, pr: &str, sha: &str) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("pending handled marker lock is poisoned"))?;
        let mut ledger = PendingHandledMarkerLedger::read(&self.path)?;
        ledger.forget_post(pr, sha);
        ledger.write(&self.path)
    }
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct PendingHandledMarkerLedger {
    mention_api_urls: BTreeSet<String>,
    pull_request_html_urls: BTreeSet<String>,
    #[serde(default)]
    unflushed_posts: Vec<UnflushedPost>,
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

    fn remember_post(&mut self, post: &UnflushedPost) {
        if let Some(existing) = self
            .unflushed_posts
            .iter_mut()
            .find(|kept| kept.pr == post.pr && kept.sha == post.sha)
        {
            *existing = post.clone();
        } else {
            self.unflushed_posts.push(post.clone());
        }
    }

    fn unflushed_posts(&self) -> Vec<UnflushedPost> {
        self.unflushed_posts.clone()
    }

    fn forget_post(&mut self, pr: &str, sha: &str) {
        self.unflushed_posts
            .retain(|post| post.pr != pr || post.sha != sha);
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

    #[test]
    fn file_store_keeps_an_unflushed_post_across_reload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pending-handled-markers.json");
        let marker = PendingHandledMarker::PullRequest {
            html_url: "https://github.com/o/r/pull/1".to_string(),
        };
        fs::write(
            &path,
            r#"{"mention_api_urls":[],"pull_request_html_urls":["https://github.com/o/r/pull/1"]}"#,
        )
        .unwrap();

        let store = FilePendingHandledMarkerStore::new(&path);
        assert!(store.contains(&marker).unwrap());
        assert!(store.unflushed_posts().unwrap().is_empty());

        let post = UnflushedPost {
            pr: "https://github.com/o/r/pull/1".to_string(),
            sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            model: "gpt-5.5".to_string(),
            prompt_hash: "ab".repeat(32),
            effort: "high".to_string(),
            started_unix: 1,
            finished_unix: 2,
            comment_url: "https://github.com/o/r/pull/1#issuecomment-1".to_string(),
        };
        store.remember_post(&post).unwrap();

        let reloaded = FilePendingHandledMarkerStore::new(&path);
        assert_eq!(reloaded.unflushed_posts().unwrap(), vec![post.clone()]);
        assert!(reloaded.contains(&marker).unwrap());

        let replaced = UnflushedPost {
            comment_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
            ..post.clone()
        };
        reloaded.remember_post(&replaced).unwrap();
        assert_eq!(store.unflushed_posts().unwrap(), vec![replaced]);

        store
            .forget_post(
                "https://github.com/o/r/pull/1",
                "0123456789abcdef0123456789abcdef01234567",
            )
            .unwrap();
        assert!(reloaded.unflushed_posts().unwrap().is_empty());
        assert!(store.contains(&marker).unwrap());
    }
}
