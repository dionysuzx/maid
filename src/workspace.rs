#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::process::Command;
use tracing::warn;

/// Held locked by the run that owns the workspace and by the agent process it
/// starts, so startup can tell leftovers from live runs, including an agent
/// that outlived a killed Maid, and from directories Maid did not create.
const LOCK_FILE: &str = "workspace.lock";

/// Where one agent run works: `<root>/checkout` holds the code and
/// `<root>/tmp` is the run's temp directory, so venvs and caches the agent
/// creates stay out of the checkout and out of the shared `/tmp`.
///
/// A workspace made by `create_in` stays locked until it and its clones are
/// dropped, so `remove_leftovers_in` skips it.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    _lock: Option<Arc<File>>,
}

impl PartialEq for Workspace {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Eq for Workspace {}

impl Workspace {
    /// A workspace at an existing path, without a lock.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            _lock: None,
        }
    }

    /// Creates an empty, locked workspace with a short random name under
    /// `parent`, readable only by Maid's user. The short path keeps Unix
    /// socket paths under `tmp` within OS limits.
    pub fn create_in(parent: &Path) -> io::Result<Self> {
        fs::create_dir_all(parent)?;
        check_real_dir(parent)?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("");
        #[cfg(unix)]
        builder.permissions(fs::Permissions::from_mode(0o700));
        // Deletes the directory if any step below fails.
        let dir = builder.tempdir_in(parent)?;
        // Lock under a temporary name and rename, so a startup sweep never
        // sees an unlocked lock file in a workspace that is being created.
        let pending = dir.path().join("workspace.lock.new");
        let lock = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&pending)?;
        lock.lock()?;
        fs::rename(&pending, dir.path().join(LOCK_FILE))?;
        let unlocked = Self::new(dir.path());
        fs::create_dir(unlocked.checkout())?;
        fs::create_dir(unlocked.tmp())?;
        Ok(Self {
            root: dir.keep(),
            _lock: Some(Arc::new(lock)),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn checkout(&self) -> PathBuf {
        self.root.join("checkout")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// A command for `program` that runs in the checkout and keeps its temp
    /// files in this workspace. The process inherits the workspace lock, so
    /// the workspace stays live while it runs, even if Maid is killed.
    pub fn command(&self, program: &str) -> Command {
        let tmp = self.tmp();
        let mut command = Command::new(program);
        command.current_dir(self.checkout());
        for var in ["TMPDIR", "TMP", "TEMP"] {
            command.env(var, &tmp);
        }
        #[cfg(unix)]
        if let Some(lock) = &self._lock {
            let fd = lock.as_raw_fd();
            // SAFETY: the closure only calls `fcntl`, which is
            // async-signal-safe, on a descriptor this workspace keeps open.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command
    }

    pub async fn remove(&self) -> io::Result<()> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || remove_workspace(&root)).await?
    }
}

/// Removes workspaces under `parent` that a killed run left behind: those
/// whose lock file is not held. Live workspaces, including another Maid's,
/// and anything without a lock file are left alone. Failures for single
/// workspaces are logged.
pub async fn remove_leftovers_in(parent: PathBuf) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        match check_real_dir(&parent) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        }
        let entries = fs::read_dir(&parent)?;
        for entry in entries {
            let root = entry?.path();
            if let Err(err) = remove_if_leftover(&root) {
                warn!(path = %root.display(), error = ?err, "failed to remove leftover workspace");
            }
        }
        Ok(())
    })
    .await?
}

/// A leftover is a directory with a regular lock file nobody holds.
fn remove_if_leftover(root: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Ok(());
    }
    let lock_path = root.join(LOCK_FILE);
    match fs::symlink_metadata(&lock_path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if !metadata.is_file() => return Ok(()),
        metadata => metadata?,
    };
    // Opened for writing because NFS refuses an exclusive lock otherwise.
    let lock = OpenOptions::new().read(true).write(true).open(lock_path)?;
    match lock.try_lock() {
        Ok(()) => remove_workspace(root),
        Err(TryLockError::WouldBlock) => Ok(()),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Workspaces live directly in a real directory. A symlink is refused, so
/// the startup sweep is never led into another directory's contents.
fn check_real_dir(parent: &Path) -> io::Result<()> {
    if fs::symlink_metadata(parent)?.is_dir() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{} is a symlink or not a directory",
        parent.display()
    )))
}

