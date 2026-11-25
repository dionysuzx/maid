use crate::{
    domain::{WorkTarget, validate_repo_name_part},
    maid::{PreparedWorktree, Worktrees},
    workspace::{self, Workspace},
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose};
use std::{
    env,
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct GitWorktrees {
    root: PathBuf,
    auth_header: String,
}

impl GitWorktrees {
    pub fn new(root: impl Into<PathBuf>, github_token: impl Into<String>) -> Self {
        let credential = format!("x-access-token:{}", github_token.into());
        let encoded = general_purpose::STANDARD.encode(credential);
        Self {
            root: absolute_root(root.into()),
            auth_header: format!("Authorization: Basic {encoded}"),
        }
    }

    pub fn repo_dir(&self, target: &WorkTarget) -> Result<PathBuf> {
        validate_repo_name_part(target.owner(), "repository owner")?;
        validate_repo_name_part(target.repo(), "repository name")?;
        Ok(self
            .root
            .join("repos")
            .join(target.owner())
            .join(format!("{}.git", target.repo())))
    }

    /// Holds every run's workspace and nothing else, so its owner can delete
    /// it whole. Kept short so socket paths under a workspace's `tmp` fit.
    fn workspaces_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// Makes this process the only owner of the git dir for as long as the
    /// returned guard lives, then deletes the workspaces a previous owner
    /// left behind; `cleanup` later prunes their git worktree records. Call
    /// at startup, before any run starts. Fails if another Maid owns it.
    pub async fn take_ownership(&self) -> Result<GitDirOwner> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        let lock_path = self.root.join("maid.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!("another Maid owns {}", self.root.display())
            }
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("failed to lock {}", lock_path.display()));
            }
        }

        let runs = self.workspaces_dir();
        if let Err(err) = workspace::remove_dir(runs.clone()).await {
            warn!(path = %runs.display(), error = ?err, "failed to remove leftover workspaces");
        }
        Ok(GitDirOwner { _lock: lock })
    }

    async fn run_git(&self, cwd: Option<&Path>, args: &[&str]) -> Result<()> {
        let mut command = Command::new("git");
        command
            .args(args)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraheader")
            .env("GIT_CONFIG_VALUE_0", &self.auth_header)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        let output = command.output().await.context("failed to run git")?;
        if output.status.success() {
            return Ok(());
        }

        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }

    async fn acquire_repo_lock(&self, repo: &Path) -> Result<RepoLock> {
        let lock_path = repo.with_extension("git.lock");
        if let Some(parent) = lock_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)
                .with_context(|| format!("failed to open {}", lock_path.display()))?;
            file.lock()
                .with_context(|| format!("failed to lock {}", lock_path.display()))?;
            Ok(RepoLock { _file: file })
        })
        .await
        .context("failed to join repo lock task")?
    }
}

#[async_trait]
impl Worktrees for GitWorktrees {
    async fn prepare(&self, target: &WorkTarget) -> Result<PreparedWorktree> {
        let repo = self.repo_dir(target)?;
        let _lock = self.acquire_repo_lock(&repo).await?;
        tokio::fs::create_dir_all(
            repo.parent()
                .ok_or_else(|| anyhow!("repository path has no parent"))?,
        )
        .await?;

        if !repo.join("HEAD").exists() {
            let repo_string = repo.to_string_lossy().to_string();
            self.run_git(None, &["clone", "--bare", target.clone_url(), &repo_string])
                .await
                .with_context(|| format!("failed to clone bare repo {}", target.repo_key()))?;
        }

        self.run_git(
            Some(&repo),
            &["remote", "set-url", "origin", target.clone_url()],
        )
        .await?;
        let fetch_ref = target.fetch_ref();
        self.run_git(Some(&repo), &["fetch", "--prune", "origin", &fetch_ref])
            .await
            .with_context(|| format!("failed to fetch {}", target.html_url()))?;

        let workspace =
            Workspace::create_in(&self.workspaces_dir()).context("failed to create workspace")?;
        let checkout = workspace.checkout().to_string_lossy().to_string();
        let added = self
            .run_git(
                Some(&repo),
                &["worktree", "add", "--detach", &checkout, "FETCH_HEAD"],
            )
            .await;
        if let Err(err) = added {
            let _ = workspace.remove().await;
            return Err(err);
        }

        Ok(PreparedWorktree::git_worktree(repo, workspace))
    }

