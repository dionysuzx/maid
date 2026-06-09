use anyhow::{Result, anyhow};
use std::fmt;

pub const OPERATOR_TRIGGER: &str = "/operate";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    pub id: String,
    pub reason: String,
    pub subject_kind: String,
    pub subject_url: Option<String>,
    pub latest_comment_url: Option<String>,
    pub unread: bool,
    pub updated_at: String,
}

impl Notification {
    pub fn is_mention_candidate(&self) -> bool {
        matches!(self.subject_kind.as_str(), "PullRequest" | "Issue") && self.subject_url.is_some()
    }

    pub fn is_read(&self) -> bool {
        !self.unread
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
    pub api_url: String,
    pub html_url: String,
    pub clone_url: String,
    pub default_branch: String,
}

impl Issue {
    pub fn repo_key(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkTarget {
    PullRequest(PullRequest),
    Issue(Issue),
}

impl WorkTarget {
    pub fn owner(&self) -> &str {
        match self {
            Self::PullRequest(pr) => &pr.owner,
            Self::Issue(issue) => &issue.owner,
        }
    }

    pub fn repo(&self) -> &str {
        match self {
            Self::PullRequest(pr) => &pr.repo,
            Self::Issue(issue) => &issue.repo,
        }
    }

    pub fn number(&self) -> u64 {
        match self {
            Self::PullRequest(pr) => pr.number,
            Self::Issue(issue) => issue.number,
        }
    }

    pub fn html_url(&self) -> &str {
        match self {
            Self::PullRequest(pr) => &pr.html_url,
            Self::Issue(issue) => &issue.html_url,
        }
    }

    pub fn clone_url(&self) -> &str {
        match self {
            Self::PullRequest(pr) => &pr.clone_url,
            Self::Issue(issue) => &issue.clone_url,
        }
    }

    pub fn repo_key(&self) -> String {
        format!("{}/{}", self.owner(), self.repo())
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::PullRequest(_) => "pulls",
            Self::Issue(_) => "issues",
        }
    }

    pub fn fetch_ref(&self) -> String {
        match self {
            Self::PullRequest(pr) => format!("pull/{}/head", pr.number),
            Self::Issue(issue) => issue.default_branch.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommentMention {
    pub author: String,
    pub body: String,
    pub api_url: String,
    pub html_url: String,
    pub target: WorkTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewState {
    Pending,
    Handled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionRequest {
    pub raw_body: String,
    pub cleaned_text: String,
}

impl MentionRequest {
    pub fn parse(body: &str, bot_login: &str) -> Result<Option<Self>> {
        let Some(cleaned_text) = remove_bot_mentions(body, bot_login) else {
            return Ok(None);
        };

        Ok(Some(Self {
            raw_body: body.to_string(),
            cleaned_text: cleaned_text.trim().to_string(),
        }))
    }

    pub fn operator_text(&self) -> Option<String> {
        let text = self.cleaned_text.strip_prefix(OPERATOR_TRIGGER)?;
        if !text.starts_with(char::is_whitespace) {
            return None;
        }

        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }
}

fn remove_bot_mentions(body: &str, bot_login: &str) -> Option<String> {
    let bot_login = bot_login.trim();
    if bot_login.is_empty() {
        return None;
    }

    let needle = format!("@{}", bot_login.to_ascii_lowercase());
    let lower_body = body.to_ascii_lowercase();
    let mut cleaned = String::with_capacity(body.len());
    let mut search_from = 0;
    let mut kept_until = 0;
    let mut found = false;

    while let Some(offset) = lower_body[search_from..].find(&needle) {
        let start = search_from + offset;
        let end = start + needle.len();
        search_from = end;

        if !is_mention_boundary_before(body.as_bytes(), start)
            || !is_mention_boundary_after(body.as_bytes(), end)
        {
            continue;
        }

        cleaned.push_str(&body[kept_until..start]);
        kept_until = end;
        found = true;
    }

    if !found {
        return None;
    }

    cleaned.push_str(&body[kept_until..]);
    Some(cleaned)
}

fn is_mention_boundary_before(bytes: &[u8], start: usize) -> bool {
    start == 0 || !is_github_login_byte(bytes[start - 1])
}

fn is_mention_boundary_after(bytes: &[u8], end: usize) -> bool {
    end == bytes.len() || !is_github_login_byte(bytes[end])
}

fn is_github_login_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexTask {
    pub pr_url: String,
    pub origin: CodexTaskOrigin,
}

impl CodexTask {
    pub fn prompt(&self, templates: &CodexPromptTemplates) -> Result<String> {
        match &self.origin {
            CodexTaskOrigin::Mention {
                mention_url,
                raw_body,
                cleaned_text,
            } => render_template(
                &templates.mention,
                &[
                    ("mention_url", mention_url.as_str()),
                    ("pr_url", self.pr_url.as_str()),
                    ("target_url", self.pr_url.as_str()),
                    ("raw_body", raw_body.as_str()),
                    ("cleaned_text", cleaned_text.as_str()),
                ],
            ),
            CodexTaskOrigin::PullRequestOpened { author } => render_template(
                &templates.pull_request_opened,
                &[
                    ("pr_url", self.pr_url.as_str()),
                    ("author", author.as_str()),
                ],
            ),
            CodexTaskOrigin::OperatorMention {
                mention_url,
                raw_body,
                request_text,
                trigger_author,
                bot_login,
            } => render_template(
                &templates.operator_mention,
                &[
                    ("bot_login", bot_login.as_str()),
                    ("mention_url", mention_url.as_str()),
                    ("operator_trigger", OPERATOR_TRIGGER),
                    ("pr_url", self.pr_url.as_str()),
                    ("target_url", self.pr_url.as_str()),
                    ("raw_body", raw_body.as_str()),
                    ("request_text", request_text.as_str()),
                    ("trigger_author", trigger_author.as_str()),
                ],
            ),
        }
    }

    pub fn trigger_url(&self) -> &str {
        match &self.origin {
            CodexTaskOrigin::Mention { mention_url, .. }
            | CodexTaskOrigin::OperatorMention { mention_url, .. } => mention_url,
            CodexTaskOrigin::PullRequestOpened { .. } => &self.pr_url,
        }
    }

    pub fn task_kind(&self) -> &'static str {
        self.origin.task_kind()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexTaskOrigin {
    Mention {
        mention_url: String,
        raw_body: String,
        cleaned_text: String,
    },
    OperatorMention {
        mention_url: String,
        raw_body: String,
        request_text: String,
        trigger_author: String,
        bot_login: String,
    },
    PullRequestOpened {
        author: String,
    },
}

impl CodexTaskOrigin {
    pub fn task_kind(&self) -> &'static str {
        match self {
            Self::Mention { .. } => "mention",
            Self::OperatorMention { .. } => "operator_mention",
            Self::PullRequestOpened { .. } => "pull_request_opened",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexPromptTemplates {
    pub mention: String,
    pub pull_request_opened: String,
    pub operator_mention: String,
}

fn render_template(template: &str, values: &[(&str, &str)]) -> Result<String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        rendered.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            return Err(anyhow!("Codex prompt template has an unclosed placeholder"));
        };

        let name = after_open[..end].trim();
        let Some((_, value)) = values.iter().find(|(key, _)| *key == name) else {
            return Err(anyhow!("unknown Codex prompt template placeholder: {name}"));
        };
        rendered.push_str(value);
        rest = &after_open[end + 2..];
    }

    rendered.push_str(rest);
    Ok(rendered)
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
    fn filters_to_mention_notifications_with_supported_subjects() {
        let eligible = Notification {
            id: "1".to_string(),
            reason: "mention".to_string(),
            subject_kind: "PullRequest".to_string(),
            subject_url: Some("https://api.github.com/repos/o/r/pulls/1".to_string()),
            latest_comment_url: Some(
                "https://api.github.com/repos/o/r/issues/comments/2".to_string(),
            ),
            unread: true,
            updated_at: "2026-06-08T04:00:00Z".to_string(),
        };

        assert!(eligible.is_mention_candidate());
        assert!(
            Notification {
                reason: "comment".to_string(),
                ..eligible.clone()
            }
            .is_mention_candidate()
        );
        assert!(
            Notification {
                latest_comment_url: None,
                ..eligible.clone()
            }
            .is_mention_candidate()
        );
        assert!(
            Notification {
                subject_kind: "Issue".to_string(),
                subject_url: Some("https://api.github.com/repos/o/r/issues/1".to_string()),
                ..eligible.clone()
            }
            .is_mention_candidate()
        );

        for notification in [
            Notification {
                subject_kind: "Discussion".to_string(),
                ..eligible.clone()
            },
            Notification {
                subject_url: None,
                ..eligible.clone()
            },
        ] {
            assert!(!notification.is_mention_candidate());
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
            MentionRequest::parse("@maid-bot-test not you", "maid-bot")
                .unwrap()
                .is_none()
        );
        assert!(
            MentionRequest::parse("mail@maid-bot.example is not a mention", "maid-bot")
                .unwrap()
                .is_none()
        );
        assert!(
            MentionRequest::parse("@MAID-BOT check this", "maid-bot")
                .unwrap()
                .is_some()
        );

        let repeated = MentionRequest::parse("cc @maid-bot, @MAID-BOT please review", "maid-bot")
            .unwrap()
            .unwrap();

        assert_eq!(repeated.cleaned_text, "cc ,  please review");
    }

    #[test]
    fn parses_operator_text_from_cleaned_mention() {
        let request = MentionRequest::parse(
            "@maid-bot /operate implement the discussed changes",
            "maid-bot",
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            request.operator_text().as_deref(),
            Some("implement the discussed changes")
        );

        let review = MentionRequest::parse("@maid-bot please review", "maid-bot")
            .unwrap()
            .unwrap();

        assert_eq!(review.operator_text(), None);

        let not_operator = MentionRequest::parse("@maid-bot /operatex review", "maid-bot")
            .unwrap()
            .unwrap();

        assert_eq!(not_operator.operator_text(), None);
    }

    #[test]
    fn builds_codex_prompt_with_required_context() {
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot review".to_string(),
                cleaned_text: "review".to_string(),
            },
        };

        let prompt = task.prompt(&test_prompts()).unwrap();
        assert!(prompt.contains("Mention URL:\nhttps://github.com/o/r/pull/1#issuecomment-2"));
        assert!(prompt.contains("Pull request URL:\nhttps://github.com/o/r/pull/1"));
        assert!(prompt.contains("Raw mention body:\n@maid-bot review"));
        assert!(prompt.contains("Cleaned request text:\nreview"));
    }

    #[test]
    fn builds_codex_prompt_for_automatic_pull_request_review() {
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::PullRequestOpened {
                author: "dionysuzx".to_string(),
            },
        };

        let prompt = task.prompt(&test_prompts()).unwrap();
        assert!(prompt.contains("Pull request URL:\nhttps://github.com/o/r/pull/1"));
        assert!(prompt.contains("Opened by:\ndionysuzx"));
        assert!(prompt.contains("Review request:\nplease review"));
    }

    #[test]
    fn renders_configured_codex_prompt_templates() {
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot review".to_string(),
                cleaned_text: "review".to_string(),
            },
        };
        let templates = CodexPromptTemplates {
            mention: "PR={{ pr_url }} REQUEST={{cleaned_text}}".to_string(),
            pull_request_opened: String::new(),
            operator_mention: String::new(),
        };

        assert_eq!(
            task.prompt(&templates).unwrap(),
            "PR=https://github.com/o/r/pull/1 REQUEST=review"
        );
    }

    #[test]
    fn rejects_unknown_codex_prompt_placeholders() {
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::PullRequestOpened {
                author: "dionysuzx".to_string(),
            },
        };
        let templates = CodexPromptTemplates {
            mention: String::new(),
            pull_request_opened: "{{missing}}".to_string(),
            operator_mention: String::new(),
        };

        assert!(task.prompt(&templates).is_err());
    }

