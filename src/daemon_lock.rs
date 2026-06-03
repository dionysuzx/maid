use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
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
                        && process_is_current_executable(existing_pid)
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
fn process_is_current_executable(pid: u32) -> bool {
    let Ok(current_exe) = fs::read_link("/proc/self/exe") else {
        return false;
    };
    let Ok(process_exe) = fs::read_link(Path::new("/proc").join(pid.to_string()).join("exe"))
    else {
        return false;
    };

    process_exe == current_exe || process_cmdline_matches(pid, &current_exe)
}

#[cfg(unix)]
fn process_cmdline_matches(pid: u32, current_exe: &Path) -> bool {
    let Ok(cmdline) = fs::read(Path::new("/proc").join(pid.to_string()).join("cmdline")) else {
        return false;
    };
    let Some(first_arg) = cmdline.split(|byte| *byte == 0).next() else {
        return false;
    };
    if first_arg.is_empty() {
        return false;
    }

    Path::new(std::ffi::OsStr::from_bytes(first_arg)) == current_exe
}

#[cfg(not(unix))]
fn process_is_current_executable(_pid: u32) -> bool {
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

    #[cfg(unix)]
    #[test]
    fn replaces_live_pid_that_is_not_this_executable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("maid.pid");
        fs::write(&path, "1").unwrap();

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
