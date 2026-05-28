use crate::{
    domain::{Issue, PullRequest, validate_repo_name_part},
    maid::RepoWorkspace,
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct GitRepoCache {
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

impl GitRepoCache {
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
        Ok(self.root.join("repos").join(&pr.owner).join(&pr.repo))
    }

    pub fn issue_repo_dir(&self, issue: &Issue) -> Result<PathBuf> {
        validate_repo_name_part(&issue.owner, "repository owner")?;
        validate_repo_name_part(&issue.repo, "repository name")?;
        Ok(self.root.join("repos").join(&issue.owner).join(&issue.repo))
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
}

#[async_trait]
impl RepoWorkspace for GitRepoCache {
    async fn prepare_pr_review(&self, pr: &PullRequest) -> Result<PathBuf> {
        let checkout = self.repo_dir(pr)?;
        tokio::fs::create_dir_all(
            checkout
                .parent()
                .ok_or_else(|| anyhow!("checkout path has no parent"))?,
        )
        .await?;

        if !checkout.join(".git").exists() {
            let checkout_string = checkout.to_string_lossy().to_string();
            self.run_git(
                &self.bot_auth,
                None,
                &["clone", &pr.clone_url, &checkout_string],
            )
            .await
            .with_context(|| format!("failed to clone {}", pr.repo_key()))?;
        }

        self.run_git(
            &self.bot_auth,
            Some(&checkout),
            &["remote", "set-url", "origin", &pr.clone_url],
        )
        .await?;
        let pr_head = format!("pull/{}/head", pr.number);
        self.run_git(
            &self.bot_auth,
            Some(&checkout),
            &["fetch", "--prune", "origin", &pr_head],
        )
        .await
        .with_context(|| format!("failed to fetch PR {}", pr.html_url))?;
        self.run_git(
            &self.bot_auth,
            Some(&checkout),
            &["switch", "--detach", "--force", "FETCH_HEAD"],
        )
        .await?;
        self.run_git(&self.bot_auth, Some(&checkout), &["clean", "-fdx"])
            .await?;

        Ok(checkout)
    }

    async fn prepare_issue_branch(&self, issue: &Issue, branch: &str) -> Result<PathBuf> {
        let checkout = self.issue_repo_dir(issue)?;
        tokio::fs::create_dir_all(
            checkout
                .parent()
                .ok_or_else(|| anyhow!("checkout path has no parent"))?,
        )
        .await?;

        if !checkout.join(".git").exists() {
            let checkout_string = checkout.to_string_lossy().to_string();
            self.run_git(
                &self.issue_git_auth,
                None,
                &["clone", self.issue_remote_url(issue), &checkout_string],
            )
            .await
            .with_context(|| format!("failed to clone {}", issue.repo_key()))?;
        }

        self.run_git(
            &self.issue_git_auth,
            Some(&checkout),
            &["remote", "set-url", "origin", self.issue_remote_url(issue)],
        )
        .await?;
        let remote_ref = format!(
            "+refs/heads/{}:refs/remotes/origin/{}",
            issue.default_branch, issue.default_branch
        );
        self.run_git(
            &self.issue_git_auth,
            Some(&checkout),
            &["fetch", "--prune", "origin", &remote_ref],
        )
        .await
        .with_context(|| format!("failed to fetch {}", issue.default_branch))?;
        let remote_branch_ref = format!("refs/heads/{branch}");
        if self
            .git_output(
                &self.issue_git_auth,
                Some(&checkout),
                &["ls-remote", "--exit-code", "origin", &remote_branch_ref],
            )
            .await
            .is_ok()
        {
            let remote_issue_ref = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
            self.run_git(
                &self.issue_git_auth,
                Some(&checkout),
                &["fetch", "origin", &remote_issue_ref],
            )
            .await
            .with_context(|| format!("failed to fetch existing issue branch {branch}"))?;
        }
        let base = format!("origin/{}", issue.default_branch);
        self.run_git(
            &self.issue_git_auth,
            Some(&checkout),
            &["switch", "-C", branch, &base],
        )
        .await?;
        self.run_git(
            &self.issue_git_auth,
            Some(&checkout),
            &["reset", "--hard", &base],
        )
        .await?;
        self.run_git(&self.issue_git_auth, Some(&checkout), &["clean", "-fdx"])
            .await?;

        Ok(checkout)
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

    #[test]
    fn maps_repositories_into_the_maid_cache() {
        let cache = GitRepoCache::new("/tmp/maid-cache", "token", "maid-bot");

        assert_eq!(
            cache.repo_dir(&pr("dionysuzx", "forkcast")).unwrap(),
            PathBuf::from("/tmp/maid-cache/repos/dionysuzx/forkcast")
        );
        assert_eq!(
            cache.repo_dir(&pr("dionysuzx", "forkcast")).unwrap(),
            cache.repo_dir(&pr("dionysuzx", "forkcast")).unwrap()
        );
    }

    #[test]
    fn rejects_repository_parts_that_could_escape_the_cache() {
        let cache = GitRepoCache::new("/tmp/maid-cache", "token", "maid-bot");

        assert!(cache.repo_dir(&pr("../dionysuzx", "forkcast")).is_err());
        assert!(cache.repo_dir(&pr("dionysuzx", "forkcast/slash")).is_err());
    }

    #[test]
    fn host_issue_git_auth_uses_ssh_remote() {
        let cache = GitRepoCache::new("/tmp/maid-cache", "token", "maid-bot")
            .with_issue_publish_mode(IssueGitAuth::Host, IssueCommitIdentity::Host);

        assert_eq!(
            cache.issue_remote_url(&issue("o", "r")),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn bot_issue_git_auth_uses_https_remote() {
        let cache = GitRepoCache::new("/tmp/maid-cache", "token", "maid-bot");

        assert_eq!(
            cache.issue_remote_url(&issue("o", "r")),
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