    #[test]
    fn builds_operator_prompt_from_template() {
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::OperatorMention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot /operate ship it".to_string(),
                request_text: "ship it".to_string(),
                trigger_author: "dionysuzx".to_string(),
                bot_login: "maid-bot".to_string(),
            },
        };
        let templates = CodexPromptTemplates {
            mention: String::new(),
            pull_request_opened: String::new(),
            operator_mention:
                "{{bot_login}}|{{trigger_author}}|{{mention_url}}|{{pr_url}}|{{raw_body}}|{{request_text}}"
                    .to_string(),
        };

        assert_eq!(
            task.prompt(&templates).unwrap(),
            "maid-bot|dionysuzx|https://github.com/o/r/pull/1#issuecomment-2|https://github.com/o/r/pull/1|@maid-bot /operate ship it|ship it"
        );
    }

    #[test]
    fn reports_task_kind_for_logging() {
        assert_eq!(
            CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot review".to_string(),
                cleaned_text: "review".to_string(),
            }
            .task_kind(),
            "mention"
        );
        assert_eq!(
            CodexTaskOrigin::OperatorMention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot /operate ship it".to_string(),
                request_text: "ship it".to_string(),
                trigger_author: "dionysuzx".to_string(),
                bot_login: "maid-bot".to_string(),
            }
            .task_kind(),
            "operator_mention"
        );
        assert_eq!(
            CodexTaskOrigin::PullRequestOpened {
                author: "dionysuzx".to_string(),
            }
            .task_kind(),
            "pull_request_opened"
        );
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

    fn test_prompts() -> CodexPromptTemplates {
        CodexPromptTemplates {
            mention: "\
Mention URL:
{{mention_url}}

Pull request URL:
{{pr_url}}

Raw mention body:
{{raw_body}}

Cleaned request text:
{{cleaned_text}}
"
            .to_string(),
            pull_request_opened: "\
Pull request URL:
{{pr_url}}

Opened by:
{{author}}

Review request:
please review
"
            .to_string(),
            operator_mention: "\
Bot:
{{bot_login}}

Trigger author:
{{trigger_author}}

Mention URL:
{{mention_url}}

Pull request URL:
{{pr_url}}

Raw mention body:
{{raw_body}}

Operator request:
{{request_text}}
"
            .to_string(),
        }
    }
}
