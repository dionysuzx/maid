use crate::domain::{CodexTask, CommentMention, MentionRequest, Notification, PullRequest};
use anyhow::Result;
use async_trait::async_trait;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tracing::{error, info};

#[async_trait]
pub trait GithubClient: Send + Sync {
    async fn notifications(&self) -> Result<Vec<Notification>>;
    async fn mention_for(&self, notification: &Notification) -> Result<Option<CommentMention>>;
    async fn post_pr_comment(&self, pr: &PullRequest, body: &str) -> Result<()>;
    async fn mention_has_handled_marker(
        &self,
        mention: &CommentMention,
        bot_login: &str,
    ) -> Result<bool>;
    async fn mark_mention_started(&self, mention: &CommentMention) -> Result<()>;
    async fn mark_mention_handled(&self, mention: &CommentMention) -> Result<()>;
    async fn mark_notification_handled(&self, notification: &Notification) -> Result<()>;
}

#[async_trait]
pub trait RepoPreparer: Send + Sync {
    async fn prepare(&self, pr: &PullRequest) -> Result<PathBuf>;
}

#[async_trait]
pub trait CodexRunner: Send + Sync {
    async fn run(&self, checkout: &Path, task: &CodexTask) -> Result<CodexRun>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexRun {
    pub response: String,
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub struct Maid<G, R, C> {
    github: G,
    repos: R,
    codex: C,
    bot_login: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PollReport {
    pub seen: usize,
    pub skipped: usize,
    pub responded: usize,
    pub failed: usize,
}

impl<G, R, C> Maid<G, R, C>
where
    G: GithubClient,
    R: RepoPreparer,
    C: CodexRunner,
{
    pub fn new(github: G, repos: R, codex: C, bot_login: impl Into<String>) -> Self {
        Self {
            github,
            repos,
            codex,
            bot_login: bot_login.into(),
        }
    }

    pub async fn run_once(&self) -> Result<PollReport> {
        let notifications = self.github.notifications().await?;
        let mut report = PollReport {
            seen: notifications.len(),
            ..PollReport::default()
        };
        let mut seen_this_poll = HashSet::new();

        for notification in notifications {
            let work_key = work_key(&notification);
            if !seen_this_poll.insert(work_key) {
                report.skipped += 1;
                continue;
            }

            match self.handle_notification(&notification).await {
                Ok(HandleOutcome::Responded) => report.responded += 1,
                Ok(HandleOutcome::Skipped) => report.skipped += 1,
                Err(err) => {
                    report.failed += 1;
                    error!(
                        notification_id = notification.id,
                        error = ?err,
                        "failed to handle notification"
                    );
                }
            }
        }

        Ok(report)
    }

    async fn handle_notification(&self, notification: &Notification) -> Result<HandleOutcome> {
        if !notification.is_pr_mention_candidate() {
            return Ok(HandleOutcome::Skipped);
        }

        let Some(mention) = self.github.mention_for(notification).await? else {
            return Ok(HandleOutcome::Skipped);
        };

        if mention.author.eq_ignore_ascii_case(&self.bot_login) {
            return Ok(HandleOutcome::Skipped);
        }

        let Some(request) = MentionRequest::parse(&mention.body, &self.bot_login)? else {
            return Ok(HandleOutcome::Skipped);
        };

        if self
            .github
            .mention_has_handled_marker(&mention, &self.bot_login)
            .await?
        {
            info!(
                notification_id = notification.id,
                mention = %mention.html_url,
                "skipping already handled mention"
            );
            self.github.mark_notification_handled(notification).await?;
            return Ok(HandleOutcome::Skipped);
        }

        self.github.mark_mention_started(&mention).await?;

        let checkout = self.repos.prepare(&mention.pr).await?;
        let task = CodexTask {
            mention_url: mention.html_url.clone(),
            pr_url: mention.pr.html_url.clone(),
            raw_body: request.raw_body,
            cleaned_text: request.cleaned_text,
        };
        let codex_run = self.codex.run(&checkout, &task).await?;

        self.github
            .post_pr_comment(&mention.pr, &codex_run.response)
            .await?;
        if let Err(err) = self.github.mark_mention_handled(&mention).await {
            error!(
                notification_id = notification.id,
                pr = %mention.pr.html_url,
                mention = %mention.html_url,
                error = ?err,
                "failed to mark mention handled after posting response"
            );
        }
        self.github.mark_notification_handled(notification).await?;

        if let Some(session_id) = &codex_run.session_id {
            let resume_command =
                format!("codex resume --include-non-interactive --all {session_id}");
            info!(
                notification_id = notification.id,
                pr = %mention.pr.html_url,
                mention = %mention.html_url,
                codex_session_id = %session_id,
                codex_resume = %resume_command,
                "responded to mention"
            );
        } else {
            info!(
                notification_id = notification.id,
                pr = %mention.pr.html_url,
                mention = %mention.html_url,
                "responded to mention"
            );
        }
        Ok(HandleOutcome::Responded)
    }
}

fn work_key(notification: &Notification) -> String {
    notification
        .latest_comment_url
        .clone()
        .unwrap_or_else(|| notification.id.clone())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandleOutcome {
    Responded,
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use std::sync::{Arc, Mutex as StdMutex};

    type FakeMentionResult = Option<Result<Option<CommentMention>, String>>;

    #[derive(Clone, Default)]
    struct FakeGithub {
        notifications: Arc<StdMutex<Vec<Notification>>>,
        mention: Arc<StdMutex<FakeMentionResult>>,
        posts: Arc<StdMutex<Vec<String>>>,
        marks: Arc<StdMutex<Vec<String>>>,
        events: Arc<StdMutex<Vec<String>>>,
        post_error: Arc<StdMutex<Option<String>>>,
        handled_error: Arc<StdMutex<Option<String>>>,
        started_mentions: Arc<StdMutex<Vec<String>>>,
        handled_mentions: Arc<StdMutex<HashSet<String>>>,
    }

    #[async_trait]
    impl GithubClient for FakeGithub {
        async fn notifications(&self) -> Result<Vec<Notification>> {
            Ok(self.notifications.lock().unwrap().clone())
        }

        async fn mention_for(
            &self,
            _notification: &Notification,
        ) -> Result<Option<CommentMention>> {
            match self
                .mention
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Ok(None))
            {
                Ok(value) => Ok(value),
                Err(message) => Err(anyhow!(message)),
            }
        }

        async fn post_pr_comment(&self, _pr: &PullRequest, body: &str) -> Result<()> {
            self.events.lock().unwrap().push("post".to_string());
            if let Some(message) = self.post_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.posts.lock().unwrap().push(body.to_string());
            Ok(())
        }

        async fn mention_has_handled_marker(
            &self,
            mention: &CommentMention,
            _bot_login: &str,
        ) -> Result<bool> {
            Ok(self
                .handled_mentions
                .lock()
                .unwrap()
                .contains(&mention.api_url))
        }

        async fn mark_mention_started(&self, mention: &CommentMention) -> Result<()> {
            self.events.lock().unwrap().push("start".to_string());
            self.started_mentions
                .lock()
                .unwrap()
                .push(mention.api_url.clone());
            Ok(())
        }

        async fn mark_mention_handled(&self, mention: &CommentMention) -> Result<()> {
            self.events.lock().unwrap().push("handled".to_string());
            if let Some(message) = self.handled_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.handled_mentions
                .lock()
                .unwrap()
                .insert(mention.api_url.clone());
            Ok(())
        }

        async fn mark_notification_handled(&self, notification: &Notification) -> Result<()> {
            self.events.lock().unwrap().push("mark".to_string());
            self.marks.lock().unwrap().push(notification.id.clone());
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeRepos {
        checkout: PathBuf,
        calls: Arc<StdMutex<Vec<String>>>,
        error: Arc<StdMutex<Option<String>>>,
    }

    #[async_trait]
    impl RepoPreparer for FakeRepos {
        async fn prepare(&self, pr: &PullRequest) -> Result<PathBuf> {
            self.calls.lock().unwrap().push(pr.repo_key());
            if let Some(message) = self.error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            Ok(self.checkout.clone())
        }
    }

    #[derive(Clone, Default)]
    struct FakeCodex {
        calls: Arc<StdMutex<Vec<(PathBuf, CodexTask)>>>,
        error: Arc<StdMutex<Option<String>>>,
    }

    #[async_trait]
    impl CodexRunner for FakeCodex {
        async fn run(&self, checkout: &Path, task: &CodexTask) -> Result<CodexRun> {
            self.calls
                .lock()
                .unwrap()
                .push((checkout.to_path_buf(), task.clone()));
            if let Some(message) = self.error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            Ok(CodexRun {
                response: "codex response".to_string(),
                session_id: Some("019e64fd-8369-7453-9cdc-4b14b388f618".to_string()),
            })
        }
    }

    fn notification(id: &str) -> Notification {
        notification_with_comment(id, "2")
    }

    fn notification_with_comment(id: &str, comment_id: &str) -> Notification {
        Notification {
            id: id.to_string(),
            reason: "mention".to_string(),
            subject_kind: "PullRequest".to_string(),
            subject_url: Some("https://api.github.com/repos/o/r/pulls/1".to_string()),
            latest_comment_url: Some(format!(
                "https://api.github.com/repos/o/r/issues/comments/{comment_id}"
            )),
        }
    }

    fn pr() -> PullRequest {
        PullRequest {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 1,
            api_url: "https://api.github.com/repos/o/r/pulls/1".to_string(),
            html_url: "https://github.com/o/r/pull/1".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
        }
    }

    fn mention(author: &str, body: &str) -> CommentMention {
        mention_with_comment(author, body, "2")
    }

    fn mention_with_comment(author: &str, body: &str, comment_id: &str) -> CommentMention {
        CommentMention {
            author: author.to_string(),
            body: body.to_string(),
            api_url: format!("https://api.github.com/repos/o/r/issues/comments/{comment_id}"),
            html_url: format!("https://github.com/o/r/pull/1#issuecomment-{comment_id}"),
            pr: pr(),
        }
    }

    fn maid(
        github: FakeGithub,
        repos: FakeRepos,
        codex: FakeCodex,
    ) -> Maid<FakeGithub, FakeRepos, FakeCodex> {
        Maid::new(github, repos, codex, "mayushii-nyan")
    }

    #[tokio::test]
    async fn responds_then_marks_notification_handled() {
        let checkout = PathBuf::from("/tmp/maid-test-checkout");
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention(
            "dionysuzx",
            "@mayushii-nyan please review this PR",
        ))));
        let repos = FakeRepos {
            checkout: checkout.clone(),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), repos.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert_eq!(
            *github.events.lock().unwrap(),
            vec!["start", "post", "handled", "mark"]
        );
        assert_eq!(
            *github.started_mentions.lock().unwrap(),
            vec!["https://api.github.com/repos/o/r/issues/comments/2"]
        );
        assert!(
            github
                .handled_mentions
                .lock()
                .unwrap()
                .contains("https://api.github.com/repos/o/r/issues/comments/2")
        );
        assert_eq!(*repos.calls.lock().unwrap(), vec!["o/r"]);
        let calls = codex.calls.lock().unwrap();
        assert_eq!(calls[0].0, checkout);
        assert_eq!(
            calls[0].1.mention_url,
            "https://github.com/o/r/pull/1#issuecomment-2"
        );
        assert_eq!(calls[0].1.pr_url, "https://github.com/o/r/pull/1");
        assert_eq!(calls[0].1.raw_body, "@mayushii-nyan please review this PR");
        assert_eq!(calls[0].1.cleaned_text, "please review this PR");
    }

    #[tokio::test]
    async fn ignores_self_authored_mentions() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("mayushii-nyan", "@mayushii-nyan review"))));
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), repos.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_duplicate_latest_comment_after_success() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1"), notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("dionysuzx", "@mayushii-nyan review"))));
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), repos, codex);

        let first_report = maid.run_once().await.unwrap();
        let second_report = maid.run_once().await.unwrap();

        assert_eq!(first_report.responded, 1);
        assert_eq!(first_report.skipped, 1);
        assert_eq!(second_report.responded, 0);
        assert_eq!(second_report.skipped, 2);
        assert_eq!(github.posts.lock().unwrap().len(), 1);
        assert_eq!(github.marks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn responds_to_new_latest_comment_on_same_notification_thread() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification_with_comment("n1", "2")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention_with_comment(
            "dionysuzx",
            "@mayushii-nyan first",
            "2",
        ))));
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), repos, codex);

        let first_report = maid.run_once().await.unwrap();

        *github.notifications.lock().unwrap() = vec![notification_with_comment("n1", "3")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention_with_comment(
            "dionysuzx",
            "@mayushii-nyan second",
            "3",
        ))));

        let second_report = maid.run_once().await.unwrap();

        assert_eq!(first_report.responded, 1);
        assert_eq!(second_report.responded, 1);
        assert_eq!(github.posts.lock().unwrap().len(), 2);
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1", "n1"]);
    }

    #[tokio::test]
    async fn skips_mentions_with_durable_handled_marker_after_restart() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("dionysuzx", "@mayushii-nyan review"))));
        github
            .handled_mentions
            .lock()
            .unwrap()
            .insert("https://api.github.com/repos/o/r/issues/comments/2".to_string());
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), repos.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repo_prep_failure_does_not_post_or_mark() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("dionysuzx", "@mayushii-nyan review"))));
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(Some("clone failed".to_string()))),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), repos, codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.failed, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn codex_failure_does_not_post_or_mark() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("dionysuzx", "@mayushii-nyan review"))));
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        *codex.error.lock().unwrap() = Some("codex failed".to_string());

        let report = maid(github.clone(), repos, codex).run_once().await.unwrap();

        assert_eq!(report.failed, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn post_failure_does_not_mark_notification_handled() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("dionysuzx", "@mayushii-nyan review"))));
        *github.post_error.lock().unwrap() = Some("post failed".to_string());
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), repos, codex).run_once().await.unwrap();

        assert_eq!(report.failed, 1);
        assert!(github.marks.lock().unwrap().is_empty());
        assert_eq!(*github.events.lock().unwrap(), vec!["start", "post"]);
    }

    #[tokio::test]
    async fn handled_marker_failure_after_post_still_marks_notification_handled() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("dionysuzx", "@mayushii-nyan review"))));
        *github.handled_error.lock().unwrap() = Some("reaction failed".to_string());
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), repos, codex).run_once().await.unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(github.handled_mentions.lock().unwrap().is_empty());
        assert_eq!(
            *github.events.lock().unwrap(),
            vec!["start", "post", "handled", "mark"]
        );
    }
}
