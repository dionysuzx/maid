use crate::{
    domain::{CodexTask, Issue, PullRequest, validate_repo_name_part},
    maid::{PreparedWorktree, Worktrees},
};
use anyhow::{Context, Result, anyhow, bail};
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
    bot_auth: GitAuth,
    issue_git_auth: GitAuth,
    issue_commit_identity: CommitIdentity,
    expected_git_identity: Option<ExpectedGitIdentity>,
}

#[derive(Clone, Debug)]
pub enum IssueGitAuth {
    Bot,
    Host,
}

#[derive(Clone, Debug)]
pub enum IssueCommitIdentity {
    Bot,
    Host,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExpectedGitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
    pub gpgsign: Option<bool>,
    pub gpg_format: Option<String>,
}

#[derive(Clone, Debug)]
enum GitAuth {
    ExtraHeader(String),
    Host,
}

#[derive(Clone, Debug)]
enum CommitIdentity {
    Bot { name: String, email: String },
    Host,
}

impl GitWorktrees {
    pub fn new(
        root: impl Into<PathBuf>,
        github_token: impl Into<String>,
        bot_login: impl Into<String>,
    ) -> Self {
        let credential = format!("x-access-token:{}", github_token.into());
        let encoded = general_purpose::STANDARD.encode(credential);
        let bot_login = bot_login.into();
        let bot_auth = GitAuth::ExtraHeader(format!("Authorization: Basic {encoded}"));
        Self {
            root: root.into(),
            bot_auth: bot_auth.clone(),
            issue_git_auth: bot_auth,
            issue_commit_identity: CommitIdentity::Bot {
                name: bot_login.clone(),
                email: format!("{bot_login}@users.noreply.github.com"),
            },
            expected_git_identity: None,
        }
    }

    pub fn with_issue_publish_mode(
        mut self,
        git_auth: IssueGitAuth,
        commit_identity: IssueCommitIdentity,
    ) -> Self {
        self.issue_git_auth = match git_auth {
            IssueGitAuth::Bot => self.bot_auth.clone(),
            IssueGitAuth::Host => GitAuth::Host,
        };
        if matches!(commit_identity, IssueCommitIdentity::Host) {
            self.issue_commit_identity = CommitIdentity::Host;
        }
        self
    }

    pub fn with_expected_git_identity(mut self, expected: Option<ExpectedGitIdentity>) -> Self {
        self.expected_git_identity = expected;
        self
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

    pub fn issue_repo_dir(&self, issue: &Issue) -> Result<PathBuf> {
        validate_repo_name_part(&issue.owner, "repository owner")?;
        validate_repo_name_part(&issue.repo, "repository name")?;
        Ok(self
            .root
            .join("repos")
            .join(&issue.owner)
            .join(format!("{}.git", issue.repo)))
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

    pub fn issue_worktree_dir(&self, issue: &Issue) -> Result<PathBuf> {
        validate_repo_name_part(&issue.owner, "repository owner")?;
        validate_repo_name_part(&issue.repo, "repository name")?;
        Ok(self
            .root
            .join("worktrees")
            .join(&issue.owner)
            .join(&issue.repo)
            .join("issues")
            .join(issue.number.to_string()))
    }

    fn issue_remote_url<'a>(&self, issue: &'a Issue) -> &'a str {
        match &self.issue_git_auth {
            GitAuth::ExtraHeader(_) => &issue.clone_url,
            GitAuth::Host => &issue.ssh_url,
        }
    }

    async fn run_git(&self, auth: &GitAuth, cwd: Option<&Path>, args: &[&str]) -> Result<()> {
        self.git_output(auth, cwd, args).await.map(|_| ())
    }

