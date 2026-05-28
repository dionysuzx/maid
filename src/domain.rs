use anyhow::{Result, anyhow};
use regex::Regex;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    pub id: String,
    pub reason: String,
    pub subject_kind: String,
    pub subject_url: Option<String>,
    pub latest_comment_url: Option<String>,
}

impl Notification {
    pub fn is_pr_mention_candidate(&self) -> bool {
        self.reason == "mention"
            && self.subject_kind == "PullRequest"
            && self.subject_url.is_some()
            && self.latest_comment_url.is_some()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RepoSlug {
    pub owner: String,
    pub repo: String,
}

impl RepoSlug {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        let Some((owner, repo)) = value.split_once('/') else {
            return Err(anyhow!(
                "repository must be formatted as owner/repo: {value:?}"
            ));
        };
        if repo.contains('/') {
            return Err(anyhow!(
                "repository must be formatted as owner/repo: {value:?}"
            ));
        }

        validate_repo_name_part(owner, "repository owner")?;
        validate_repo_name_part(repo, "repository name")?;
        Ok(Self {
            owner: owner.to_ascii_lowercase(),
            repo: repo.to_ascii_lowercase(),
        })
    }
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.repo)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub author: String,
    pub api_url: String,
    pub html_url: String,
    pub clone_url: String,
}

