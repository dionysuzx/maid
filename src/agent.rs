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
        let path = root.join(format!(
            "{:x}",
            NEXT_SCRATCH_ID.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir_all(&path)
            .await
            .with_context(|| format!("failed to create {}", path.display()))?;
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

/// Deletes leftover scratch under `root`. A `git_dir` inside `root` is left in place.
/// Called at daemon startup, before tasks run.
pub async fn wipe_agent_scratch(root: &Path, git_dir: &Path) -> Result<()> {
    let root = root.to_path_buf();
    let git_dir = git_dir.to_path_buf();
    let displayed = root.display().to_string();
    tokio::task::spawn_blocking(move || wipe_scratch_sync(&root, &git_dir))
        .await
        .context("failed to wipe agent scratch")?
        .with_context(|| format!("failed to wipe {displayed}"))
}

fn wipe_scratch_sync(root: &Path, git_dir: &Path) -> std::io::Result<()> {
    if overlaps(root, git_dir) {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        for entry in entries {
            let path = entry?.path();
            if overlaps(&path, git_dir) || !is_scratch_id(path.file_name()) {
                continue;
            }
            remove_scratch_sync(&path)?;
        }
        return Ok(());
    }
    remove_scratch_sync(root)
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn is_scratch_id(name: Option<&std::ffi::OsStr>) -> bool {
    let Some(name) = name.and_then(|name| name.to_str()) else {
        return false;
    };
    !name.is_empty() && name.chars().all(|char| char.is_ascii_hexdigit())
}

fn remove_scratch_sync(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
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
            assert!(
                !id.is_empty() && id.len() <= 16 && id.chars().all(|char| char.is_ascii_hexdigit()),
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
        wipe_agent_scratch(root.path(), std::path::Path::new("/outside-git"))
            .await
            .unwrap();
        assert!(!leftover.exists());
        assert!(!root.path().exists());
    }

    #[tokio::test]
    async fn wipe_keeps_a_git_dir_that_lives_under_the_scratch_root() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("tmp");
        let git_dir = scratch.join("git");
        let repo = git_dir.join("repo.git");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let leftover = scratch.join("a1");
        std::fs::create_dir_all(&leftover).unwrap();
        lock_directory(&leftover.join("pkg"));
        std::fs::write(scratch.join("notes"), "keep\n").unwrap();

        wipe_agent_scratch(&scratch, &git_dir).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        assert!(!leftover.exists());
        assert_eq!(
            std::fs::read_to_string(scratch.join("notes")).unwrap(),
            "keep\n"
        );
    }

    fn lock_directory(path: &std::path::Path) {
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("file"), "module").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}
