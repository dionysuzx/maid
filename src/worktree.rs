use crate::{
    domain::{CodexTask, PullRequest, validate_repo_name_part},
    maid::{PreparedWorktree, Worktrees},
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

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
            root: root.into(),
            auth_header: format!("Authorization: Basic {encoded}"),
        }
    }

    pub fn repo_dir(&self, pr: &PullRequest) -> Result<PathBuf> {
        validate_repo_name_part(&pr.owner, "repository owner")?;
        validate_repo_name_part(&pr.repo, "repository name")?;
        Ok(self
            .root
            .join("repos")
            .join(&pr.owner)
            .join(format!("{}.git", pr.repo)))
    }

    pub fn worktree_dir(&self, pr: &PullRequest, task: &CodexTask) -> Result<PathBuf> {
        validate_repo_name_part(&pr.owner, "repository owner")?;
        validate_repo_name_part(&pr.repo, "repository name")?;
        Ok(self
            .root
            .join("worktrees")
            .join(&pr.owner)
            .join(&pr.repo)
            .join(pr.number.to_string())
            .join(worktree_key(task)))
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
    async fn prepare(&self, pr: &PullRequest, task: &CodexTask) -> Result<PreparedWorktree> {
        let repo = self.repo_dir(pr)?;
        let worktree = self.worktree_dir(pr, task)?;
        let _lock = self.acquire_repo_lock(&repo).await?;
        tokio::fs::create_dir_all(
            repo.parent()
                .ok_or_else(|| anyhow!("repository path has no parent"))?,
        )
        .await?;
        tokio::fs::create_dir_all(
            worktree
                .parent()
                .ok_or_else(|| anyhow!("worktree path has no parent"))?,
        )
        .await?;

        if !repo.join("HEAD").exists() {
            let repo_string = repo.to_string_lossy().to_string();
            self.run_git(None, &["clone", "--bare", &pr.clone_url, &repo_string])
                .await
                .with_context(|| format!("failed to clone bare repo {}", pr.repo_key()))?;
        }

        self.run_git(Some(&repo), &["remote", "set-url", "origin", &pr.clone_url])
            .await?;
        let pr_head = format!("pull/{}/head", pr.number);
        self.run_git(Some(&repo), &["fetch", "--prune", "origin", &pr_head])
            .await
            .with_context(|| format!("failed to fetch PR {}", pr.html_url))?;

        let worktree_string = worktree.to_string_lossy().to_string();
        if worktree.exists() {
            let _ = self
                .run_git(
                    Some(&repo),
                    &["worktree", "remove", "--force", &worktree_string],
                )
                .await;
            if worktree.exists() {
                tokio::fs::remove_dir_all(&worktree)
                    .await
                    .with_context(|| format!("failed to remove {}", worktree.display()))?;
            }
        }

        self.run_git(Some(&repo), &["worktree", "prune"]).await?;
        self.run_git(
            Some(&repo),
            &[
                "worktree",
                "add",
                "--detach",
                &worktree_string,
                "FETCH_HEAD",
            ],
        )
        .await?;

        Ok(PreparedWorktree::git_worktree(repo, worktree))
    }

    async fn cleanup(&self, worktree: PreparedWorktree) -> Result<()> {
        let repo = worktree
            .repo()
            .ok_or_else(|| anyhow!("worktree has no git repository path"))?
            .to_path_buf();
        let _lock = self.acquire_repo_lock(&repo).await?;
        let worktree_string = worktree.path().to_string_lossy().to_string();
        let remove_result = self
            .run_git(
                Some(&repo),
                &["worktree", "remove", "--force", &worktree_string],
            )
            .await;
        if worktree.path().exists() {
            tokio::fs::remove_dir_all(worktree.path())
                .await
                .with_context(|| format!("failed to remove {}", worktree.path().display()))?;
        }
        self.run_git(Some(&repo), &["worktree", "prune"]).await?;
        if worktree.path().exists() {
            return remove_result;
        }
        Ok(())
    }
}