impl PullRequest {
    pub fn repo_key(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Issue {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub author: String,
    pub title: String,
    pub body: String,
    pub api_url: String,
    pub html_url: String,
    pub clone_url: String,
    pub default_branch: String,
}

impl Issue {
    pub fn repo_key(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    pub fn implementation_branch(&self) -> String {
        format!("maid/issue-{}", self.number)
    }

    pub fn pull_request_title(&self) -> String {
        format!("Implement issue #{}: {}", self.number, self.title)
    }

    pub fn pull_request_body(&self, summary: &str) -> String {
        format!(
            "\
Closes #{number}

{summary}
",
            number = self.number,
            summary = summary.trim(),
        )
    }

    pub fn no_changes_comment(&self) -> String {
        format!(
            "I looked at this issue but did not produce a code change for #{}.",
            self.number
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommentMention {
    pub author: String,
    pub body: String,
    pub api_url: String,
    pub html_url: String,
    pub pr: PullRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionRequest {
    pub raw_body: String,
    pub cleaned_text: String,
}

impl MentionRequest {
    pub fn parse(body: &str, bot_login: &str) -> Result<Option<Self>> {
        let pattern = Regex::new(&format!(r"(?i)@{}\b", regex::escape(bot_login)))?;
        if !pattern.is_match(body) {
            return Ok(None);
        }

        let cleaned_text = pattern.replace_all(body, "").trim().to_string();
        Ok(Some(Self {
            raw_body: body.to_string(),
            cleaned_text,
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexTask {
    pub subject_url: String,
    pub origin: CodexTaskOrigin,
}

impl CodexTask {
    pub fn prompt(&self) -> String {
        match &self.origin {
            CodexTaskOrigin::Mention {
                mention_url,
                raw_body,
                cleaned_text,
            } => format!(
                "\
You are responding to a GitHub pull request mention.

Inspect the checkout in your current working directory and answer the request.
Return only the GitHub comment body to post. Do not include tool logs or wrappers.

Mention URL:
{mention_url}

Pull request URL:
{subject_url}

Raw mention body:
{raw_body}

Cleaned request text:
{cleaned_text}
",
                mention_url = mention_url,
                subject_url = self.subject_url,
                raw_body = raw_body,
                cleaned_text = cleaned_text,
            ),
            CodexTaskOrigin::PullRequestOpened { author } => format!(
                "\
You are responding automatically to a GitHub pull request opened by a trusted account.

Inspect the checkout in your current working directory and review the pull request.
Return only the GitHub comment body to post. Do not include tool logs or wrappers.

Pull request URL:
{subject_url}

Opened by:
{author}

Review request:
please review
",
                subject_url = self.subject_url,
                author = author,
            ),
            CodexTaskOrigin::IssueImplementation {
                title,
                body,
                branch,
            } => format!(
                "\
You are implementing a GitHub issue from a trusted label trigger.

Inspect the checkout in your current working directory, make the smallest code change that correctly implements the issue, and run the relevant checks when practical.
Leave your changes in the working tree. Do not commit, push, create branches, or open pull requests.
Return only a concise pull request body describing what changed and what you checked. Do not include tool logs or wrappers.

Issue URL:
{subject_url}

Branch Maid will publish:
{branch}

Issue title:
{title}

Issue body:
{body}
",
                subject_url = self.subject_url,
                branch = branch,
                title = title,
                body = body,
            ),
        }
    }

    pub fn trigger_url(&self) -> &str {
        match &self.origin {
            CodexTaskOrigin::Mention { mention_url, .. } => mention_url,
            CodexTaskOrigin::PullRequestOpened { .. }
            | CodexTaskOrigin::IssueImplementation { .. } => &self.subject_url,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexTaskOrigin {
    Mention {
        mention_url: String,
        raw_body: String,
        cleaned_text: String,
    },
    PullRequestOpened {
        author: String,
    },
    IssueImplementation {
        title: String,
        body: String,
        branch: String,
    },
}

pub fn validate_repo_name_part(value: &str, label: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));

    if valid {
        Ok(())
    } else {
        Err(anyhow!("invalid {label}: {value:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_to_pull_request_mention_notifications_with_comment_urls() {
        let eligible = Notification {
            id: "1".to_string(),
            reason: "mention".to_string(),
            subject_kind: "PullRequest".to_string(),
            subject_url: Some("https://api.github.com/repos/o/r/pulls/1".to_string()),
            latest_comment_url: Some(
                "https://api.github.com/repos/o/r/issues/comments/2".to_string(),
            ),
        };

        assert!(eligible.is_pr_mention_candidate());

        for notification in [
            Notification {
                reason: "comment".to_string(),
                ..eligible.clone()
            },
            Notification {
                subject_kind: "Issue".to_string(),
                ..eligible.clone()
            },
            Notification {
                latest_comment_url: None,
                ..eligible.clone()
            },
        ] {
            assert!(!notification.is_pr_mention_candidate());
        }
    }

    #[test]
    fn parses_mentions_and_cleans_request_text() {
        let request = MentionRequest::parse("@maid-bot please review this PR", "maid-bot")
            .unwrap()
            .unwrap();

        assert_eq!(request.raw_body, "@maid-bot please review this PR");
        assert_eq!(request.cleaned_text, "please review this PR");

        assert!(
            MentionRequest::parse("@maid-botx not you", "maid-bot")
                .unwrap()
                .is_none()
        );
        assert!(
            MentionRequest::parse("@MAID-BOT check this", "maid-bot")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn builds_codex_prompt_with_required_context() {
        let task = CodexTask {
            subject_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot review".to_string(),
                cleaned_text: "review".to_string(),
            },
        };

        let prompt = task.prompt();
        assert!(prompt.contains("Mention URL:\nhttps://github.com/o/r/pull/1#issuecomment-2"));
        assert!(prompt.contains("Pull request URL:\nhttps://github.com/o/r/pull/1"));
        assert!(prompt.contains("Raw mention body:\n@maid-bot review"));
        assert!(prompt.contains("Cleaned request text:\nreview"));
    }

    #[test]
    fn builds_codex_prompt_for_automatic_pull_request_review() {
        let task = CodexTask {
            subject_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::PullRequestOpened {
                author: "dionysuzx".to_string(),
            },
        };

        let prompt = task.prompt();
        assert!(prompt.contains("automatically to a GitHub pull request"));
        assert!(prompt.contains("Pull request URL:\nhttps://github.com/o/r/pull/1"));
        assert!(prompt.contains("Opened by:\ndionysuzx"));
        assert!(prompt.contains("Review request:\nplease review"));
    }

    #[test]
    fn parses_repository_slugs() {
        assert_eq!(
            RepoSlug::parse(" Dionysuzx/Maid ").unwrap(),
            RepoSlug {
                owner: "dionysuzx".to_string(),
                repo: "maid".to_string(),
            }
        );

        assert!(RepoSlug::parse("dionysuzx").is_err());
        assert!(RepoSlug::parse("dionysuzx/maid/extra").is_err());
        assert!(RepoSlug::parse("../maid").is_err());
    }
}