    async fn git_output(
        &self,
        auth: &GitAuth,
        cwd: Option<&Path>,
        args: &[&str],
    ) -> Result<Vec<u8>> {
        let mut command = Command::new("git");
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let GitAuth::ExtraHeader(auth_header) = auth {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraheader")
                .env("GIT_CONFIG_VALUE_0", auth_header);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        let output = command.output().await.context("failed to run git")?;
        if output.status.success() {
            return Ok(output.stdout);
        }

        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }

    async fn git_config_value(&self, checkout: &Path, key: &str) -> Result<Option<String>> {
        let output = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(checkout)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to run git config")?;

        if output.status.success() {
            return Ok(Some(
                String::from_utf8(output.stdout)
                    .context("git config returned output that was not valid UTF-8")?
                    .trim()
                    .to_string(),
            ));
        }

        if output.status.code() == Some(1) {
            return Ok(None);
        }

        Err(anyhow!(
            "git config --get {key} failed: {}",
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
            self.run_git(
                &self.bot_auth,
                None,
                &["clone", "--bare", &pr.clone_url, &repo_string],
            )
            .await
            .with_context(|| format!("failed to clone bare repo {}", pr.repo_key()))?;
        }

        self.run_git(
            &self.bot_auth,
            Some(&repo),
            &["remote", "set-url", "origin", &pr.clone_url],
        )
        .await?;
        let pr_head = format!("pull/{}/head", pr.number);
        self.run_git(
            &self.bot_auth,
            Some(&repo),
            &["fetch", "--prune", "origin", &pr_head],
        )
        .await
        .with_context(|| format!("failed to fetch PR {}", pr.html_url))?;

        let worktree_string = worktree.to_string_lossy().to_string();
        if worktree.exists() {
            let _ = self
                .run_git(
                    &self.bot_auth,
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

        self.run_git(&self.bot_auth, Some(&repo), &["worktree", "prune"])
            .await?;
        self.run_git(
            &self.bot_auth,
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

    async fn prepare_issue_branch(&self, issue: &Issue, branch: &str) -> Result<PreparedWorktree> {
        let repo = self.issue_repo_dir(issue)?;
        let worktree = self.issue_worktree_dir(issue)?;
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
            self.run_git(
                &self.issue_git_auth,
                None,
                &[
                    "clone",
                    "--bare",
                    self.issue_remote_url(issue),
                    &repo_string,
                ],
            )
            .await
            .with_context(|| format!("failed to clone bare repo {}", issue.repo_key()))?;
        }

        self.run_git(
            &self.issue_git_auth,
            Some(&repo),
            &["remote", "set-url", "origin", self.issue_remote_url(issue)],
        )
        .await?;
        let remote_ref = format!(
            "+refs/heads/{}:refs/remotes/origin/{}",
            issue.default_branch, issue.default_branch
        );
        self.run_git(
            &self.issue_git_auth,
            Some(&repo),
            &["fetch", "--prune", "origin", &remote_ref],
        )
        .await
        .with_context(|| format!("failed to fetch {}", issue.default_branch))?;

        let worktree_string = worktree.to_string_lossy().to_string();
        if worktree.exists() {
            let _ = self
                .run_git(
                    &self.issue_git_auth,
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

        self.run_git(&self.issue_git_auth, Some(&repo), &["worktree", "prune"])
            .await?;
        let base = format!("origin/{}", issue.default_branch);
        self.run_git(
            &self.issue_git_auth,
            Some(&repo),
            &["worktree", "add", "-B", branch, &worktree_string, &base],
        )
        .await?;
        self.run_git(
            &self.issue_git_auth,
            Some(&worktree),
            &["reset", "--hard", &base],
        )
        .await?;
        self.run_git(&self.issue_git_auth, Some(&worktree), &["clean", "-fdx"])
            .await?;

        Ok(PreparedWorktree::git_worktree(repo, worktree))
    }

    async fn verify_issue_commit_identity(&self, checkout: &Path) -> Result<()> {
        let CommitIdentity::Host = self.issue_commit_identity else {
            return Ok(());
        };
        let Some(expected) = &self.expected_git_identity else {
            return Ok(());
        };

        let actual = EffectiveGitIdentity {
            name: self.git_config_value(checkout, "user.name").await?,
            email: self.git_config_value(checkout, "user.email").await?,
            gpgsign: self.git_config_value(checkout, "commit.gpgsign").await?,
            gpg_format: self.git_config_value(checkout, "gpg.format").await?,
        };

        verify_expected_git_identity(expected, &actual)
    }

    async fn has_changes(&self, checkout: &Path) -> Result<bool> {
        let output = self
            .git_output(
                &self.issue_git_auth,
                Some(checkout),
                &["status", "--porcelain"],
            )
            .await?;
        Ok(!output.is_empty())
    }

    async fn commit_all(&self, checkout: &Path, message: &str) -> Result<()> {
        self.run_git(&self.issue_git_auth, Some(checkout), &["add", "-A"])
            .await?;
        match &self.issue_commit_identity {
            CommitIdentity::Bot { name, email } => {
                self.run_git(
                    &self.issue_git_auth,
                    Some(checkout),
                    &[
                        "-c",
                        &format!("user.name={name}"),
                        "-c",
                        &format!("user.email={email}"),
                        "commit",
                        "-m",
                        message,
                    ],
                )
                .await
            }
            CommitIdentity::Host => {
                self.run_git(
                    &self.issue_git_auth,
                    Some(checkout),
                    &["commit", "-m", message],
                )
                .await
            }
        }
    }

    async fn push_branch(&self, checkout: &Path, branch: &str) -> Result<()> {
        let refspec = format!("HEAD:refs/heads/{branch}");
        self.run_git(
            &self.issue_git_auth,
            Some(checkout),
            &["push", "--force-with-lease", "origin", &refspec],
        )
        .await
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
                &self.bot_auth,
                Some(&repo),
                &["worktree", "remove", "--force", &worktree_string],
            )
            .await;
        if worktree.path().exists() {
            tokio::fs::remove_dir_all(worktree.path())
                .await
                .with_context(|| format!("failed to remove {}", worktree.path().display()))?;
        }
        self.run_git(&self.bot_auth, Some(&repo), &["worktree", "prune"])
            .await?;
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct EffectiveGitIdentity {
    name: Option<String>,
    email: Option<String>,
    gpgsign: Option<String>,
    gpg_format: Option<String>,
}

fn verify_expected_git_identity(
    expected: &ExpectedGitIdentity,
    actual: &EffectiveGitIdentity,
) -> Result<()> {
    verify_expected_string(
        "user.name",
        expected.name.as_deref(),
        actual.name.as_deref(),
    )?;
    verify_expected_string(
        "user.email",
        expected.email.as_deref(),
        actual.email.as_deref(),
    )?;
    verify_expected_string(
        "gpg.format",
        expected.gpg_format.as_deref(),
        actual.gpg_format.as_deref(),
    )?;

    if let Some(expected_gpgsign) = expected.gpgsign {
        let Some(actual_gpgsign) = &actual.gpgsign else {
            bail!(
                "host git identity mismatch for commit.gpgsign: expected {expected_gpgsign}, got unset"
            );
        };
        let actual_gpgsign = parse_git_bool(actual_gpgsign).with_context(|| {
            format!(
                "host git identity mismatch for commit.gpgsign: invalid value {actual_gpgsign:?}"
            )
        })?;
        if actual_gpgsign != expected_gpgsign {
            bail!(
                "host git identity mismatch for commit.gpgsign: expected {expected_gpgsign}, got {actual_gpgsign}"
            );
        }
    }

    Ok(())
}

fn verify_expected_string(key: &str, expected: Option<&str>, actual: Option<&str>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    match actual {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => {
            bail!("host git identity mismatch for {key}: expected {expected:?}, got {actual:?}")
        }
        None => bail!("host git identity mismatch for {key}: expected {expected:?}, got unset"),
    }
}

fn parse_git_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        value => bail!("not a git boolean: {value:?}"),
    }
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
            subject_url: "https://github.com/o/r/pull/46".to_string(),
            origin: crate::domain::CodexTaskOrigin::Mention {
                mention_url: trigger.to_string(),
                raw_body: "@maid-bot review".to_string(),
                cleaned_text: "review".to_string(),
            },
        }
    }

    fn issue(owner: &str, repo: &str) -> Issue {
        Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: 12,
            author: "dionysuzx".to_string(),
            title: "Add issue mode".to_string(),
            body: "body".to_string(),
            api_url: "https://api.github.com/repos/o/r/issues/12".to_string(),
            html_url: "https://github.com/o/r/issues/12".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
            ssh_url: "git@github.com:o/r.git".to_string(),
            default_branch: "main".to_string(),
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
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token", "maid-bot");
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
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token", "maid-bot");
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

        let worktrees = GitWorktrees::new(temp.path().join("git"), "token", "maid-bot");
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
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token", "maid-bot");

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

    #[test]
    fn maps_issue_repositories_and_worktrees_into_git_dir() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token", "maid-bot");
        let issue = issue("dionysuzx", "forkcast");

        assert_eq!(
            worktrees.issue_repo_dir(&issue).unwrap(),
            PathBuf::from("/tmp/maid-git/repos/dionysuzx/forkcast.git")
        );
        assert_eq!(
            worktrees.issue_worktree_dir(&issue).unwrap(),
            PathBuf::from("/tmp/maid-git/worktrees/dionysuzx/forkcast/issues/12")
        );
    }

    #[test]
    fn host_issue_git_auth_uses_ssh_remote() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token", "maid-bot")
            .with_issue_publish_mode(IssueGitAuth::Host, IssueCommitIdentity::Host);

        assert_eq!(
            worktrees.issue_remote_url(&issue("o", "r")),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn bot_issue_git_auth_uses_https_remote() {
        let worktrees = GitWorktrees::new("/tmp/maid-git", "token", "maid-bot");

        assert_eq!(
            worktrees.issue_remote_url(&issue("o", "r")),
            "https://github.com/o/r.git"
        );
    }

    #[test]
    fn accepts_matching_expected_git_identity() {
        let expected = ExpectedGitIdentity {
            name: Some("Dionysus".to_string()),
            email: Some("dionysuzx@users.noreply.github.com".to_string()),
            gpgsign: Some(true),
            gpg_format: Some("ssh".to_string()),
        };
        let actual = EffectiveGitIdentity {
            name: Some("Dionysus".to_string()),
            email: Some("dionysuzx@users.noreply.github.com".to_string()),
            gpgsign: Some("yes".to_string()),
            gpg_format: Some("ssh".to_string()),
        };

        verify_expected_git_identity(&expected, &actual).unwrap();
    }

    #[test]
    fn rejects_mismatched_expected_git_identity() {
        let expected = ExpectedGitIdentity {
            email: Some("dionysuzx@users.noreply.github.com".to_string()),
            ..ExpectedGitIdentity::default()
        };
        let actual = EffectiveGitIdentity {
            email: Some("maid-bot@users.noreply.github.com".to_string()),
            ..EffectiveGitIdentity::default()
        };

        assert!(verify_expected_git_identity(&expected, &actual).is_err());
    }

    #[test]
    fn parses_git_boolean_values() {
        assert!(parse_git_bool("true").unwrap());
        assert!(parse_git_bool("on").unwrap());
        assert!(!parse_git_bool("false").unwrap());
        assert!(!parse_git_bool("0").unwrap());
        assert!(parse_git_bool("sometimes").is_err());
    }
}
