use anyhow::{Context, Result, bail};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process,
};

#[derive(Debug)]
pub struct DaemonLock {
    path: PathBuf,
    pid: u32,
}

impl DaemonLock {
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let pid = process::id();
        loop {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{pid}")
                        .with_context(|| format!("failed to write {}", path.display()))?;
                    file.sync_all()
                        .with_context(|| format!("failed to sync {}", path.display()))?;
                    return Ok(Self { path, pid });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing_pid = fs::read_to_string(&path)
                        .ok()
                        .and_then(|pid| pid.trim().parse::<u32>().ok());

                    if let Some(existing_pid) = existing_pid
                        && process_exists(existing_pid)
                    {
                        bail!("maid is already running with pid {existing_pid}");
                    }

                    fs::remove_file(&path).with_context(|| {
                        format!("failed to remove stale daemon pid file {}", path.display())
                    })?;
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let current_pid = fs::read_to_string(&self.path)
            .ok()
            .and_then(|pid| pid.trim().parse::<u32>().ok());

        if current_pid == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prevents_two_instances_with_the_same_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("maid.pid");

        let lock = DaemonLock::acquire(&path).unwrap();
        let err = DaemonLock::acquire(&path).unwrap_err();

        assert!(err.to_string().contains("maid is already running"));
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn replaces_stale_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("maid.pid");
        fs::write(&path, "999999999").unwrap();

        let _lock = DaemonLock::acquire(&path).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap().trim(),
            process::id().to_string()
        );
    }

    #[test]
    fn does_not_remove_another_process_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("maid.pid");

        let lock = DaemonLock::acquire(&path).unwrap();
        fs::write(&path, "999999999").unwrap();
        drop(lock);

        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "999999999");
    }
}
