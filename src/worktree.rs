use crate::{
    domain::{CodexTask, WorkTarget, validate_repo_name_part},
    maid::{PreparedWorktree, Worktrees},
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose};
use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

const GIT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct GitWorktrees {
    root: PathBuf,
    auth_header: String,
    #[cfg(test)]
    test_source: Option<PathBuf>,
}

impl GitWorktrees {
    pub fn new(root: impl Into<PathBuf>, github_token: impl Into<String>) -> Self {
        let credential = format!("x-access-token:{}", github_token.into());
        let encoded = general_purpose::STANDARD.encode(credential);
        Self {
            root: absolute_root(root.into()),
            auth_header: format!("Authorization: Basic {encoded}"),
            #[cfg(test)]
            test_source: None,
        }
    }

    #[cfg(test)]
    fn with_test_source(root: impl Into<PathBuf>, source: impl Into<PathBuf>) -> Self {
        let mut worktrees = Self::new(root, "test-token");
        worktrees.test_source = Some(source.into());
        worktrees
    }

    pub fn worktree_dir(&self, target: &WorkTarget, task: &CodexTask) -> Result<PathBuf> {
        validate_repo_name_part(target.owner(), "repository owner")?;
        validate_repo_name_part(target.repo(), "repository name")?;
        Ok(self
            .root
            .join("worktrees")
            .join(target.owner())
            .join(target.repo())
            .join(target.kind())
            .join(target.number().to_string())
            .join(worktree_key(task)))
    }

    fn clone_origin(&self, target: &WorkTarget) -> Result<CloneOrigin> {
        validate_repo_name_part(target.owner(), "repository owner")?;
        validate_repo_name_part(target.repo(), "repository name")?;
        #[cfg(test)]
        if let Some(source) = &self.test_source {
            return Ok(CloneOrigin::Test(source.clone()));
        }
        Ok(CloneOrigin::GitHub(format!(
            "https://github.com/{}/{}.git",
            target.owner(),
            target.repo()
        )))
    }

