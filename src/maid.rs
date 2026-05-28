use crate::domain::{
    CodexTask, CodexTaskOrigin, CommentMention, Issue, MentionRequest, Notification, PullRequest,
    RepoSlug,
};
use crate::task_limit::{NoTaskLimit, TaskStartDecision, TaskStartRecorder};
use anyhow::Result;
use async_trait::async_trait;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tracing::{debug, error, info};

#[async_trait]
pub trait GithubClient: Send + Sync {
    async fn notifications(&self) -> Result<Vec<Notification>>;
    async fn mention_for(&self, notification: &Notification) -> Result<Option<CommentMention>>;
    async fn open_pull_requests(&self, repo: &RepoSlug) -> Result<Vec<PullRequest>>;
    async fn recent_labeled_issues(
        &self,
        repo: &RepoSlug,
        label: &str,
        since: SystemTime,
    ) -> Result<Vec<Issue>>;
    async fn post_pr_comment(&self, pr: &PullRequest, body: &str) -> Result<()>;
    async fn post_issue_comment(&self, issue: &Issue, body: &str) -> Result<()>;
    async fn open_pull_request_for_branch(
        &self,
        issue: &Issue,
        branch: &str,
    ) -> Result<Option<PullRequest>>;
    async fn create_pull_request(
        &self,
        issue: &Issue,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest>;
    async fn mention_state(&self, mention: &CommentMention, bot_login: &str)
    -> Result<ReviewState>;
    async fn mark_mention_started(&self, mention: &CommentMention) -> Result<()>;
    async fn mark_mention_handled(&self, mention: &CommentMention) -> Result<()>;
    async fn pr_state(&self, pr: &PullRequest, bot_login: &str) -> Result<ReviewState>;
    async fn mark_pr_started(&self, pr: &PullRequest) -> Result<()>;
    async fn mark_pr_handled(&self, pr: &PullRequest) -> Result<()>;
    async fn issue_state(&self, issue: &Issue, bot_login: &str) -> Result<ReviewState>;
    async fn mark_issue_started(&self, issue: &Issue) -> Result<()>;
    async fn mark_issue_handled(&self, issue: &Issue) -> Result<()>;
    async fn mark_notification_handled(&self, notification: &Notification) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewState {
    Pending,
    Handled,
}

#[async_trait]
pub trait RepoWorkspace: Send + Sync {
    async fn prepare_pr_review(&self, pr: &PullRequest) -> Result<PathBuf>;
    async fn prepare_issue_branch(&self, issue: &Issue, branch: &str) -> Result<PathBuf>;
    async fn has_changes(&self, checkout: &Path) -> Result<bool>;
    async fn commit_all(&self, checkout: &Path, message: &str) -> Result<()>;
    async fn push_branch(&self, checkout: &Path, branch: &str) -> Result<()>;
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

impl CodexRun {
    pub fn resume_command(&self) -> Option<(&str, String)> {
        self.session_id.as_deref().map(|session_id| {
            (
                session_id,
                format!("codex resume --include-non-interactive --all {session_id}"),
            )
        })
    }
}

#[derive(Clone)]
pub struct Maid<G, R, C> {
    github: G,
    implementation_github: G,
    repos: R,
    codex: C,
    bot_login: String,
    master_accounts: HashSet<String>,
    auto_review_accounts: HashSet<String>,
    auto_review_repos: Vec<RepoSlug>,
    auto_implement_accounts: HashSet<String>,
    auto_implement_repos: Vec<RepoSlug>,
    auto_implement_label: String,
    auto_implement_window: Duration,
    task_starts: Arc<dyn TaskStartRecorder>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaidSettings {
    pub bot_login: String,
    pub master_accounts: Vec<String>,
    pub auto_review_accounts: Vec<String>,
    pub auto_review_repos: Vec<RepoSlug>,
    pub auto_implement_accounts: Vec<String>,
    pub auto_implement_repos: Vec<RepoSlug>,
    pub auto_implement_label: String,
    pub auto_implement_window: Duration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PollReport {
    pub seen: usize,
    pub skipped: usize,
    pub skip_breakdown: SkipBreakdown,
    pub responded: usize,
    pub failed: usize,
}

impl PollReport {
    fn record_skip(&mut self, reason: SkipReason) {
        self.skipped += 1;
        self.skip_breakdown.record(reason);
    }

    pub fn has_actionable_result(&self) -> bool {
        self.responded > 0 || self.failed > 0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkipBreakdown {
    pub duplicate_notification: usize,
    pub non_pr_notification: usize,
    pub missing_mention: usize,
    pub self_authored_mention: usize,
    pub non_master_mention: usize,
    pub no_bot_request: usize,
    pub already_handled_mention: usize,
    pub self_authored_pr: usize,
    pub auto_review_disabled: usize,
    pub already_handled_pr: usize,
    pub self_authored_issue: usize,
    pub auto_implement_disabled: usize,
    pub already_handled_issue: usize,
    pub existing_issue_pr: usize,
    pub issue_without_changes: usize,
    pub task_limit_reached: usize,
}

impl SkipBreakdown {
    fn record(&mut self, reason: SkipReason) {
        match reason {
            SkipReason::DuplicateNotification => self.duplicate_notification += 1,
            SkipReason::NonPrNotification => self.non_pr_notification += 1,
            SkipReason::MissingMention => self.missing_mention += 1,
            SkipReason::SelfAuthoredMention => self.self_authored_mention += 1,
            SkipReason::NonMasterMention => self.non_master_mention += 1,
            SkipReason::NoBotRequest => self.no_bot_request += 1,
            SkipReason::AlreadyHandledMention => self.already_handled_mention += 1,
            SkipReason::SelfAuthoredPr => self.self_authored_pr += 1,
            SkipReason::AutoReviewDisabled => self.auto_review_disabled += 1,
            SkipReason::AlreadyHandledPr => self.already_handled_pr += 1,
            SkipReason::SelfAuthoredIssue => self.self_authored_issue += 1,
            SkipReason::AutoImplementDisabled => self.auto_implement_disabled += 1,
            SkipReason::AlreadyHandledIssue => self.already_handled_issue += 1,
            SkipReason::ExistingIssuePr => self.existing_issue_pr += 1,
            SkipReason::IssueWithoutChanges => self.issue_without_changes += 1,
            SkipReason::TaskLimitReached => self.task_limit_reached += 1,
        }
    }
}

impl<G, R, C> Maid<G, R, C>
where
    G: GithubClient,
    R: RepoWorkspace,
    C: CodexRunner,
{
    pub fn new(
        github: G,
        implementation_github: G,
        repos: R,
        codex: C,
        settings: MaidSettings,
    ) -> Self {
        Self {
            github,
            implementation_github,
            repos,
            codex,
            bot_login: settings.bot_login,
            master_accounts: normalized_logins(settings.master_accounts),
            auto_review_accounts: normalized_logins(settings.auto_review_accounts),
            auto_review_repos: settings.auto_review_repos,
            auto_implement_accounts: normalized_logins(settings.auto_implement_accounts),
            auto_implement_repos: settings.auto_implement_repos,
            auto_implement_label: settings.auto_implement_label,
            auto_implement_window: settings.auto_implement_window,
            task_starts: Arc::new(NoTaskLimit),
        }
    }

    pub fn with_task_start_recorder(
        mut self,
        task_starts: impl TaskStartRecorder + 'static,
    ) -> Self {
        self.task_starts = Arc::new(task_starts);
        self
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
                report.record_skip(SkipReason::DuplicateNotification);
                debug!(
                    notification_id = notification.id,
                    "skipped duplicate notification for latest comment"
                );
                continue;
            }

            match self.handle_notification(&notification).await {
                Ok(HandleOutcome::Responded) => report.responded += 1,
                Ok(HandleOutcome::Skipped(reason)) => report.record_skip(reason),
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

        for repo in &self.auto_review_repos {
            let pull_requests = self.github.open_pull_requests(repo).await?;
            report.seen += pull_requests.len();

            for pr in pull_requests {
                match self.handle_auto_review_pr(&pr).await {
                    Ok(HandleOutcome::Responded) => report.responded += 1,
                    Ok(HandleOutcome::Skipped(reason)) => report.record_skip(reason),
                    Err(err) => {
                        report.failed += 1;
                        error!(
                            pr = %pr.html_url,
                            error = ?err,
                            "failed to handle auto-review pull request"
                        );
                    }
                }
            }
        }

        let issue_since = SystemTime::now()
            .checked_sub(self.auto_implement_window)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        for repo in &self.auto_implement_repos {
            let issues = self
                .github
                .recent_labeled_issues(repo, &self.auto_implement_label, issue_since)
                .await?;
            report.seen += issues.len();

            for issue in issues {
                match self.handle_auto_implement_issue(&issue).await {
                    Ok(HandleOutcome::Responded) => report.responded += 1,
                    Ok(HandleOutcome::Skipped(reason)) => report.record_skip(reason),
                    Err(err) => {
                        report.failed += 1;
                        error!(
                            issue = %issue.html_url,
                            error = ?err,
                            "failed to handle auto-implement issue"
                        );
                    }
                }
            }
        }

        Ok(report)
    }

    async fn handle_notification(&self, notification: &Notification) -> Result<HandleOutcome> {
        if notification.is_pr_mention_candidate() {
            return self.handle_mention(notification).await;
        }

        self.github.mark_notification_handled(notification).await?;
        debug!(
            notification_id = notification.id,
            "skipped non-PR notification"
        );
        Ok(HandleOutcome::Skipped(SkipReason::NonPrNotification))
    }

    async fn handle_mention(&self, notification: &Notification) -> Result<HandleOutcome> {
        let Some(mention) = self.github.mention_for(notification).await? else {
            debug!(
                notification_id = notification.id,
                "skipped notification without a resolvable mention"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::MissingMention));
        };

        if mention.author.eq_ignore_ascii_case(&self.bot_login) {
            debug!(
                notification_id = notification.id,
                mention = %mention.html_url,
                "skipped mention authored by bot"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::SelfAuthoredMention));
        }

        if !self
            .master_accounts
            .contains(&mention.author.to_ascii_lowercase())
        {
            debug!(
                notification_id = notification.id,
                mention = %mention.html_url,
                author = %mention.author,
                "skipping mention from non-master account"
            );
            self.github.mark_notification_handled(notification).await?;
            return Ok(HandleOutcome::Skipped(SkipReason::NonMasterMention));
        }

        let Some(request) = MentionRequest::parse(&mention.body, &self.bot_login)? else {
            debug!(
                notification_id = notification.id,
                mention = %mention.html_url,
                "skipped mention without bot request"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::NoBotRequest));
        };

        match self.github.mention_state(&mention, &self.bot_login).await? {
            ReviewState::Pending => {}
            ReviewState::Handled => {
                debug!(
                    notification_id = notification.id,
                    mention = %mention.html_url,
                    "skipping already handled mention"
                );
                self.github.mark_notification_handled(notification).await?;
                return Ok(HandleOutcome::Skipped(SkipReason::AlreadyHandledMention));
            }
        }

        if !self.can_start_task()? {
            info!(
                notification_id = notification.id,
                pr = %mention.pr.html_url,
                mention = %mention.html_url,
                "skipping mention because the 24-hour task limit is reached"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::TaskLimitReached));
        }

        self.github.mark_mention_started(&mention).await?;
        info!(
            notification_id = notification.id,
            pr = %mention.pr.html_url,
            mention = %mention.html_url,
            "started handling mention"
        );

        let checkout = self.repos.prepare_pr_review(&mention.pr).await?;
        let task = CodexTask {
            subject_url: mention.pr.html_url.clone(),
            origin: CodexTaskOrigin::Mention {
                mention_url: mention.html_url.clone(),
                raw_body: request.raw_body,
                cleaned_text: request.cleaned_text,
            },
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

        if let Some((session_id, resume_command)) = codex_run.resume_command() {
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

    async fn handle_auto_review_pr(&self, pr: &PullRequest) -> Result<HandleOutcome> {
        if pr.author.eq_ignore_ascii_case(&self.bot_login) {
            debug!(pr = %pr.html_url, "skipped pull request authored by bot");
            return Ok(HandleOutcome::Skipped(SkipReason::SelfAuthoredPr));
        }

        if !self
            .auto_review_accounts
            .contains(&pr.author.to_ascii_lowercase())
        {
            debug!(
                pr = %pr.html_url,
                author = %pr.author,
                "skipping pull request from account without auto review"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::AutoReviewDisabled));
        }

        match self.github.pr_state(pr, &self.bot_login).await? {
            ReviewState::Pending => {}
            ReviewState::Handled => {
                debug!(
                    pr = %pr.html_url,
                    "skipping already handled auto-review pull request"
                );
                return Ok(HandleOutcome::Skipped(SkipReason::AlreadyHandledPr));
            }
        }

        if !self.can_start_task()? {
            info!(
                pr = %pr.html_url,
                author = %pr.author,
                "skipping auto-review pull request because the 24-hour task limit is reached"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::TaskLimitReached));
        }

        self.github.mark_pr_started(pr).await?;
        info!(
            pr = %pr.html_url,
            author = %pr.author,
            "started handling auto-review pull request"
        );

        let checkout = self.repos.prepare_pr_review(pr).await?;
        let task = CodexTask {
            subject_url: pr.html_url.clone(),
            origin: CodexTaskOrigin::PullRequestOpened {
                author: pr.author.clone(),
            },
        };
        let codex_run = self.codex.run(&checkout, &task).await?;

        self.github.post_pr_comment(pr, &codex_run.response).await?;
        if let Err(err) = self.github.mark_pr_handled(pr).await {
            error!(
                pr = %pr.html_url,
                error = ?err,
                "failed to mark auto-review pull request handled after posting response"
            );
        }

        if let Some((session_id, resume_command)) = codex_run.resume_command() {
            info!(
                pr = %pr.html_url,
                author = %pr.author,
                codex_session_id = %session_id,
                codex_resume = %resume_command,
                "responded to auto-review pull request"
            );
        } else {
            info!(
                pr = %pr.html_url,
                author = %pr.author,
                "responded to auto-review pull request"
            );
        }
        Ok(HandleOutcome::Responded)
    }

    async fn handle_auto_implement_issue(&self, issue: &Issue) -> Result<HandleOutcome> {
        if issue.author.eq_ignore_ascii_case(&self.bot_login) {
            debug!(issue = %issue.html_url, "skipped issue authored by bot");
            return Ok(HandleOutcome::Skipped(SkipReason::SelfAuthoredIssue));
        }

        if !self
            .auto_implement_accounts
            .contains(&issue.author.to_ascii_lowercase())
        {
            debug!(
                issue = %issue.html_url,
                author = %issue.author,
                "skipping issue from account without auto implementation"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::AutoImplementDisabled));
        }

        match self.github.issue_state(issue, &self.bot_login).await? {
            ReviewState::Pending => {}
            ReviewState::Handled => {
                debug!(issue = %issue.html_url, "skipping already handled issue");
                return Ok(HandleOutcome::Skipped(SkipReason::AlreadyHandledIssue));
            }
        }

        let branch = issue.implementation_branch();
        if let Some(existing_pr) = self
            .github
            .open_pull_request_for_branch(issue, &branch)
            .await?
        {
            info!(
                issue = %issue.html_url,
                pr = %existing_pr.html_url,
                branch = %branch,
                "skipping issue because a Maid pull request already exists"
            );
            self.github.mark_issue_handled(issue).await?;
            return Ok(HandleOutcome::Skipped(SkipReason::ExistingIssuePr));
        }

        if !self.can_start_task()? {
            info!(
                issue = %issue.html_url,
                author = %issue.author,
                "skipping issue implementation because the 24-hour task limit is reached"
            );
            return Ok(HandleOutcome::Skipped(SkipReason::TaskLimitReached));
        }

        self.github.mark_issue_started(issue).await?;
        info!(
            issue = %issue.html_url,
            author = %issue.author,
            branch = %branch,
            "started handling auto-implement issue"
        );

        let checkout = self.repos.prepare_issue_branch(issue, &branch).await?;
        let task = CodexTask {
            subject_url: issue.html_url.clone(),
            origin: CodexTaskOrigin::IssueImplementation {
                title: issue.title.clone(),
                body: issue.body.clone(),
                branch: branch.clone(),
            },
        };
        let codex_run = self.codex.run(&checkout, &task).await?;

        if !self.repos.has_changes(&checkout).await? {
            self.github
                .post_issue_comment(issue, &issue.no_changes_comment())
                .await?;
            if let Err(err) = self.github.mark_issue_handled(issue).await {
                error!(
                    issue = %issue.html_url,
                    error = ?err,
                    "failed to mark issue handled after no-change response"
                );
            }
            return Ok(HandleOutcome::Skipped(SkipReason::IssueWithoutChanges));
        }

        self.repos
            .commit_all(&checkout, &issue.pull_request_title())
            .await?;
        self.repos.push_branch(&checkout, &branch).await?;
        let pr = self
            .implementation_github
            .create_pull_request(
                issue,
                &branch,
                &issue.pull_request_title(),
                &issue.pull_request_body(&codex_run.response),
            )
            .await?;
        self.github
            .post_issue_comment(issue, &format!("Opened pull request: {}", pr.html_url))
            .await?;
        if let Err(err) = self.github.mark_issue_handled(issue).await {
            error!(
                issue = %issue.html_url,
                pr = %pr.html_url,
                error = ?err,
                "failed to mark issue handled after opening pull request"
            );
        }

        if let Some((session_id, resume_command)) = codex_run.resume_command() {
            info!(
                issue = %issue.html_url,
                pr = %pr.html_url,
                branch = %branch,
                codex_session_id = %session_id,
                codex_resume = %resume_command,
                "opened pull request for issue"
            );
        } else {
            info!(
                issue = %issue.html_url,
                pr = %pr.html_url,
                branch = %branch,
                "opened pull request for issue"
            );
        }
        Ok(HandleOutcome::Responded)
    }

    fn can_start_task(&self) -> Result<bool> {
        match self.task_starts.try_record_started()? {
            TaskStartDecision::Recorded => Ok(true),
            TaskStartDecision::AtLimit { limit, window } => {
                info!(
                    limit,
                    window_seconds = window.as_secs(),
                    "task start limit reached"
                );
                Ok(false)
            }
        }
    }
}

fn work_key(notification: &Notification) -> String {
    notification
        .latest_comment_url
        .as_deref()
        .unwrap_or(&notification.id)
        .to_string()
}

fn normalized_logins(logins: impl IntoIterator<Item = impl Into<String>>) -> HashSet<String> {
    logins
        .into_iter()
        .map(|login| login.into().trim().to_ascii_lowercase())
        .filter(|login| !login.is_empty())
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandleOutcome {
    Responded,
    Skipped(SkipReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SkipReason {
    DuplicateNotification,
    NonPrNotification,
    MissingMention,
    SelfAuthoredMention,
    NonMasterMention,
    NoBotRequest,
    AlreadyHandledMention,
    SelfAuthoredPr,
    AutoReviewDisabled,
    AlreadyHandledPr,
    SelfAuthoredIssue,
    AutoImplementDisabled,
    AlreadyHandledIssue,
    ExistingIssuePr,
    IssueWithoutChanges,
    TaskLimitReached,
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
        pull_requests: Arc<StdMutex<Vec<PullRequest>>>,
        issues: Arc<StdMutex<Vec<Issue>>>,
        existing_issue_pr: Arc<StdMutex<Option<PullRequest>>>,
        created_pull_requests: Arc<StdMutex<Vec<String>>>,
        posts: Arc<StdMutex<Vec<String>>>,
        marks: Arc<StdMutex<Vec<String>>>,
        events: Arc<StdMutex<Vec<String>>>,
        post_error: Arc<StdMutex<Option<String>>>,
        handled_error: Arc<StdMutex<Option<String>>>,
        started_mentions: Arc<StdMutex<Vec<String>>>,
        handled_mentions: Arc<StdMutex<HashSet<String>>>,
        started_prs: Arc<StdMutex<Vec<String>>>,
        handled_prs: Arc<StdMutex<HashSet<String>>>,
        started_issues: Arc<StdMutex<Vec<String>>>,
        handled_issues: Arc<StdMutex<HashSet<String>>>,
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

        async fn open_pull_requests(&self, _repo: &RepoSlug) -> Result<Vec<PullRequest>> {
            Ok(self.pull_requests.lock().unwrap().clone())
        }

        async fn recent_labeled_issues(
            &self,
            _repo: &RepoSlug,
            _label: &str,
            _since: SystemTime,
        ) -> Result<Vec<Issue>> {
            Ok(self.issues.lock().unwrap().clone())
        }

        async fn post_pr_comment(&self, _pr: &PullRequest, body: &str) -> Result<()> {
            self.events.lock().unwrap().push("post".to_string());
            if let Some(message) = self.post_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.posts.lock().unwrap().push(body.to_string());
            Ok(())
        }

        async fn post_issue_comment(&self, _issue: &Issue, body: &str) -> Result<()> {
            self.events.lock().unwrap().push("post_issue".to_string());
            if let Some(message) = self.post_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.posts.lock().unwrap().push(body.to_string());
            Ok(())
        }

        async fn open_pull_request_for_branch(
            &self,
            _issue: &Issue,
            _branch: &str,
        ) -> Result<Option<PullRequest>> {
            Ok(self.existing_issue_pr.lock().unwrap().clone())
        }

        async fn create_pull_request(
            &self,
            issue: &Issue,
            branch: &str,
            _title: &str,
            _body: &str,
        ) -> Result<PullRequest> {
            self.events.lock().unwrap().push("create_pr".to_string());
            self.created_pull_requests
                .lock()
                .unwrap()
                .push(branch.to_string());
            Ok(PullRequest {
                owner: issue.owner.clone(),
                repo: issue.repo.clone(),
                number: 7,
                author: "maid-bot".to_string(),
                api_url: "https://api.github.com/repos/o/r/pulls/7".to_string(),
                html_url: "https://github.com/o/r/pull/7".to_string(),
                clone_url: issue.clone_url.clone(),
            })
        }

        async fn mention_state(
            &self,
            mention: &CommentMention,
            _bot_login: &str,
        ) -> Result<ReviewState> {
            if self
                .handled_mentions
                .lock()
                .unwrap()
                .contains(&mention.api_url)
            {
                Ok(ReviewState::Handled)
            } else {
                Ok(ReviewState::Pending)
            }
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

        async fn pr_state(&self, pr: &PullRequest, _bot_login: &str) -> Result<ReviewState> {
            if self.handled_prs.lock().unwrap().contains(&pr.html_url) {
                Ok(ReviewState::Handled)
            } else {
                Ok(ReviewState::Pending)
            }
        }

        async fn mark_pr_started(&self, pr: &PullRequest) -> Result<()> {
            self.events.lock().unwrap().push("start_pr".to_string());
            self.started_prs.lock().unwrap().push(pr.html_url.clone());
            Ok(())
        }

        async fn mark_pr_handled(&self, pr: &PullRequest) -> Result<()> {
            self.events.lock().unwrap().push("handled_pr".to_string());
            if let Some(message) = self.handled_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.handled_prs.lock().unwrap().insert(pr.html_url.clone());
            Ok(())
        }

        async fn issue_state(&self, issue: &Issue, _bot_login: &str) -> Result<ReviewState> {
            if self
                .handled_issues
                .lock()
                .unwrap()
                .contains(&issue.html_url)
            {
                Ok(ReviewState::Handled)
            } else {
                Ok(ReviewState::Pending)
            }
        }

        async fn mark_issue_started(&self, issue: &Issue) -> Result<()> {
            self.events.lock().unwrap().push("start_issue".to_string());
            self.started_issues
                .lock()
                .unwrap()
                .push(issue.html_url.clone());
            Ok(())
        }

        async fn mark_issue_handled(&self, issue: &Issue) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push("handled_issue".to_string());
            if let Some(message) = self.handled_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.handled_issues
                .lock()
                .unwrap()
                .insert(issue.html_url.clone());
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
    impl RepoWorkspace for FakeRepos {
        async fn prepare_pr_review(&self, pr: &PullRequest) -> Result<PathBuf> {
            self.calls.lock().unwrap().push(pr.repo_key());
            if let Some(message) = self.error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            Ok(self.checkout.clone())
        }

        async fn prepare_issue_branch(&self, issue: &Issue, branch: &str) -> Result<PathBuf> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:{branch}", issue.repo_key()));
            if let Some(message) = self.error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            Ok(self.checkout.clone())
        }

        async fn has_changes(&self, _checkout: &Path) -> Result<bool> {
            Ok(true)
        }

        async fn commit_all(&self, _checkout: &Path, message: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("commit:{message}"));
            Ok(())
        }

        async fn push_branch(&self, _checkout: &Path, branch: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("push:{branch}"));
            Ok(())
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

    #[derive(Clone)]
    struct FixedTaskStartRecorder {
        decision: TaskStartDecision,
        calls: Arc<StdMutex<usize>>,
    }

    impl TaskStartRecorder for FixedTaskStartRecorder {
        fn try_record_started(&self) -> Result<TaskStartDecision> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.decision)
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

    fn irrelevant_notification(id: &str) -> Notification {
        Notification {
            id: id.to_string(),
            reason: "subscribed".to_string(),
            subject_kind: "Issue".to_string(),
            subject_url: Some("https://api.github.com/repos/o/r/issues/1".to_string()),
            latest_comment_url: None,
        }
    }

    fn pr() -> PullRequest {
        pr_with_author("dionysuzx")
    }

    fn pr_with_author(author: &str) -> PullRequest {
        PullRequest {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 1,
            author: author.to_string(),
            api_url: "https://api.github.com/repos/o/r/pulls/1".to_string(),
            html_url: "https://github.com/o/r/pull/1".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
        }
    }

    fn issue() -> Issue {
        issue_with_author("dionysuzx")
    }

    fn issue_with_author(author: &str) -> Issue {
        Issue {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 3,
            author: author.to_string(),
            title: "Add the thing".to_string(),
            body: "Please add the missing thing.".to_string(),
            api_url: "https://api.github.com/repos/o/r/issues/3".to_string(),
            html_url: "https://github.com/o/r/issues/3".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
            ssh_url: "git@github.com:o/r.git".to_string(),
            default_branch: "main".to_string(),
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
        Maid::new(
            github.clone(),
            github,
            repos,
            codex,
            MaidSettings {
                bot_login: "maid-bot".to_string(),
                master_accounts: vec!["dionysuzx".to_string()],
                auto_review_accounts: vec!["dionysuzx".to_string()],
                auto_review_repos: vec![RepoSlug {
                    owner: "o".to_string(),
                    repo: "r".to_string(),
                }],
                auto_implement_accounts: vec!["dionysuzx".to_string()],
                auto_implement_repos: vec![RepoSlug {
                    owner: "o".to_string(),
                    repo: "r".to_string(),
                }],
                auto_implement_label: "maid".to_string(),
                auto_implement_window: Duration::from_secs(30 * 24 * 60 * 60),
            },
        )
    }

    fn at_limit_recorder() -> FixedTaskStartRecorder {
        FixedTaskStartRecorder {
            decision: TaskStartDecision::AtLimit {
                limit: 1,
                window: std::time::Duration::from_secs(24 * 60 * 60),
            },
            calls: Arc::new(StdMutex::new(0)),
        }
    }

    #[test]
    fn builds_codex_resume_command_from_session_id() {
        let run = CodexRun {
            response: "done".to_string(),
            session_id: Some("019e64fd-8369-7453-9cdc-4b14b388f618".to_string()),
        };

        assert_eq!(
            run.resume_command(),
            Some((
                "019e64fd-8369-7453-9cdc-4b14b388f618",
                "codex resume --include-non-interactive --all 019e64fd-8369-7453-9cdc-4b14b388f618"
                    .to_string()
            ))
        );
    }

    #[tokio::test]
    async fn responds_then_marks_notification_handled() {
        let checkout = PathBuf::from("/tmp/maid-test-checkout");
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention(
            "dionysuzx",
            "@maid-bot please review this PR",
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
        assert_eq!(calls[0].1.subject_url, "https://github.com/o/r/pull/1");
        assert_eq!(
            calls[0].1.origin,
            CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot please review this PR".to_string(),
                cleaned_text: "please review this PR".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn responds_to_opened_pr_from_auto_review_account() {
        let checkout = PathBuf::from("/tmp/maid-test-checkout");
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr()];
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

        assert_eq!(report.seen, 1);
        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        assert!(github.marks.lock().unwrap().is_empty());
        assert_eq!(
            *github.events.lock().unwrap(),
            vec!["start_pr", "post", "handled_pr"]
        );
        assert_eq!(
            *github.started_prs.lock().unwrap(),
            vec!["https://github.com/o/r/pull/1"]
        );
        assert!(
            github
                .handled_prs
                .lock()
                .unwrap()
                .contains("https://github.com/o/r/pull/1")
        );
        assert_eq!(*repos.calls.lock().unwrap(), vec!["o/r"]);
        let calls = codex.calls.lock().unwrap();
        assert_eq!(calls[0].0, checkout);
        assert_eq!(calls[0].1.subject_url, "https://github.com/o/r/pull/1");
        assert_eq!(
            calls[0].1.origin,
            CodexTaskOrigin::PullRequestOpened {
                author: "dionysuzx".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn implements_labeled_issue_by_opening_pull_request() {
        let checkout = PathBuf::from("/tmp/maid-test-checkout");
        let github = FakeGithub::default();
        *github.issues.lock().unwrap() = vec![issue()];
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

        assert_eq!(report.seen, 1);
        assert_eq!(report.responded, 1);
        assert_eq!(
            *github.events.lock().unwrap(),
            vec!["start_issue", "create_pr", "post_issue", "handled_issue"]
        );
        assert_eq!(
            *github.created_pull_requests.lock().unwrap(),
            vec!["maid/issue-3"]
        );
        assert_eq!(
            *github.posts.lock().unwrap(),
            vec!["Opened pull request: https://github.com/o/r/pull/7"]
        );
        assert!(
            github
                .handled_issues
                .lock()
                .unwrap()
                .contains("https://github.com/o/r/issues/3")
        );
        assert_eq!(
            *repos.calls.lock().unwrap(),
            vec![
                "o/r:maid/issue-3",
                "commit:Implement issue #3: Add the thing",
                "push:maid/issue-3"
            ]
        );
        let calls = codex.calls.lock().unwrap();
        assert_eq!(calls[0].0, checkout);
        assert_eq!(calls[0].1.subject_url, "https://github.com/o/r/issues/3");
        assert_eq!(
            calls[0].1.origin,
            CodexTaskOrigin::IssueImplementation {
                title: "Add the thing".to_string(),
                body: "Please add the missing thing.".to_string(),
                branch: "maid/issue-3".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn skips_labeled_issue_when_existing_maid_pull_request_is_open() {
        let github = FakeGithub::default();
        *github.issues.lock().unwrap() = vec![issue()];
        *github.existing_issue_pr.lock().unwrap() = Some(pr());
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

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.skip_breakdown.existing_issue_pr, 1);
        assert_eq!(*github.events.lock().unwrap(), vec!["handled_issue"]);
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_labeled_issue_from_account_without_auto_implementation() {
        let github = FakeGithub::default();
        *github.issues.lock().unwrap() = vec![issue_with_author("mayushii")];
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

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.skip_breakdown.auto_implement_disabled, 1);
        assert!(github.events.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn leaves_eligible_mention_pending_when_task_limit_is_reached() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let task_starts = at_limit_recorder();

        let report = maid(github.clone(), repos.clone(), codex.clone())
            .with_task_start_recorder(task_starts.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(*task_starts.calls.lock().unwrap(), 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(github.events.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn leaves_auto_review_pr_pending_when_task_limit_is_reached() {
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr()];
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let task_starts = at_limit_recorder();

        let report = maid(github.clone(), repos.clone(), codex.clone())
            .with_task_start_recorder(task_starts.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(*task_starts.calls.lock().unwrap(), 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(github.events.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_opened_pr_from_account_without_auto_review() {
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr_with_author("mayushii")];
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

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_opened_pr_with_durable_handled_marker_after_restart() {
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr()];
        github
            .handled_prs
            .lock()
            .unwrap()
            .insert("https://github.com/o/r/pull/1".to_string());
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

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(github.events.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_self_authored_opened_prs() {
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr_with_author("maid-bot")];
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

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_self_authored_mentions() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("maid-bot", "@maid-bot review"))));
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
    async fn marks_irrelevant_unread_notifications_handled() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![irrelevant_notification("n1")];
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
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_mentions_from_non_master_accounts() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("not-trusted", "@maid-bot review"))));
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
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(repos.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn master_account_matching_is_case_insensitive() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("Dionysuzx", "@maid-bot review"))));
        let repos = FakeRepos {
            checkout: PathBuf::from("/tmp/checkout"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), repos, codex).run_once().await.unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
    }

    #[tokio::test]
    async fn skips_duplicate_latest_comment_after_success() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1"), notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
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
        assert_eq!(first_report.skip_breakdown.duplicate_notification, 1);
        assert_eq!(second_report.responded, 0);
        assert_eq!(second_report.skipped, 2);
        assert_eq!(second_report.skip_breakdown.missing_mention, 1);
        assert_eq!(second_report.skip_breakdown.duplicate_notification, 1);
        assert_eq!(github.posts.lock().unwrap().len(), 1);
        assert_eq!(github.marks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn responds_to_new_latest_comment_on_same_notification_thread() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification_with_comment("n1", "2")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention_with_comment(
            "dionysuzx",
            "@maid-bot first",
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
            "@maid-bot second",
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
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
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
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
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
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
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
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
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
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
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
