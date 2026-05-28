use crate::{
    domain::{Issue, PullRequest, validate_repo_name_part},
    maid::RepoWorkspace,
};
use anyhow::{Context, Result, anyhow};
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
    auth_header: String,
    commit_author_name: String,
    commit_author_email: String,
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
        Self {
            root: root.into(),
            auth_header: format!("Authorization: Basic {encoded}"),
            commit_author_name: bot_login.clone(),
            commit_author_email: format!("{bot_login}@users.noreply.github.com"),
        }
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

    async fn run_git(&self, cwd: Option<&Path>, args: &[&str]) -> Result<()> {
        self.git_output(cwd, args).await.map(|_| ())
    }

    async fn git_output(&self, cwd: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
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
            return Ok(output.stdout);
        }

        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
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
            self.run_git(None, &["clone", &pr.clone_url, &checkout_string])
                .await
                .with_context(|| format!("failed to clone {}", pr.repo_key()))?;
        }

        self.run_git(
            Some(&checkout),
            &["remote", "set-url", "origin", &pr.clone_url],
        )
        .await?;
        let pr_head = format!("pull/{}/head", pr.number);
        self.run_git(Some(&checkout), &["fetch", "--prune", "origin", &pr_head])
            .await
            .with_context(|| format!("failed to fetch PR {}", pr.html_url))?;
        self.run_git(
            Some(&checkout),
            &["switch", "--detach", "--force", "FETCH_HEAD"],
        )
        .await?;
        self.run_git(Some(&checkout), &["clean", "-fdx"]).await?;

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
            self.run_git(None, &["clone", &issue.clone_url, &checkout_string])
                .await
                .with_context(|| format!("failed to clone {}", issue.repo_key()))?;
        }

        self.run_git(
            Some(&checkout),
            &["remote", "set-url", "origin", &issue.clone_url],
        )
        .await?;
        let remote_ref = format!(
            "+refs/heads/{}:refs/remotes/origin/{}",
            issue.default_branch, issue.default_branch
        );
        self.run_git(
            Some(&checkout),
            &["fetch", "--prune", "origin", &remote_ref],
        )
        .await
        .with_context(|| format!("failed to fetch {}", issue.default_branch))?;
        let remote_branch_ref = format!("refs/heads/{branch}");
        if self
            .git_output(
                Some(&checkout),
                &["ls-remote", "--exit-code", "origin", &remote_branch_ref],
            )
            .await
            .is_ok()
        {
            let remote_issue_ref = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
            self.run_git(Some(&checkout), &["fetch", "origin", &remote_issue_ref])
                .await
                .with_context(|| format!("failed to fetch existing issue branch {branch}"))?;
        }
        let base = format!("origin/{}", issue.default_branch);
        self.run_git(Some(&checkout), &["switch", "-C", branch, &base])
            .await?;
        self.run_git(Some(&checkout), &["reset", "--hard", &base])
            .await?;
        self.run_git(Some(&checkout), &["clean", "-fdx"]).await?;

        Ok(checkout)
    }

    async fn has_changes(&self, checkout: &Path) -> Result<bool> {
        let output = self
            .git_output(Some(checkout), &["status", "--porcelain"])
            .await?;
        Ok(!output.is_empty())
    }

    async fn commit_all(&self, checkout: &Path, message: &str) -> Result<()> {
        self.run_git(Some(checkout), &["add", "-A"]).await?;
        self.run_git(
            Some(checkout),
            &[
                "-c",
                &format!("user.name={}", self.commit_author_name),
                "-c",
                &format!("user.email={}", self.commit_author_email),
                "commit",
                "-m",
                message,
            ],
        )
        .await
    }

    async fn push_branch(&self, checkout: &Path, branch: &str) -> Result<()> {
        let refspec = format!("HEAD:refs/heads/{branch}");
        self.run_git(
            Some(checkout),
            &["push", "--force-with-lease", "origin", &refspec],
        )
        .await
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
}
