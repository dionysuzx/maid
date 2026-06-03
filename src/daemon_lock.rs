use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions, TryLockError},
    io::Write,
    path::{Path, PathBuf},
    process,
};

#[derive(Debug)]
pub struct DaemonLock {
    path: PathBuf,
    pid: u32,
    _lock_file: fs::File,
}

impl DaemonLock {
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let pid = process::id();
        let lock_file = open_lock_file(&path)?;
        lock_daemon_file(&lock_file, &path)?;

        if let Some(existing_pid) = read_pid(&path)
            && process_is_current_executable(existing_pid)
        {
            bail!("maid is already running with pid {existing_pid}");
        }

        write_pid_file(&path, pid)?;
        Ok(Self {
            path,
            pid,
            _lock_file: lock_file,
        })
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if read_pid(&self.path) == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn open_lock_file(pid_path: &Path) -> Result<fs::File> {
    let lock_path = lock_path_for(pid_path);
    OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open daemon lock file {}", lock_path.display()))
}

fn lock_path_for(pid_path: &Path) -> PathBuf {
    let mut path = OsString::from(pid_path.as_os_str());
    path.push(".lock");
    PathBuf::from(path)
}

fn lock_daemon_file(file: &fs::File, pid_path: &Path) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => {
            if let Some(existing_pid) = read_pid(pid_path)
                && process_is_current_executable(existing_pid)
            {
                bail!("maid is already running with pid {existing_pid}");
            }

            bail!("maid is already starting or running");
        }
        Err(TryLockError::Error(err)) => Err(err)
            .with_context(|| format!("failed to lock {}", lock_path_for(pid_path).display())),
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
}

fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::Builder::new()
        .prefix(".maid.pid.")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temp pid file in {}", parent.display()))?;

    writeln!(file, "{pid}").with_context(|| format!("failed to write {}", path.display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;

    match file.into_temp_path().persist(path) {
        Ok(()) => Ok(()),
        Err(err) => Err(err.error).with_context(|| format!("failed to create {}", path.display())),
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
    fn publishes_complete_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("maid.pid");

        write_pid_file(&path, 12345).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "12345");
    }

    #[test]
    fn replaces_existing_pid_file_when_publishing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("maid.pid");
        fs::write(&path, "existing").unwrap();

        write_pid_file(&path, 12345).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "12345");
    }

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
