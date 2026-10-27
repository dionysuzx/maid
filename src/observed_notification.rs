use crate::domain::Notification;
use crate::json_file;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub trait ObservedNotificationStore: Send + Sync {
    fn contains_current(&self, notification: &Notification) -> Result<bool>;
    fn record_current(&self, notification: &Notification) -> Result<()>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryObservedNotificationStore {
    ledger: Arc<Mutex<ObservedNotificationLedger>>,
}

impl ObservedNotificationStore for MemoryObservedNotificationStore {
    fn contains_current(&self, notification: &Notification) -> Result<bool> {
        Ok(self
            .ledger
            .lock()
            .map_err(|_| anyhow!("observed notification lock is poisoned"))?
            .contains_current(notification))
    }

    fn record_current(&self, notification: &Notification) -> Result<()> {
        self.ledger
            .lock()
            .map_err(|_| anyhow!("observed notification lock is poisoned"))?
            .record_current(notification);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FileObservedNotificationStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl FileObservedNotificationStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }
}

impl ObservedNotificationStore for FileObservedNotificationStore {
    fn contains_current(&self, notification: &Notification) -> Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("observed notification lock is poisoned"))?;
        Ok(json_file::read::<ObservedNotificationLedger>(&self.path)?
            .contains_current(notification))
    }

    fn record_current(&self, notification: &Notification) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("observed notification lock is poisoned"))?;
        let mut ledger = json_file::read::<ObservedNotificationLedger>(&self.path)?;
        ledger.record_current(notification);
        json_file::write(&self.path, &ledger)
    }
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct ObservedNotificationLedger {
    notifications: BTreeMap<String, ObservedNotificationRecord>,
}

impl ObservedNotificationLedger {
    fn contains_current(&self, notification: &Notification) -> bool {
        self.notifications
            .get(&notification.id)
            .is_some_and(|record| record.matches(notification))
    }

    fn record_current(&mut self, notification: &Notification) {
        self.notifications.insert(
            notification.id.clone(),
            ObservedNotificationRecord {
                updated_at: notification.updated_at.clone(),
                latest_comment_url: notification.latest_comment_url.clone(),
            },
        );
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ObservedNotificationRecord {
    updated_at: String,
    latest_comment_url: Option<String>,
}

impl ObservedNotificationRecord {
    fn matches(&self, notification: &Notification) -> bool {
        self.updated_at == notification.updated_at
            && self.latest_comment_url == notification.latest_comment_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_persists_current_notification_observations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("observed-notifications.json");
        let current = notification("n1", "2026-06-08T04:00:00Z");

        let store = FileObservedNotificationStore::new(&path);
        store.record_current(&current).unwrap();

        let reloaded = FileObservedNotificationStore::new(&path);
        assert!(reloaded.contains_current(&current).unwrap());
        assert!(
            !reloaded
                .contains_current(&notification("n1", "2026-06-08T04:05:00Z"))
                .unwrap()
        );
    }

    fn notification(id: &str, updated_at: &str) -> Notification {
        Notification {
            id: id.to_string(),
            reason: "mention".to_string(),
            subject_kind: "PullRequest".to_string(),
            subject_url: Some("https://api.github.com/repos/o/r/pulls/1".to_string()),
            latest_comment_url: Some(
                "https://api.github.com/repos/o/r/issues/comments/1".to_string(),
            ),
            unread: false,
            updated_at: updated_at.to_string(),
        }
    }
}