struct RepoLock {
    _file: File,
}

fn worktree_key(task: &CodexTask) -> String {
    format!(
        "{}-{:016x}",
        task.task_kind(),
        stable_hash(task.trigger_url())
    )
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    fn task(trigger: &str) -> CodexTask {
        CodexTask {
            pr_url: "https://github.com/o/r/pull/46".to_string(),
            origin: crate::domain::CodexTaskOrigin::Mention {
                mention_url: trigger.to_string(),
                raw_body: "@maid-bot review".to_string(),
                cleaned_text: "review".to_string(),
            },
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
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
    }

    #[test]
    fn maps_repositories_and_worktrees_into_git_dir() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");
        let pr = pr("dionysuzx", "forkcast");
        let task = task("https://github.com/o/r/pull/46#issuecomment-2");

        assert_eq!(
            worktrees.repo_dir(&pr).unwrap(),
            PathBuf::from("/tmp/maid-git/repos/dionysuzx/forkcast.git")
        );
        let worktree = worktrees.worktree_dir(&pr, &task).unwrap();
        assert!(worktree.starts_with("/tmp/maid-git/worktrees/dionysuzx/forkcast/46"));
        assert_eq!(
            worktrees.worktree_dir(&pr, &task).unwrap(),
            worktrees.worktree_dir(&pr, &task).unwrap()
        );
    }

    #[test]
    fn uses_distinct_worktrees_for_distinct_triggers_on_the_same_pull_request() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");
        let pr = pr("dionysuzx", "forkcast");

        assert_ne!(
            worktrees
                .worktree_dir(&pr, &task("https://github.com/o/r/pull/46#issuecomment-2"))
                .unwrap(),
            worktrees
                .worktree_dir(&pr, &task("https://github.com/o/r/pull/46#issuecomment-3"))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn prepares_and_cleans_up_distinct_git_worktrees_from_bare_repo() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["config", "user.name", "test"]);
        run_git(&source, &["config", "user.email", "test@example.com"]);
        std::fs::write(source.join("file.txt"), "hello\n").unwrap();
        run_git(&source, &["add", "file.txt"]);
        run_git(&source, &["commit", "-m", "initial"]);
        run_git(&source, &["update-ref", "refs/pull/46/head", "HEAD"]);

        let worktrees = GitWorktrees::new(temp.path().join("git"), "token");
        let pr = PullRequest {
            clone_url: source.to_string_lossy().to_string(),
            ..pr("o", "r")
        };

        let first = worktrees
            .prepare(&pr, &task("https://github.com/o/r/pull/46#issuecomment-2"))
            .await
            .unwrap();
        let second = worktrees
            .prepare(&pr, &task("https://github.com/o/r/pull/46#issuecomment-3"))
            .await
            .unwrap();

        assert_ne!(first.path(), second.path());
        assert!(worktrees.repo_dir(&pr).unwrap().join("HEAD").exists());
        assert_eq!(
            std::fs::read_to_string(first.path().join("file.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            std::fs::read_to_string(second.path().join("file.txt")).unwrap(),
            "hello\n"
        );

        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        worktrees.cleanup(first).await.unwrap();
        worktrees.cleanup(second).await.unwrap();

        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn rejects_repository_parts_that_could_escape_git_dir() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");

        let task = task("https://github.com/o/r/pull/46#issuecomment-2");

        assert!(worktrees.repo_dir(&pr("../dionysuzx", "forkcast")).is_err());
        assert!(
            worktrees
                .repo_dir(&pr("dionysuzx", "forkcast/slash"))
                .is_err()
        );
        assert!(
            worktrees
                .worktree_dir(&pr("../dionysuzx", "forkcast"), &task)
                .is_err()
        );
        assert!(
            worktrees
                .worktree_dir(&pr("dionysuzx", "forkcast/slash"), &task)
                .is_err()
        );
    }
}
