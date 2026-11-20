use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use std::{
    fmt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::process::Command;
use tracing::warn;

static NEXT_SCRATCH_ID: AtomicU64 = AtomicU64::new(0);
const SCRATCH_MARKER: &str = ".maid-scratch";

#[async_trait]
pub trait Agent: Send + Sync {
    fn label(&self) -> &AgentLabel;
    async fn run(&self, worktree: &Path, prompt: &str) -> Result<AgentRun>;
}

/// One run's scratch directory under the Maid scratch root (`~/.maid/tmp/<id>`).
///
/// Drop removes it, including read-only directories tools like the Go module
/// cache leave behind. A removal failure is logged; the startup wipe is the
/// backstop, so cleanup never fails the review.
pub(crate) struct AgentScratch {
    path: PathBuf,
}

impl AgentScratch {
    pub(crate) async fn create(root: &Path, command: &mut Command) -> Result<Self> {
        tokio::fs::create_dir_all(root)
            .await
            .with_context(|| format!("failed to create {}", root.display()))?;
        let path = loop {
            let path = root.join(scratch_name(
                NEXT_SCRATCH_ID.fetch_add(1, Ordering::Relaxed),
            ));
            match tokio::fs::create_dir(&path).await {
                Ok(()) => break path,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        };
        if let Err(err) = tokio::fs::write(path.join(SCRATCH_MARKER), []).await {
            let _ = tokio::fs::remove_dir(&path).await;
            return Err(err).with_context(|| format!("failed to mark {}", path.display()));
        }
        command
            .env("TMPDIR", &path)
            .env("TMP", &path)
            .env("TEMP", &path);
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for AgentScratch {
    fn drop(&mut self) {
        if let Err(err) = remove_scratch_sync(&self.path) {
            warn!(
                error = %err,
                path = %self.path.display(),
                "failed to remove agent scratch"
            );
        }
    }
}

/// Deletes scratch directories Maid created under `root`. Called at daemon startup, before tasks run.
///
/// A directory is ours only when Maid wrote its marker. Other names, files, and a scratch root that
/// is a symlink stay in place.
pub async fn wipe_agent_scratch(root: &Path) -> Result<()> {
    let root = root.to_path_buf();
    let displayed = root.display().to_string();
    tokio::task::spawn_blocking(move || wipe_scratch_sync(&root))
        .await
        .context("failed to wipe agent scratch")?
        .with_context(|| format!("failed to wipe {displayed}"))
}

fn scratch_name(id: u64) -> String {
    format!("maid-{id:x}")
}

fn wipe_scratch_sync(root: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let path = entry?.path();
        if is_our_scratch(&path) {
            remove_scratch_sync(&path)?;
        }
    }
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || std::fs::read_dir(root)?.next().is_some()
    {
        return Ok(());
    }
    remove_scratch_sync(root)
}

fn is_our_scratch(path: &Path) -> bool {
    if !is_scratch_id(path.file_name()) {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    std::fs::symlink_metadata(path.join(SCRATCH_MARKER)).is_ok_and(|metadata| metadata.is_file())
}

fn is_scratch_id(name: Option<&std::ffi::OsStr>) -> bool {
    let Some(name) = name.and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(id) = name.strip_prefix("maid-") else {
        return false;
    };
    !id.is_empty() && id.chars().all(|char| char.is_ascii_hexdigit())
}

fn remove_scratch_sync(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() {
        return std::fs::remove_file(path);
    }
    make_dirs_writable(path);
    match std::fs::remove_dir_all(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn make_dirs_writable(path: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    let _ = std::fs::set_permissions(path, permissions);
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        make_dirs_writable(&entry.path());
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Codex,
    Claude,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn resume_command(self, session_id: &str) -> String {
        match self {
            Self::Codex => format!("codex resume --include-non-interactive --all {session_id}"),
            Self::Claude => format!("claude --resume {session_id}"),
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLabel {
    pub kind: AgentKind,
    pub model: String,
    pub effort: String,
}

impl fmt::Display for AgentLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} · {} · {}",
            self.kind, self.model, self.effort
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRun {
    pub response: String,
    pub session_id: Option<String>,
}

/// The agents that answer every review and mention in parallel.
#[derive(Clone)]
pub struct AgentPanel {
    agents: Vec<Arc<dyn Agent>>,
}

impl AgentPanel {
    /// Kinds must be unique because each kind gets its own worktree for a task.
    pub fn new(agents: Vec<Arc<dyn Agent>>) -> Result<Self> {
        if agents.is_empty() {
            bail!("at least one agent is required");
        }
        for (index, agent) in agents.iter().enumerate() {
            let kind = agent.label().kind;
            if agents[..index]
                .iter()
                .any(|earlier| earlier.label().kind == kind)
            {
                bail!("agents must have unique kinds; {kind} is listed twice");
            }
        }

        Ok(Self { agents })
    }

    pub fn all(&self) -> &[Arc<dyn Agent>] {
        &self.agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LabelOnly(AgentLabel);

    #[async_trait]
    impl Agent for LabelOnly {
        fn label(&self) -> &AgentLabel {
            &self.0
        }

        async fn run(&self, _worktree: &Path, _prompt: &str) -> Result<AgentRun> {
            unreachable!("panel tests do not run agents")
        }
    }

    fn agent(kind: AgentKind) -> Arc<dyn Agent> {
        Arc::new(LabelOnly(AgentLabel {
            kind,
            model: "model".to_string(),
            effort: "high".to_string(),
        }))
    }

    #[test]
    fn panel_requires_one_agent_of_each_listed_kind() {
        assert!(AgentPanel::new(Vec::new()).is_err());
        assert!(AgentPanel::new(vec![agent(AgentKind::Claude), agent(AgentKind::Codex)]).is_ok());
        assert_eq!(
            AgentPanel::new(vec![agent(AgentKind::Codex), agent(AgentKind::Codex)])
                .err()
                .unwrap()
                .to_string(),
            "agents must have unique kinds; codex is listed twice"
        );
    }

    #[tokio::test]
    async fn scratch_is_a_short_directory_and_cleanup_removes_read_only_trees() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        let scratch_path = {
            let scratch = AgentScratch::create(root.path(), &mut command)
                .await
                .unwrap();
            let scratch_path = scratch.path().to_path_buf();
            assert_eq!(scratch_path.parent(), Some(root.path()));
            let id = scratch_path.file_name().unwrap().to_str().unwrap();
            let hex = id.strip_prefix("maid-").expect(id);
            assert!(
                !hex.is_empty()
                    && hex.len() <= 16
                    && hex.chars().all(|char| char.is_ascii_hexdigit()),
                "{id}"
            );

            let output = command
                .arg("-c")
                .arg("printf '%s\\n%s\\n%s' \"$TMPDIR\" \"$TMP\" \"$TEMP\"")
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let scratch_text = scratch_path.to_str().unwrap();
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{scratch_text}\n{scratch_text}\n{scratch_text}")
            );
            lock_directory(&scratch_path.join("pkg"));
            scratch_path
        };
        assert!(!scratch_path.exists());

        let leftover = {
            let scratch = AgentScratch::create(root.path(), &mut Command::new("true"))
                .await
                .unwrap();
            let leftover = scratch.path().to_path_buf();
            lock_directory(&leftover.join("pkg"));
            std::mem::forget(scratch);
            leftover
        };
        assert!(leftover.exists());
        wipe_agent_scratch(root.path()).await.unwrap();
        assert!(!leftover.exists());
        assert!(!root.path().exists());
    }

    #[tokio::test]
    async fn wipe_removes_only_scratch_maid_created() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("tmp");
        let repo = scratch.join("git").join("repo.git");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let named_like_scratch = scratch.join("maid-cafe");
        std::fs::create_dir_all(&named_like_scratch).unwrap();
        let outside = root.path().join("store");
        std::fs::create_dir_all(outside.join("repo.git")).unwrap();
        std::fs::write(outside.join("repo.git").join("HEAD"), "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, named_like_scratch.join("git")).unwrap();
        std::fs::write(scratch.join("maid-a"), "file\n").unwrap();
        std::fs::write(scratch.join("notes"), "keep\n").unwrap();
        let ours = {
            let scratch_dir = AgentScratch::create(&scratch, &mut Command::new("true"))
                .await
                .unwrap();
            let ours = scratch_dir.path().to_path_buf();
            lock_directory(&ours.join("pkg"));
            std::mem::forget(scratch_dir);
            ours
        };
        let volume = root.path().join("volume");
        std::fs::create_dir(&volume).unwrap();
        let scratch_link = root.path().join("scratch-link");
        std::os::unix::fs::symlink(&volume, &scratch_link).unwrap();

        wipe_agent_scratch(&scratch).await.unwrap();
        wipe_agent_scratch(&scratch_link).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("repo.git").join("HEAD")).unwrap(),
            "outside\n"
        );
        assert!(
            named_like_scratch
                .join("git")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(scratch.join("maid-a")).unwrap(),
            "file\n"
        );
        assert_eq!(
            std::fs::read_to_string(scratch.join("notes")).unwrap(),
            "keep\n"
        );
        assert!(!ours.exists());
        assert!(
            scratch_link
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(volume.exists());
    }

    fn lock_directory(path: &std::path::Path) {
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("file"), "module").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}