/// Removes everything in the workspace before its lock file, so a failed
/// removal leaves a workspace the next startup still recognizes. Read-only
/// directories, such as a Go module cache, are made writable first. A
/// missing workspace counts as removed.
fn remove_workspace(root: &Path) -> io::Result<()> {
    make_dirs_writable(root);
    let entries = match fs::read_dir(root) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == LOCK_FILE {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_dir_all(root)
}

/// Best effort: anything left read-only surfaces as the `remove_dir_all` error.
#[cfg(not(unix))]
fn make_dirs_writable(_path: &Path) {}

/// Best effort: anything left read-only surfaces as the `remove_dir_all` error.
#[cfg(unix)]
fn make_dirs_writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    let _ = fs::set_permissions(path, permissions);
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            make_dirs_writable(&entry.path());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Lays out a workspace a killed run left, by hand, so no open lock file
    /// can leak into processes other tests start in parallel.
    pub(crate) fn leftover_in(parent: &Path) -> PathBuf {
        let root = parent.join("leftover");
        fs::create_dir_all(root.join("checkout")).unwrap();
        fs::create_dir(root.join("tmp")).unwrap();
        fs::write(root.join(LOCK_FILE), "").unwrap();
        fs::set_permissions(root.join("tmp"), fs::Permissions::from_mode(0o555)).unwrap();
        root
    }

    #[tokio::test]
    async fn command_runs_in_the_checkout_with_the_workspace_temp_dir() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = Workspace::create_in(parent.path()).unwrap();

        let output = workspace
            .command("sh")
            .arg("-c")
            .arg(r#"printf '%s %s %s %s' "$PWD" "$TMPDIR" "$TMP" "$TEMP""#)
            .output()
            .await
            .unwrap();

        let checkout = workspace.checkout().canonicalize().unwrap();
        let tmp = workspace.tmp();
        let tmp = tmp.display();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{} {tmp} {tmp} {tmp}", checkout.display())
        );
    }

    #[tokio::test]
    async fn workspaces_get_distinct_private_roots_under_the_parent() {
        let parent = tempfile::tempdir().unwrap();

        let first = Workspace::create_in(parent.path()).unwrap();
        let second = Workspace::create_in(parent.path()).unwrap();

        assert_ne!(first, second);
        assert_eq!(first.root().parent(), Some(parent.path()));
        let mode = fs::metadata(first.root()).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "workspace root is {mode:o}");
    }

    #[tokio::test]
    async fn remove_deletes_read_only_directories() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = Workspace::create_in(parent.path()).unwrap();
        let root = workspace.root().to_path_buf();
        let module_cache = workspace.tmp().join("pkg");
        fs::create_dir(&module_cache).unwrap();
        fs::write(module_cache.join("go.mod"), "module x").unwrap();
        fs::set_permissions(&module_cache, fs::Permissions::from_mode(0o555)).unwrap();

        workspace.remove().await.unwrap();

        assert!(!root.exists());
    }

    #[tokio::test]
    async fn removes_unlocked_workspaces_and_keeps_live_ones_and_other_data() {
        let parent = tempfile::tempdir().unwrap();
        let live = Workspace::create_in(parent.path()).unwrap();
        let leftover = leftover_in(parent.path());
        let other = parent.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("config.toml"), "").unwrap();
        fs::write(parent.path().join("file"), "").unwrap();
        let not_a_lock = parent.path().join("not-a-lock");
        fs::create_dir_all(not_a_lock.join(LOCK_FILE)).unwrap();
        fs::create_dir(leftover.join("target")).unwrap();

        remove_leftovers_in(parent.path().to_path_buf())
            .await
            .unwrap();

        assert!(!leftover.exists());
        assert!(live.checkout().is_dir());
        assert!(other.join("config.toml").is_file());
        assert!(parent.path().join("file").is_file());
        assert!(not_a_lock.join(LOCK_FILE).is_dir());
    }

    #[tokio::test]
    async fn an_agent_that_outlives_its_workspace_handle_keeps_it_live() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = Workspace::create_in(parent.path()).unwrap();
        let root = workspace.root().to_path_buf();
        let mut agent = workspace.command("sleep").arg("30").spawn().unwrap();
        drop(workspace);

        remove_leftovers_in(parent.path().to_path_buf())
            .await
            .unwrap();
        assert!(root.join("checkout").is_dir());
        agent.kill().await.unwrap();
    }

    #[tokio::test]
    async fn refuses_a_symlinked_parent() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let leftover = leftover_in(&target);
        let link = temp.path().join("runs");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(remove_leftovers_in(link.clone()).await.is_err());
        assert!(leftover.join("checkout").is_dir());
        assert!(Workspace::create_in(&link).is_err());
    }

    #[tokio::test]
    async fn removing_leftovers_from_a_missing_directory_succeeds() {
        let parent = tempfile::tempdir().unwrap();

        remove_leftovers_in(parent.path().join("missing"))
            .await
            .unwrap();
    }
}