    async fn cleanup(&self, worktree: PreparedWorktree) -> Result<()> {
        let repo = worktree
            .repo()
            .ok_or_else(|| anyhow!("worktree has no git repository path"))?;
        let workspace = worktree.workspace();
        workspace
            .remove()
            .await
            .with_context(|| format!("failed to remove {}", workspace.root().display()))?;
        let _lock = self.acquire_repo_lock(repo).await?;
        self.run_git(Some(repo), &["worktree", "prune"]).await
    }
}

struct RepoLock {
    _file: File,
}

/// Keeps a process the only owner of a git dir until dropped.
#[derive(Debug)]
pub struct GitDirOwner {
    _lock: File,
}

fn absolute_root(root: PathBuf) -> PathBuf {
    if root.is_absolute() {
        return root;
    }

    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CommitSha, Issue, PullRequest};
    use std::process::Command as StdCommand;

    fn pr(owner: &str, repo: &str) -> PullRequest {
        PullRequest {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: 46,
            author: "dionysuzx".to_string(),
            api_url: "https://api.github.com/repos/o/r/pulls/46".to_string(),
            html_url: "https://github.com/o/r/pull/46".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
            head: CommitSha::parse(&"a".repeat(40)).unwrap(),
        }
    }

    fn issue(owner: &str, repo: &str) -> Issue {
        Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: 322,
            author: "dionysuzx".to_string(),
            api_url: "https://api.github.com/repos/o/r/issues/322".to_string(),
            html_url: "https://github.com/o/r/issues/322".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
            default_branch: "main".to_string(),
        }
    }

    fn pr_target(owner: &str, repo: &str) -> WorkTarget {
        WorkTarget::PullRequest(pr(owner, repo))
    }

    fn issue_target(owner: &str, repo: &str) -> WorkTarget {
        WorkTarget::Issue(issue(owner, repo))
    }

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn source_repo(dir: &Path) -> PathBuf {
        let source = dir.join("source");
        std::fs::create_dir(&source).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["config", "user.name", "test"]);
        run_git(&source, &["config", "user.email", "test@example.com"]);
        std::fs::write(source.join("file.txt"), "hello\n").unwrap();
        run_git(&source, &["add", "file.txt"]);
        run_git(&source, &["commit", "-m", "initial"]);
        run_git(&source, &["branch", "-M", "main"]);
        source
    }

    fn read_checkout_file(worktree: &PreparedWorktree) -> String {
        std::fs::read_to_string(worktree.workspace().checkout().join("file.txt")).unwrap()
    }

    #[test]
    fn maps_repositories_into_git_dir() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");

        assert_eq!(
            worktrees
                .repo_dir(&pr_target("dionysuzx", "forkcast"))
                .unwrap(),
            PathBuf::from("/tmp/maid-git/repos/dionysuzx/forkcast.git")
        );
    }

    #[test]
    fn maps_relative_git_dir_from_the_process_directory() {
        let current_dir = std::env::current_dir().unwrap();
        let worktrees = GitWorktrees::new("target/maid-git", "token");
        let pr = pr_target("dionysuzx", "forkcast");

        assert_eq!(
            worktrees.repo_dir(&pr).unwrap(),
            current_dir.join("target/maid-git/repos/dionysuzx/forkcast.git")
        );
    }

    #[tokio::test]
    async fn prepares_and_cleans_up_distinct_workspaces_at_the_listed_head() {
        let temp = tempfile::tempdir().unwrap();
        let source = source_repo(temp.path());
        let listed_head = CommitSha::parse(&run_git(&source, &["rev-parse", "HEAD"])).unwrap();
        std::fs::write(source.join("file.txt"), "pushed after listing\n").unwrap();
        run_git(&source, &["commit", "-am", "pushed after listing"]);
        run_git(&source, &["update-ref", "refs/pull/46/head", "HEAD"]);

        let worktrees = GitWorktrees::new(temp.path().join("git"), "token");
        let pr = WorkTarget::PullRequest(PullRequest {
            clone_url: source.to_string_lossy().to_string(),
            head: listed_head,
            ..pr("o", "r")
        });

        let first = worktrees.prepare(&pr).await.unwrap();
        let second = worktrees.prepare(&pr).await.unwrap();

        assert_ne!(first.workspace(), second.workspace());
        assert!(first.workspace().tmp().is_dir());
        assert_eq!(read_checkout_file(&first), "hello\n");
        assert_eq!(read_checkout_file(&second), "hello\n");

        let first_root = first.workspace().root().to_path_buf();
        let second_root = second.workspace().root().to_path_buf();
        worktrees.cleanup(first).await.unwrap();
        worktrees.cleanup(second).await.unwrap();

        assert!(!first_root.exists());
        assert!(!second_root.exists());
        let repo = worktrees.repo_dir(&pr).unwrap();
        assert_eq!(run_git(&repo, &["worktree", "list"]).lines().count(), 1);
    }

    #[tokio::test]
    async fn prepares_issue_workspace_from_default_branch() {
        let temp = tempfile::tempdir().unwrap();
        let source = source_repo(temp.path());

        let worktrees = GitWorktrees::new(temp.path().join("git"), "token");
        let issue = WorkTarget::Issue(Issue {
            clone_url: source.to_string_lossy().to_string(),
            ..issue("o", "r")
        });

        let prepared = worktrees.prepare(&issue).await.unwrap();

        assert_eq!(read_checkout_file(&prepared), "hello\n");
        worktrees.cleanup(prepared).await.unwrap();
    }

    #[tokio::test]
    async fn taking_ownership_removes_workspaces_a_killed_run_left_and_keeps_repositories() {
        let temp = tempfile::tempdir().unwrap();
        let source = source_repo(temp.path());
        let worktrees = GitWorktrees::new(temp.path().join("git"), "token");
        let issue = WorkTarget::Issue(Issue {
            clone_url: source.to_string_lossy().to_string(),
            ..issue("o", "r")
        });
        let leftover = worktrees.prepare(&issue).await.unwrap();

        let _owner = worktrees.take_ownership().await.unwrap();

        assert!(!leftover.workspace().root().exists());
        let next = worktrees.prepare(&issue).await.unwrap();
        assert_eq!(read_checkout_file(&next), "hello\n");
        worktrees.cleanup(next).await.unwrap();
        let repo = worktrees.repo_dir(&issue).unwrap();
        assert_eq!(run_git(&repo, &["worktree", "list"]).lines().count(), 1);
    }

    #[tokio::test]
    async fn only_one_process_owns_a_git_dir_at_a_time() {
        let temp = tempfile::tempdir().unwrap();
        let worktrees = GitWorktrees::new(temp.path().join("git"), "token");

        let owner = worktrees.take_ownership().await.unwrap();
        let err = worktrees.take_ownership().await.unwrap_err();
        assert!(err.to_string().starts_with("another Maid owns"), "{err}");

        drop(owner);
        worktrees.take_ownership().await.unwrap();
    }

    #[test]
    fn rejects_repository_parts_that_could_escape_git_dir() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");

        assert!(
            worktrees
                .repo_dir(&pr_target("../dionysuzx", "forkcast"))
                .is_err()
        );
        assert!(
            worktrees
                .repo_dir(&issue_target("dionysuzx", "forkcast/slash"))
                .is_err()
        );
    }
}
