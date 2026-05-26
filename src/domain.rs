use anyhow::{Result, anyhow};
use regex::Regex;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub owner: String,
    pub repo: String,
    pub number: u64,
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
    pub mention_url: String,
    pub pr_url: String,
    pub raw_body: String,
    pub cleaned_text: String,
}

impl CodexTask {
    pub fn prompt(&self) -> String {
        format!(
            "\
You are responding to a GitHub pull request mention.

Inspect the checkout in your current working directory and answer the request.
Return only the GitHub comment body to post. Do not include tool logs or wrappers.

Mention URL:
{mention_url}

Pull request URL:
{pr_url}

Raw mention body:
{raw_body}

Cleaned request text:
{cleaned_text}
",
            mention_url = self.mention_url,
            pr_url = self.pr_url,
            raw_body = self.raw_body,
            cleaned_text = self.cleaned_text,
        )
    }
}

pub fn validate_repo_name_part(value: &str, label: &str) -> Result<()> {
    let valid = !value.is_empty()
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
            mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            raw_body: "@maid-bot review".to_string(),
            cleaned_text: "review".to_string(),
        };

        let prompt = task.prompt();
        assert!(prompt.contains("Mention URL:\nhttps://github.com/o/r/pull/1#issuecomment-2"));
        assert!(prompt.contains("Pull request URL:\nhttps://github.com/o/r/pull/1"));
        assert!(prompt.contains("Raw mention body:\n@maid-bot review"));
        assert!(prompt.contains("Cleaned request text:\nreview"));
    }
}
