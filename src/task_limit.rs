use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TASK_LIMIT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

pub trait TaskStartRecorder: Send + Sync {
    fn try_record_started(&self) -> Result<TaskStartDecision>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStartDecision {
    Recorded,
    AtLimit { limit: usize, window: Duration },
}

#[derive(Clone, Debug, Default)]
pub struct NoTaskLimit;

impl TaskStartRecorder for NoTaskLimit {
    fn try_record_started(&self) -> Result<TaskStartDecision> {
        Ok(TaskStartDecision::Recorded)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TaskLimit {
    max_started_tasks: usize,
    window: Duration,
}

impl TaskLimit {
    fn per_24_hours(max_started_tasks: usize) -> Self {
        Self {
            max_started_tasks,
            window: TASK_LIMIT_WINDOW,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileTaskStartRecorder {
    limit: Option<TaskLimit>,
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl FileTaskStartRecorder {
    pub fn new(max_started_tasks_per_24h: Option<usize>, path: impl Into<PathBuf>) -> Self {
        Self {
            limit: max_started_tasks_per_24h.map(TaskLimit::per_24_hours),
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }
}

impl TaskStartRecorder for FileTaskStartRecorder {
    fn try_record_started(&self) -> Result<TaskStartDecision> {
        let Some(limit) = self.limit else {
            return Ok(TaskStartDecision::Recorded);
        };

        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("task start ledger lock is poisoned"))?;
        let now = unix_seconds(SystemTime::now())?;
        let mut ledger = TaskStartLedgerFile::read(&self.path)?;
        ledger.retain_window(now, limit.window);

        if ledger.started_at_unix_seconds.len() >= limit.max_started_tasks {
            ledger.write(&self.path)?;
            return Ok(TaskStartDecision::AtLimit {
                limit: limit.max_started_tasks,
                window: limit.window,
            });
        }

        ledger.started_at_unix_seconds.push(now);
        ledger.write(&self.path)?;
        Ok(TaskStartDecision::Recorded)
    }
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct TaskStartLedgerFile {
    started_at_unix_seconds: Vec<u64>,
}

impl TaskStartLedgerFile {
    fn read(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse task start ledger {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn retain_window(&mut self, now: u64, window: Duration) {
        let window_seconds = window.as_secs();
        self.started_at_unix_seconds
            .retain(|started_at| now.saturating_sub(*started_at) < window_seconds);
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
                .unwrap_or("maid-task-starts.json")
        ));
        let contents = serde_json::to_vec_pretty(self).context("failed to encode task ledger")?;
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

fn unix_seconds(time: SystemTime) -> Result<u64> {
    Ok(time
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_starts_outside_the_24_hour_window() {
        let mut ledger = TaskStartLedgerFile {
            started_at_unix_seconds: vec![99, 100, 101],
        };

        ledger.retain_window(100 + TASK_LIMIT_WINDOW.as_secs(), TASK_LIMIT_WINDOW);

        assert_eq!(
            ledger.started_at_unix_seconds,
            vec![101],
            "timestamps exactly one window old no longer count"
        );
    }

    #[test]
    fn file_recorder_persists_the_rolling_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("task-starts.json");
        let recorder = FileTaskStartRecorder::new(Some(1), &path);

        assert_eq!(
            recorder.try_record_started().unwrap(),
            TaskStartDecision::Recorded
        );
        assert_eq!(
            recorder.try_record_started().unwrap(),
            TaskStartDecision::AtLimit {
                limit: 1,
                window: TASK_LIMIT_WINDOW
            }
        );

        let reloaded = FileTaskStartRecorder::new(Some(1), &path);
        assert_eq!(
            reloaded.try_record_started().unwrap(),
            TaskStartDecision::AtLimit {
                limit: 1,
                window: TASK_LIMIT_WINDOW
            }
        );
    }

    #[test]
    fn zero_limit_disables_new_starts() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = FileTaskStartRecorder::new(Some(0), temp.path().join("task-starts.json"));

        assert_eq!(
            recorder.try_record_started().unwrap(),
            TaskStartDecision::AtLimit {
                limit: 0,
                window: TASK_LIMIT_WINDOW
            }
        );
    }
}