    async fn run_git(&self, cwd: Option<&Path>, args: &[&str], authenticated: bool) -> Result<()> {
        let runtime_home = self.root.join("git-runtime");
        create_private_directory(&runtime_home)?;

        let mut command = Command::new("git");
        command
            .args(args)
            .env_clear()
            .env("PATH", GIT_PATH)
            .env("HOME", &runtime_home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("GIT_CONFIG_COUNT", if authenticated { "4" } else { "3" })
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", "/dev/null")
            .env("GIT_CONFIG_KEY_1", "http.followRedirects")
            .env("GIT_CONFIG_VALUE_1", "false")
            .env("GIT_CONFIG_KEY_2", "protocol.ext.allow")
            .env("GIT_CONFIG_VALUE_2", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if authenticated {
            command
                .env("GIT_CONFIG_KEY_3", "http.https://github.com/.extraHeader")
                .env("GIT_CONFIG_VALUE_3", &self.auth_header);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let child = command.spawn().context("failed to run git")?;
        let mut process_group = GitProcessGroup::new(&child);
        let output = timeout(GIT_COMMAND_TIMEOUT, child.wait_with_output())
            .await
            .context("git command exceeded its 5 minute time limit")??;
        process_group.disarm();
        if output.status.success() {
            return Ok(());
        }

        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[async_trait]
impl Worktrees for GitWorktrees {
    async fn prepare(&self, target: &WorkTarget, task: &CodexTask) -> Result<PreparedWorktree> {
        let worktree = self.worktree_dir(target, task)?;
        let origin = self.clone_origin(target)?;
        if worktree.exists() {
            tokio::fs::remove_dir_all(&worktree)
                .await
                .with_context(|| {
                    format!("failed to remove stale task repo {}", worktree.display())
                })?;
        }
        let parent = worktree
            .parent()
            .ok_or_else(|| anyhow!("task repository path has no parent"))?;
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;

        let worktree_string = worktree.to_string_lossy().to_string();
        self.run_git(None, &["init", "--quiet", &worktree_string], false)
            .await?;
        let fetch_ref = target.fetch_ref();
        self.run_git(
            Some(&worktree),
            &["fetch", "--quiet", "--no-tags", origin.as_str(), &fetch_ref],
            origin.needs_authentication(),
        )
        .await
        .with_context(|| format!("failed to fetch {}", target.html_url()))?;
        self.run_git(
            Some(&worktree),
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            false,
        )
        .await?;

        Ok(PreparedWorktree::new(worktree))
    }

    async fn cleanup(&self, worktree: PreparedWorktree) -> Result<()> {
        if worktree.path().exists() {
            tokio::fs::remove_dir_all(worktree.path())
                .await
                .with_context(|| format!("failed to remove {}", worktree.path().display()))?;
        }
        Ok(())
    }
}

struct GitProcessGroup {
    #[cfg(unix)]
    pid: Option<u32>,
}

impl GitProcessGroup {
    fn new(child: &tokio::process::Child) -> Self {
        Self {
            #[cfg(unix)]
            pid: child.id(),
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pid = None;
        }
    }
}

impl Drop for GitProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

enum CloneOrigin {
    GitHub(String),
    #[cfg(test)]
    Test(PathBuf),
}

impl CloneOrigin {
    fn as_str(&self) -> &str {
        match self {
            Self::GitHub(url) => url,
            #[cfg(test)]
            Self::Test(path) => path.to_str().expect("test source path must be UTF-8"),
        }
    }

    fn needs_authentication(&self) -> bool {
        matches!(self, Self::GitHub(_))
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect private directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "private directory must be a real directory: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure private directory {}", path.display()))?;
    }
    Ok(())
}

fn absolute_root(root: PathBuf) -> PathBuf {
    if root.is_absolute() {
        return root;
    }
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(root)
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
    use crate::domain::{Issue, PullRequest};
    use std::process::Command as StdCommand;

    fn pr(owner: &str, repo: &str) -> PullRequest {
        PullRequest {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: 46,
            author: "author".to_string(),
            api_url: "https://api.github.com/repos/o/r/pulls/46".to_string(),
            html_url: "https://github.com/o/r/pull/46".to_string(),
            clone_url: "https://untrusted.invalid/repo.git".to_string(),
        }
    }

    fn issue(owner: &str, repo: &str) -> Issue {
        Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: 322,
            author: "author".to_string(),
            api_url: "https://api.github.com/repos/o/r/issues/322".to_string(),
            html_url: "https://github.com/o/r/issues/322".to_string(),
            clone_url: "https://untrusted.invalid/repo.git".to_string(),
            default_branch: "main".to_string(),
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

    fn source_repo(root: &Path) -> PathBuf {
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["config", "user.name", "test"]);
        run_git(&source, &["config", "user.email", "test@example.com"]);
        std::fs::write(source.join("file.txt"), "hello\n").unwrap();
        run_git(&source, &["add", "file.txt"]);
        run_git(&source, &["commit", "-m", "initial"]);
        source
    }

    #[test]
    fn maps_distinct_triggers_to_distinct_task_repositories() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");
        let target = WorkTarget::PullRequest(pr("owner", "repo"));
        let first = worktrees
            .worktree_dir(
                &target,
                &task("https://github.com/o/r/pull/46#issuecomment-2"),
            )
            .unwrap();
        let second = worktrees
            .worktree_dir(
                &target,
                &task("https://github.com/o/r/pull/46#issuecomment-3"),
            )
            .unwrap();
        assert!(first.starts_with("/tmp/maid-git/worktrees/owner/repo/pulls/46"));
        assert_ne!(first, second);
    }

    #[test]
    fn derives_authenticated_origin_instead_of_trusting_api_clone_url() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");
        let target = WorkTarget::PullRequest(pr("Owner", "Repo"));
        assert_eq!(
            worktrees.clone_origin(&target).unwrap().as_str(),
            "https://github.com/Owner/Repo.git"
        );
    }

    #[tokio::test]
    async fn prepares_and_removes_a_fresh_repository_for_each_task() {
        let temp = tempfile::tempdir().unwrap();
        let source = source_repo(temp.path());
        run_git(&source, &["update-ref", "refs/pull/46/head", "HEAD"]);
        let worktrees = GitWorktrees::with_test_source(temp.path().join("git"), &source);
        let target = WorkTarget::PullRequest(pr("o", "r"));

        let prepared = worktrees
            .prepare(
                &target,
                &task("https://github.com/o/r/pull/46#issuecomment-2"),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(prepared.path().join("file.txt")).unwrap(),
            "hello\n"
        );
        assert!(prepared.path().join(".git").is_dir());
        assert!(
            !std::fs::read_to_string(prepared.path().join(".git/config"))
                .unwrap()
                .contains("token")
        );

        let path = prepared.path().to_path_buf();
        worktrees.cleanup(prepared).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn prepares_issue_task_from_default_branch() {
        let temp = tempfile::tempdir().unwrap();
        let source = source_repo(temp.path());
        run_git(&source, &["branch", "-M", "main"]);
        let worktrees = GitWorktrees::with_test_source(temp.path().join("git"), &source);
        let target = WorkTarget::Issue(issue("o", "r"));

        let prepared = worktrees
            .prepare(
                &target,
                &task("https://github.com/o/r/issues/322#issuecomment-2"),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(prepared.path().join("file.txt")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn rejects_repository_parts_that_could_escape_git_dir() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token");
        let invalid_owner = WorkTarget::PullRequest(pr("../owner", "repo"));
        let invalid_repo = WorkTarget::Issue(issue("owner", "repo/slash"));
        assert!(
            worktrees
                .worktree_dir(&invalid_owner, &task("trigger"))
                .is_err()
        );
        assert!(worktrees.clone_origin(&invalid_repo).is_err());
    }
}
