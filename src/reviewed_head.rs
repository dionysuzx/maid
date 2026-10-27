use crate::domain::{CommitSha, PullRequest};
use crate::json_file;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// The last head commit Maid posted an automatic review for, per pull request.
pub trait ReviewedHeadStore: Send + Sync {
    fn last_reviewed(&self, pr: &PullRequest) -> Result<Option<CommitSha>>;
    fn record(&self, pr: &PullRequest) -> Result<()>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryReviewedHeadStore {
    ledger: Arc<Mutex<ReviewedHeadLedger>>,
}

impl ReviewedHeadStore for MemoryReviewedHeadStore {
    fn last_reviewed(&self, pr: &PullRequest) -> Result<Option<CommitSha>> {
        Ok(self
            .ledger
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?
            .last_reviewed(pr))
    }

    fn record(&self, pr: &PullRequest) -> Result<()> {
        self.ledger
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?
            .record(pr);
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
    fn last_reviewed(&self, pr: &PullRequest) -> Result<Option<CommitSha>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?;
        Ok(json_file::read::<ReviewedHeadLedger>(&self.path)?.last_reviewed(pr))
    }

    fn record(&self, pr: &PullRequest) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("reviewed head lock is poisoned"))?;
        let mut ledger = json_file::read::<ReviewedHeadLedger>(&self.path)?;
        ledger.record(pr);
        json_file::write(&self.path, &ledger)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ReviewedHeadLedger {
    heads_by_pull_request: BTreeMap<String, CommitSha>,
}

impl ReviewedHeadLedger {
    fn last_reviewed(&self, pr: &PullRequest) -> Option<CommitSha> {
        self.heads_by_pull_request.get(&pr.html_url).cloned()
    }

    fn record(&mut self, pr: &PullRequest) {
        self.heads_by_pull_request
            .insert(pr.html_url.clone(), pr.head.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_keeps_the_last_reviewed_head_across_restarts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reviewed-heads.json");
        let first = pr_at('a');
        let second = pr_at('b');

        assert_eq!(
            FileReviewedHeadStore::new(&path)
                .last_reviewed(&first)
                .unwrap(),
            None
        );
        FileReviewedHeadStore::new(&path).record(&first).unwrap();
        FileReviewedHeadStore::new(&path).record(&second).unwrap();

        assert_eq!(
            FileReviewedHeadStore::new(&path)
                .last_reviewed(&first)
                .unwrap(),
            Some(second.head)
        );
    }

    fn pr_at(digit: char) -> PullRequest {
        PullRequest {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 1,
            author: "dionysuzx".to_string(),
            api_url: "https://api.github.com/repos/o/r/pulls/1".to_string(),
            html_url: "https://github.com/o/r/pull/1".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
            head: CommitSha::parse(&digit.to_string().repeat(40)).unwrap(),
        }
    }
}
