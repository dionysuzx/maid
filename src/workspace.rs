#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use tokio::process::Command;

/// Where one agent run works: `<root>/checkout` holds the code and
/// `<root>/tmp` is the run's temp directory, so venvs and caches the agent
/// creates stay out of the checkout and out of the shared `/tmp`.
///
/// Workspaces are created under one parent directory that holds nothing else,
/// so removing that parent at startup clears runs a crash left behind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Creates an empty workspace with a short random name under `parent`,
    /// readable only by Thoth's user. The short path keeps Unix socket paths
    /// under `tmp` within OS limits.
    pub fn create_in(parent: &Path) -> io::Result<Self> {
        fs::create_dir_all(parent)?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("");
        #[cfg(unix)]
        builder.permissions(fs::Permissions::from_mode(0o700));
        let workspace = Self::new(builder.tempdir_in(parent)?.keep());
        fs::create_dir(workspace.checkout())?;
        fs::create_dir(workspace.tmp())?;
        Ok(workspace)
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
    /// files in this workspace.
    pub fn command(&self, program: &str) -> Command {
        let tmp = self.tmp();
        let mut command = Command::new(program);
        command.current_dir(self.checkout());
        for var in ["TMPDIR", "TMP", "TEMP"] {
            command.env(var, &tmp);
        }
        command
    }

    pub async fn remove(&self) -> io::Result<()> {
        remove_dir(self.root.clone()).await
    }
}

/// Removes `path` and everything under it, including read-only directories
/// such as a Go module cache. A missing path counts as removed.
pub async fn remove_dir(path: PathBuf) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        make_dirs_writable(&path);
        match fs::remove_dir_all(&path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    })
    .await?
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
mod tests {
    use super::*;

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
    async fn removing_a_missing_directory_succeeds() {
        let parent = tempfile::tempdir().unwrap();

        remove_dir(parent.path().join("missing")).await.unwrap();
    }
}
