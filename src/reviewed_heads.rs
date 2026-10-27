use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub trait ReviewedHeadStore: Send + Sync {
    fn contains(&self, pr_url: &str, head: &str) -> Result<bool>;
    fn record(&self, pr_url: &str, head: &str) -> Result<()>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryReviewedHeadStore {
    heads: Arc<Mutex<BTreeSet<ReviewedHead>>>,
}

impl ReviewedHeadStore for MemoryReviewedHeadStore {
    fn contains(&self, pr_url: &str, head: &str) -> Result<bool> {
        Ok(self
            .heads
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?
            .contains(&ReviewedHead::new(pr_url, head)))
    }

    fn record(&self, pr_url: &str, head: &str) -> Result<()> {
        self.heads
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?
            .insert(ReviewedHead::new(pr_url, head));
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FileReviewedHeadStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl FileReviewedHeadStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }
}

impl ReviewedHeadStore for FileReviewedHeadStore {
    fn contains(&self, pr_url: &str, head: &str) -> Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?;
        Ok(ReviewedHeadLedger::read(&self.path)?.contains(pr_url, head))
    }

    fn record(&self, pr_url: &str, head: &str) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?;
        let mut ledger = ReviewedHeadLedger::read(&self.path)?;
        ledger.record(pr_url, head);
        ledger.write(&self.path)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
struct ReviewedHead {
    pr_url: String,
    head: String,
}

impl ReviewedHead {
    fn new(pr_url: &str, head: &str) -> Self {
        Self {
            pr_url: pr_url.to_string(),
            head: head.to_string(),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ReviewedHeadLedger {
    heads: BTreeSet<ReviewedHead>,
}

impl ReviewedHeadLedger {
    fn contains(&self, pr_url: &str, head: &str) -> bool {
        self.heads.contains(&ReviewedHead::new(pr_url, head))
    }

    fn record(&mut self, pr_url: &str, head: &str) {
        self.heads.insert(ReviewedHead::new(pr_url, head));
    }

    fn read(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse reviewed heads {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let mut file = tempfile::Builder::new()
            .prefix(".maid-reviewed-heads.")
            .tempfile_in(parent)
            .with_context(|| {
                format!(
                    "failed to create temp reviewed heads in {}",
                    parent.display()
                )
            })?;
        serde_json::to_writer_pretty(&mut file, self).context("failed to encode reviewed heads")?;
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
