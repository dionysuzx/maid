use crate::domain::{
    CodexTask, CodexTaskOrigin, CommentMention, MentionRequest, Notification, PullRequest, RepoSlug,
};
use crate::handled_marker::{
    MemoryPendingHandledMarkerStore, PendingHandledMarker, PendingHandledMarkerStore,
};
use crate::task_limit::{NoTaskLimit, TaskStartDecision, TaskStartRecorder};
use anyhow::Result;
use async_trait::async_trait;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{error, info};

#[async_trait]
pub trait GithubClient: Send + Sync {
    async fn notifications(&self) -> Result<Vec<Notification>>;
    async fn mention_for(&self, notification: &Notification) -> Result<Option<CommentMention>>;
    async fn mentions_for(&self, notification: &Notification) -> Result<Vec<CommentMention>> {
        Ok(self.mention_for(notification).await?.into_iter().collect())
    }
    async fn open_pull_requests(&self, repo: &RepoSlug) -> Result<Vec<PullRequest>>;
    async fn post_pr_comment(&self, pr: &PullRequest, body: &str) -> Result<()>;
    async fn mention_state(&self, mention: &CommentMention, bot_login: &str)
    -> Result<ReviewState>;
    async fn mark_mention_started(&self, mention: &CommentMention) -> Result<()>;
    async fn mark_mention_handled(&self, mention: &CommentMention) -> Result<()>;
    async fn mark_mention_api_url_handled(&self, api_url: &str) -> Result<()>;
    async fn pr_state(&self, pr: &PullRequest, bot_login: &str) -> Result<ReviewState>;
    async fn mark_pr_started(&self, pr: &PullRequest) -> Result<()>;
    async fn mark_pr_handled(&self, pr: &PullRequest) -> Result<()>;
    async fn mark_pull_request_html_url_handled(&self, html_url: &str) -> Result<()>;
    async fn mark_notification_handled(&self, notification: &Notification) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewState {
    Pending,
    Handled,
}

#[async_trait]
pub trait Worktrees: Send + Sync {
    async fn prepare(&self, pr: &PullRequest, task: &CodexTask) -> Result<PreparedWorktree>;
    async fn cleanup(&self, worktree: PreparedWorktree) -> Result<()>;
}

#[async_trait]
pub trait CodexRunner: Send + Sync {
    async fn run(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedWorktree {
    path: PathBuf,
    repo: Option<PathBuf>,
}

impl PreparedWorktree {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            repo: None,
        }
    }

    pub fn git_worktree(repo: impl Into<PathBuf>, path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            repo: Some(repo.into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn repo(&self) -> Option<&Path> {
        self.repo.as_deref()
    }
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
    worktrees: R,
    codex: C,
    bot_login: String,
    master_accounts: HashSet<String>,
    auto_review_accounts: HashSet<String>,
    auto_review_repos: Vec<RepoSlug>,
    task_starts: Arc<dyn TaskStartRecorder>,
    pending_handled_markers: Arc<dyn PendingHandledMarkerStore>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PollReport {
    pub seen: usize,
    pub skipped: usize,
    pub started: usize,
    pub responded: usize,
    pub failed: usize,
    pub in_flight: usize,
}

impl<G, R, C> Maid<G, R, C>
where
    G: GithubClient,
    R: Worktrees,
    C: CodexRunner,
{
    pub fn new(
        github: G,
        worktrees: R,
        codex: C,
        bot_login: impl Into<String>,
        master_accounts: impl IntoIterator<Item = impl Into<String>>,
        auto_review_accounts: impl IntoIterator<Item = impl Into<String>>,
        auto_review_repos: impl IntoIterator<Item = RepoSlug>,
    ) -> Self {
        Self {
            github,
            worktrees,
            codex,
            bot_login: bot_login.into(),
            master_accounts: normalized_logins(master_accounts),
            auto_review_accounts: normalized_logins(auto_review_accounts),
            auto_review_repos: auto_review_repos.into_iter().collect(),
            task_starts: Arc::new(NoTaskLimit),
            pending_handled_markers: Arc::new(MemoryPendingHandledMarkerStore::default()),
        }
    }

    pub fn with_task_start_recorder(
        mut self,
        task_starts: impl TaskStartRecorder + 'static,
    ) -> Self {
        self.task_starts = Arc::new(task_starts);
        self
    }

    pub fn with_pending_handled_marker_store(
        mut self,
        pending_handled_markers: impl PendingHandledMarkerStore + 'static,
    ) -> Self {
        self.pending_handled_markers = Arc::new(pending_handled_markers);
        self
    }

    pub fn into_concurrent(self, max_concurrent_requests: usize) -> ConcurrentMaid<G, R, C> {
        ConcurrentMaid {
            maid: self,
            work: WorkQueue::new(max_concurrent_requests),
        }
    }

    pub async fn run_once(&self) -> Result<PollReport> {
        self.retry_pending_handled_markers().await?;

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

            match self.task_for_notification(&notification).await {
                Ok(assessments) => {
                    self.handle_assessments(&notification, assessments, &mut report)
                        .await;
                }
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
                match self.task_for_auto_review_pr(&pr).await {
                    Ok(TaskAssessment::Ready(intent)) => match self.start_task(intent).await {
                        Ok(TaskStartOutcome::Started(task)) => match self.finish_task(task).await {
                            Ok(()) => report.responded += 1,
                            Err(err) => {
                                report.failed += 1;
                                error!(
                                    pr = %pr.html_url,
                                    error = ?err,
                                    "failed to handle auto-review pull request"
                                );
                            }
                        },
                        Ok(TaskStartOutcome::Skipped) => report.skipped += 1,
                        Err(err) => {
                            report.failed += 1;
                            error!(
                                pr = %pr.html_url,
                                error = ?err,
                                "failed to start auto-review pull request task"
                            );
                        }
                    },
                    Ok(TaskAssessment::Skipped) => report.skipped += 1,
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

        Ok(report)
    }

    async fn retry_pending_handled_markers(&self) -> Result<()> {
        for marker in self.pending_handled_markers.pending()? {
            match &marker {
                PendingHandledMarker::Mention { api_url } => {
                    self.github.mark_mention_api_url_handled(api_url).await?;
                    self.pending_handled_markers.remove(&marker)?;
                    info!(
                        mention_api_url = %api_url,
                        "marked pending completed mention handled"
                    );
                }
                PendingHandledMarker::PullRequest { html_url } => {
                    self.github
                        .mark_pull_request_html_url_handled(html_url)
                        .await?;
                    self.pending_handled_markers.remove(&marker)?;
                    info!(
                        pr = %html_url,
                        "marked pending completed auto-review pull request handled"
                    );
                }
            }
        }

        Ok(())
    }

    async fn handle_assessments(
        &self,
        notification: &Notification,
        assessments: Vec<TaskAssessment>,
        report: &mut PollReport,
    ) {
        for assessment in assessments {
            match assessment {
                TaskAssessment::Ready(intent) => match self.start_task(intent).await {
                    Ok(TaskStartOutcome::Started(task)) => match self.finish_task(task).await {
                        Ok(()) => report.responded += 1,
                        Err(err) => {
                            report.failed += 1;
                            error!(
                                notification_id = notification.id,
                                error = ?err,
                                "failed to handle notification"
                            );
                        }
                    },
                    Ok(TaskStartOutcome::Skipped) => report.skipped += 1,
                    Err(err) => {
                        report.failed += 1;
                        error!(
                            notification_id = notification.id,
                            error = ?err,
                            "failed to start notification task"
                        );
                    }
                },
                TaskAssessment::Skipped => report.skipped += 1,
            }
        }
    }

    async fn task_for_notification(
        &self,
        notification: &Notification,
    ) -> Result<Vec<TaskAssessment>> {
        if notification.is_pr_mention_candidate() {
            return self.tasks_for_mentions(notification).await;
        }

        self.github.mark_notification_handled(notification).await?;
        Ok(vec![TaskAssessment::Skipped])
    }

    async fn tasks_for_mentions(&self, notification: &Notification) -> Result<Vec<TaskAssessment>> {
        let mentions = self.mentions_for_notification(notification).await?;
        let mut assessments = Vec::new();

        for mention in mentions {
            if mention.author.eq_ignore_ascii_case(&self.bot_login) {
                continue;
            }

            if !self
                .master_accounts
                .contains(&mention.author.to_ascii_lowercase())
            {
                info!(
                    notification_id = notification.id,
                    mention = %mention.html_url,
                    author = %mention.author,
                    "skipping mention from non-master account"
                );
                continue;
            }

            let Some(request) = MentionRequest::parse(&mention.body, &self.bot_login)? else {
                continue;
            };

            match self.github.mention_state(&mention, &self.bot_login).await? {
                ReviewState::Pending => {
                    if self
                        .retry_pending_mention_marker(notification, &mention)
                        .await?
                    {
                        assessments.push(TaskAssessment::Skipped);
                        continue;
                    }
                }
                ReviewState::Handled => {
                    self.pending_handled_markers
                        .remove(&PendingHandledMarker::for_mention(&mention))?;
                    info!(
                        notification_id = notification.id,
                        mention = %mention.html_url,
                        "skipping already handled mention"
                    );
                    assessments.push(TaskAssessment::Skipped);
                    continue;
                }
            }

            let task = CodexTask {
                pr_url: mention.pr.html_url.clone(),
                origin: mention_task_origin(&mention, request, &self.bot_login),
            };

            assessments.push(TaskAssessment::Ready(Box::new(TaskIntent::Mention {
                notification: notification.clone(),
                mention: Box::new(mention),
                task,
            })));
        }

        if assessments.iter().all(|assessment| assessment.is_skipped()) {
            self.github.mark_notification_handled(notification).await?;
        }

        if assessments.is_empty() {
            return Ok(vec![TaskAssessment::Skipped]);
        }
        Ok(assessments)
    }

    async fn mentions_for_notification(
        &self,
        notification: &Notification,
    ) -> Result<Vec<CommentMention>> {
        let Some(latest) = self.github.mention_for(notification).await? else {
            return Ok(Vec::new());
        };

        if latest.author.eq_ignore_ascii_case(&self.bot_login) {
            return self.scanned_mentions_or_latest(notification, latest).await;
        }

        if self
            .master_accounts
            .contains(&latest.author.to_ascii_lowercase())
            && MentionRequest::parse(&latest.body, &self.bot_login)?.is_some()
            && self.github.mention_state(&latest, &self.bot_login).await? == ReviewState::Pending
        {
            return self.scanned_mentions_or_latest(notification, latest).await;
        }

        Ok(vec![latest])
    }

    async fn scanned_mentions_or_latest(
        &self,
        notification: &Notification,
        latest: CommentMention,
    ) -> Result<Vec<CommentMention>> {
        let mentions = self.github.mentions_for(notification).await?;
        if mentions.is_empty() {
            return Ok(vec![latest]);
        }

        Ok(mentions)
    }

    async fn task_for_auto_review_pr(&self, pr: &PullRequest) -> Result<TaskAssessment> {
        if pr.author.eq_ignore_ascii_case(&self.bot_login) {
            return Ok(TaskAssessment::Skipped);
        }

        if !self
            .auto_review_accounts
            .contains(&pr.author.to_ascii_lowercase())
        {
            info!(
                pr = %pr.html_url,
                author = %pr.author,
                "skipping pull request from account without auto review"
            );
            return Ok(TaskAssessment::Skipped);
        }

        match self.github.pr_state(pr, &self.bot_login).await? {
            ReviewState::Pending => {
                if self.retry_pending_pr_marker(pr).await? {
                    return Ok(TaskAssessment::Skipped);
                }
            }
            ReviewState::Handled => {
                self.pending_handled_markers
                    .remove(&PendingHandledMarker::for_pull_request(pr))?;
                info!(
                    pr = %pr.html_url,
                    "skipping already handled auto-review pull request"
                );
                return Ok(TaskAssessment::Skipped);
            }
        }

        let task = CodexTask {
            pr_url: pr.html_url.clone(),
            origin: CodexTaskOrigin::PullRequestOpened {
                author: pr.author.clone(),
            },
        };
        Ok(TaskAssessment::Ready(Box::new(
            TaskIntent::AutoReviewPullRequest {
                pr: pr.clone(),
                task,
            },
        )))
    }

    async fn retry_pending_mention_marker(
        &self,
        notification: &Notification,
        mention: &CommentMention,
    ) -> Result<bool> {
        let marker = PendingHandledMarker::for_mention(mention);
        if !self.pending_handled_markers.contains(&marker)? {
            return Ok(false);
        }

        self.github.mark_mention_handled(mention).await?;
        self.pending_handled_markers.remove(&marker)?;
        info!(
            notification_id = notification.id,
            mention = %mention.html_url,
            "marked pending completed mention handled"
        );
        Ok(true)
    }

    async fn retry_pending_pr_marker(&self, pr: &PullRequest) -> Result<bool> {
        let marker = PendingHandledMarker::for_pull_request(pr);
        if !self.pending_handled_markers.contains(&marker)? {
            return Ok(false);
        }

        self.github.mark_pr_handled(pr).await?;
        self.pending_handled_markers.remove(&marker)?;
        info!(
            pr = %pr.html_url,
            "marked pending completed auto-review pull request handled"
        );
        Ok(true)
    }

    async fn start_task(&self, intent: Box<TaskIntent>) -> Result<TaskStartOutcome> {
        if !self.can_start_task()? {
            intent.log_task_limit_reached();
            return Ok(TaskStartOutcome::Skipped);
        }

        match *intent {
            TaskIntent::Mention {
                notification,
                mention,
                task,
            } => self.start_mention_task(notification, mention, task).await,
            TaskIntent::AutoReviewPullRequest { pr, task } => {
                self.start_auto_review_task(pr, task).await
            }
        }
    }

    async fn start_mention_task(
        &self,
        notification: Notification,
        mention: Box<CommentMention>,
        task: CodexTask,
    ) -> Result<TaskStartOutcome> {
        self.github.mark_mention_started(&mention).await?;
        info!(
            notification_id = notification.id,
            pr = %mention.pr.html_url,
            mention = %mention.html_url,
            task_kind = task.task_kind(),
            "started handling pull request mention"
        );

        Ok(TaskStartOutcome::Started(Box::new(StartedTask::Mention {
            notification,
            mention,
            task,
        })))
    }

    async fn finish_task(&self, task: Box<StartedTask>) -> Result<()> {
        match *task {
            StartedTask::Mention {
                notification,
                mention,
                task,
            } => self.finish_mention_task(notification, mention, task).await,
            StartedTask::AutoReviewPullRequest { pr, task } => {
                self.finish_auto_review_task(pr, task).await
            }
        }
    }

    async fn finish_mention_task(
        &self,
        notification: Notification,
        mention: Box<CommentMention>,
        task: CodexTask,
    ) -> Result<()> {
        let worktree = self.worktrees.prepare(&mention.pr, &task).await?;
        let result = async {
            let codex_run = self.codex.run(worktree.path(), &task).await?;

            self.github
                .post_pr_comment(&mention.pr, &codex_run.response)
                .await?;
            let marker = PendingHandledMarker::for_mention(&mention);
            self.pending_handled_markers.record(&marker)?;
            if let Err(err) = self.github.mark_mention_handled(&mention).await {
                error!(
                    notification_id = notification.id,
                    pr = %mention.pr.html_url,
                    mention = %mention.html_url,
                    error = ?err,
                    "failed to mark mention handled after posting response"
                );
            } else {
                self.pending_handled_markers.remove(&marker)?;
            }
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
            Ok(())
        }
        .await;

        self.cleanup_worktree(worktree, result).await
    }

    async fn start_auto_review_task(
        &self,
        pr: PullRequest,
        task: CodexTask,
    ) -> Result<TaskStartOutcome> {
        self.github.mark_pr_started(&pr).await?;
        info!(
            pr = %pr.html_url,
            author = %pr.author,
            task_kind = task.task_kind(),
            "started handling auto-review pull request"
        );

        Ok(TaskStartOutcome::Started(Box::new(
            StartedTask::AutoReviewPullRequest { pr, task },
        )))
    }

    async fn finish_auto_review_task(&self, pr: PullRequest, task: CodexTask) -> Result<()> {
        let worktree = self.worktrees.prepare(&pr, &task).await?;
        let result = async {
            let codex_run = self.codex.run(worktree.path(), &task).await?;

            self.github
                .post_pr_comment(&pr, &codex_run.response)
                .await?;
            let marker = PendingHandledMarker::for_pull_request(&pr);
            self.pending_handled_markers.record(&marker)?;
            if let Err(err) = self.github.mark_pr_handled(&pr).await {
                error!(
                    pr = %pr.html_url,
                    error = ?err,
                    "failed to mark auto-review pull request handled after posting response"
                );
            } else {
                self.pending_handled_markers.remove(&marker)?;
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
            Ok(())
        }
        .await;

        self.cleanup_worktree(worktree, result).await
    }

    async fn cleanup_worktree(
        &self,
        worktree: PreparedWorktree,
        task_result: Result<()>,
    ) -> Result<()> {
        let cleanup_result = self.worktrees.cleanup(worktree).await;
        match (task_result, cleanup_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(err)) => Err(err),
            (Err(err), Ok(())) => Err(err),
            (Err(err), Err(cleanup_err)) => {
                error!(
                    error = ?cleanup_err,
                    "failed to clean up worktree after task failure"
                );
                Err(err)
            }
        }
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

#[derive(Clone)]
pub struct ConcurrentMaid<G, R, C> {
    maid: Maid<G, R, C>,
    work: WorkQueue,
}

impl<G, R, C> ConcurrentMaid<G, R, C>
where
    G: GithubClient + Clone + 'static,
    R: Worktrees + Clone + 'static,
    C: CodexRunner + Clone + 'static,
{
    pub async fn run_once(&self) -> Result<PollReport> {
        let notifications = self.maid.github.notifications().await?;
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

            match self.maid.task_for_notification(&notification).await {
                Ok(assessments) => {
                    for assessment in assessments {
                        match assessment {
                            TaskAssessment::Ready(intent) => {
                                self.start_or_defer(intent, &mut report).await;
                            }
                            TaskAssessment::Skipped => report.skipped += 1,
                        }
                    }
                }
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

        for repo in &self.maid.auto_review_repos {
            let pull_requests = self.maid.github.open_pull_requests(repo).await?;
            report.seen += pull_requests.len();

            for pr in pull_requests {
                match self.maid.task_for_auto_review_pr(&pr).await {
                    Ok(TaskAssessment::Ready(intent)) => {
                        self.start_or_defer(intent, &mut report).await;
                    }
                    Ok(TaskAssessment::Skipped) => report.skipped += 1,
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

        report.in_flight = self.work.in_flight();
        Ok(report)
    }

    async fn start_or_defer(&self, intent: Box<TaskIntent>, report: &mut PollReport) {
        let task_key = intent.task_key();
        let Some(reservation) = self.work.try_reserve(task_key) else {
            report.skipped += 1;
            return;
        };

        match self.maid.start_task(intent).await {
            Ok(TaskStartOutcome::Started(task)) => {
                report.started += 1;
                let maid = self.maid.clone();
                tokio::spawn(async move {
                    let _reservation = reservation;
                    if let Err(err) = maid.finish_task(task.clone()).await {
                        task.log_failure(&err);
                    }
                });
            }
            Ok(TaskStartOutcome::Skipped) => report.skipped += 1,
            Err(err) => {
                report.failed += 1;
                error!(error = ?err, "failed to start task");
            }
        }
    }
}

#[derive(Clone)]
struct WorkQueue {
    slots: Arc<Semaphore>,
    running: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl WorkQueue {
    fn new(max_concurrent_requests: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(max_concurrent_requests.max(1))),
            running: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    fn try_reserve(&self, key: String) -> Option<WorkReservation> {
        let permit = self.slots.clone().try_acquire_owned().ok()?;
        let mut running = self.running.lock().expect("work queue lock is poisoned");
        if !running.insert(key.clone()) {
            return None;
        }

        Some(WorkReservation {
            key,
            running: self.running.clone(),
            _permit: permit,
        })
    }

    fn in_flight(&self) -> usize {
        self.running
            .lock()
            .expect("work queue lock is poisoned")
            .len()
    }
}

struct WorkReservation {
    key: String,
    running: Arc<std::sync::Mutex<HashSet<String>>>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for WorkReservation {
    fn drop(&mut self) {
        self.running
            .lock()
            .expect("work queue lock is poisoned")
            .remove(&self.key);
    }
}

#[derive(Clone, Debug)]
enum TaskAssessment {
    Ready(Box<TaskIntent>),
    Skipped,
}

impl TaskAssessment {
    fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped)
    }
}

#[derive(Clone, Debug)]
enum TaskStartOutcome {
    Started(Box<StartedTask>),
    Skipped,
}

#[derive(Clone, Debug)]
enum TaskIntent {
    Mention {
        notification: Notification,
        mention: Box<CommentMention>,
        task: CodexTask,
    },
    AutoReviewPullRequest {
        pr: PullRequest,
        task: CodexTask,
    },
}

impl TaskIntent {
    fn task_key(&self) -> String {
        match self {
            Self::Mention { task, .. } | Self::AutoReviewPullRequest { task, .. } => {
                format!("task:{}", task.trigger_url())
            }
        }
    }

    fn log_task_limit_reached(&self) {
        match self {
            Self::Mention {
                notification,
                mention,
                ..
            } => info!(
                notification_id = notification.id,
                pr = %mention.pr.html_url,
                mention = %mention.html_url,
                "skipping mention because the 24-hour task limit is reached"
            ),
            Self::AutoReviewPullRequest { pr, .. } => info!(
                pr = %pr.html_url,
                author = %pr.author,
                "skipping auto-review pull request because the 24-hour task limit is reached"
            ),
        }
    }
}

#[derive(Clone, Debug)]
enum StartedTask {
    Mention {
        notification: Notification,
        mention: Box<CommentMention>,
        task: CodexTask,
    },
    AutoReviewPullRequest {
        pr: PullRequest,
        task: CodexTask,
    },
}

impl StartedTask {
    fn log_failure(&self, err: &anyhow::Error) {
        match self {
            Self::Mention {
                notification,
                mention,
                ..
            } => error!(
                notification_id = notification.id,
                pr = %mention.pr.html_url,
                mention = %mention.html_url,
                error = ?err,
                "failed to handle notification task"
            ),
            Self::AutoReviewPullRequest { pr, .. } => error!(
                pr = %pr.html_url,
                error = ?err,
                "failed to handle auto-review pull request task"
            ),
        }
    }
}

impl PendingHandledMarker {
    fn for_mention(mention: &CommentMention) -> Self {
        Self::Mention {
            api_url: mention.api_url.clone(),
        }
    }

    fn for_pull_request(pr: &PullRequest) -> Self {
        Self::PullRequest {
            html_url: pr.html_url.clone(),
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

fn mention_task_origin(
    mention: &CommentMention,
    request: MentionRequest,
    bot_login: &str,
) -> CodexTaskOrigin {
    if let Some(operator_text) = request.operator_text() {
        return CodexTaskOrigin::OperatorMention {
            mention_url: mention.html_url.clone(),
            raw_body: request.raw_body,
            request_text: operator_text,
            trigger_author: mention.author.clone(),
            bot_login: bot_login.to_string(),
        };
    }

    CodexTaskOrigin::Mention {
        mention_url: mention.html_url.clone(),
        raw_body: request.raw_body,
        cleaned_text: request.cleaned_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::sync::{Barrier, Notify};

    type FakeMentionResult = Option<Result<Option<CommentMention>, String>>;
    type FakeMentionsResult = Option<Result<Vec<CommentMention>, String>>;
    #[derive(Clone, Default)]
    struct FakeGithub {
        notifications: Arc<StdMutex<Vec<Notification>>>,
        mention: Arc<StdMutex<FakeMentionResult>>,
        mentions: Arc<StdMutex<FakeMentionsResult>>,
        pull_requests: Arc<StdMutex<Vec<PullRequest>>>,
        posts: Arc<StdMutex<Vec<String>>>,
        marks: Arc<StdMutex<Vec<String>>>,
        events: Arc<StdMutex<Vec<String>>>,
        post_error: Arc<StdMutex<Option<String>>>,
        handled_error: Arc<StdMutex<Option<String>>>,
        started_mentions: Arc<StdMutex<Vec<String>>>,
        handled_mentions: Arc<StdMutex<HashSet<String>>>,
        started_prs: Arc<StdMutex<Vec<String>>>,
        handled_prs: Arc<StdMutex<HashSet<String>>>,
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
            self.take_mention()
        }

        async fn mentions_for(&self, _notification: &Notification) -> Result<Vec<CommentMention>> {
            if let Some(result) = self.mentions.lock().unwrap().take() {
                return result.map_err(|message| anyhow!(message));
            }

            Ok(self.take_mention()?.into_iter().collect())
        }

        async fn open_pull_requests(&self, _repo: &RepoSlug) -> Result<Vec<PullRequest>> {
            Ok(self.pull_requests.lock().unwrap().clone())
        }

        async fn post_pr_comment(&self, _pr: &PullRequest, body: &str) -> Result<()> {
            self.events.lock().unwrap().push("post".to_string());
            if let Some(message) = self.post_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.posts.lock().unwrap().push(body.to_string());
            Ok(())
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

        async fn mark_mention_api_url_handled(&self, api_url: &str) -> Result<()> {
            self.events.lock().unwrap().push("handled".to_string());
            if let Some(message) = self.handled_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.handled_mentions
                .lock()
                .unwrap()
                .insert(api_url.to_string());
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

        async fn mark_pull_request_html_url_handled(&self, html_url: &str) -> Result<()> {
            self.events.lock().unwrap().push("handled_pr".to_string());
            if let Some(message) = self.handled_error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            self.handled_prs
                .lock()
                .unwrap()
                .insert(html_url.to_string());
            Ok(())
        }

        async fn mark_notification_handled(&self, notification: &Notification) -> Result<()> {
            self.events.lock().unwrap().push("mark".to_string());
            self.marks.lock().unwrap().push(notification.id.clone());
            Ok(())
        }
    }

    impl FakeGithub {
        fn take_mention(&self) -> Result<Option<CommentMention>> {
            match self.mention.lock().unwrap().take().unwrap_or(Ok(None)) {
                Ok(value) => Ok(value),
                Err(message) => Err(anyhow!(message)),
            }
        }
    }

    #[derive(Clone)]
    struct FakeWorktrees {
        worktree: PathBuf,
        calls: Arc<StdMutex<Vec<String>>>,
        error: Arc<StdMutex<Option<String>>>,
    }

    #[async_trait]
    impl Worktrees for FakeWorktrees {
        async fn prepare(&self, pr: &PullRequest, _task: &CodexTask) -> Result<PreparedWorktree> {
            self.calls.lock().unwrap().push(pr.repo_key());
            if let Some(message) = self.error.lock().unwrap().take() {
                return Err(anyhow!(message));
            }
            Ok(PreparedWorktree::new(self.worktree.clone()))
        }

        async fn cleanup(&self, _worktree: PreparedWorktree) -> Result<()> {
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
        async fn run(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun> {
            self.calls
                .lock()
                .unwrap()
                .push((worktree.to_path_buf(), task.clone()));
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
    struct BlockingCodex {
        calls: Arc<StdMutex<Vec<(PathBuf, CodexTask)>>>,
        entered: Arc<Barrier>,
        release: Arc<Notify>,
    }

    impl BlockingCodex {
        fn new() -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                entered: Arc::new(Barrier::new(2)),
                release: Arc::new(Notify::new()),
            }
        }
    }

    #[async_trait]
    impl CodexRunner for BlockingCodex {
        async fn run(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun> {
            self.calls
                .lock()
                .unwrap()
                .push((worktree.to_path_buf(), task.clone()));
            self.entered.wait().await;
            self.release.notified().await;
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
        pr_with_author_and_number(author, 1)
    }

    fn pr_with_number(number: u64) -> PullRequest {
        pr_with_author_and_number("dionysuzx", number)
    }

    fn pr_with_author_and_number(author: &str, number: u64) -> PullRequest {
        PullRequest {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number,
            author: author.to_string(),
            api_url: format!("https://api.github.com/repos/o/r/pulls/{number}"),
            html_url: format!("https://github.com/o/r/pull/{number}"),
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

    fn maid<C>(
        github: FakeGithub,
        worktrees: FakeWorktrees,
        codex: C,
    ) -> Maid<FakeGithub, FakeWorktrees, C>
    where
        C: CodexRunner,
    {
        Maid::new(
            github,
            worktrees,
            codex,
            "maid-bot",
            ["dionysuzx"],
            ["dionysuzx"],
            [RepoSlug {
                owner: "o".to_string(),
                repo: "r".to_string(),
            }],
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

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("condition was not met before timeout");
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

    #[test]
    fn task_keys_allow_distinct_mentions_on_the_same_pull_request() {
        let first_mention = mention_with_comment("dionysuzx", "@maid-bot first", "2");
        let second_mention = mention_with_comment("dionysuzx", "@maid-bot second", "3");
        let first = TaskIntent::Mention {
            notification: notification_with_comment("n1", "2"),
            task: CodexTask {
                pr_url: first_mention.pr.html_url.clone(),
                origin: CodexTaskOrigin::Mention {
                    mention_url: first_mention.html_url.clone(),
                    raw_body: first_mention.body.clone(),
                    cleaned_text: "first".to_string(),
                },
            },
            mention: Box::new(first_mention),
        };
        let second = TaskIntent::Mention {
            notification: notification_with_comment("n1", "3"),
            task: CodexTask {
                pr_url: second_mention.pr.html_url.clone(),
                origin: CodexTaskOrigin::Mention {
                    mention_url: second_mention.html_url.clone(),
                    raw_body: second_mention.body.clone(),
                    cleaned_text: "second".to_string(),
                },
            },
            mention: Box::new(second_mention),
        };

        assert_ne!(first.task_key(), second.task_key());
    }

    #[tokio::test]
    async fn responds_then_marks_mention_handled() {
        let worktree = PathBuf::from("/tmp/maid-test-worktree");
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention(
            "dionysuzx",
            "@maid-bot please review this PR",
        ))));
        let worktrees = FakeWorktrees {
            worktree: worktree.clone(),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        assert!(github.marks.lock().unwrap().is_empty());
        assert_eq!(
            *github.events.lock().unwrap(),
            vec!["start", "post", "handled"]
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
        assert_eq!(*worktrees.calls.lock().unwrap(), vec!["o/r"]);
        let calls = codex.calls.lock().unwrap();
        assert_eq!(calls[0].0, worktree);
        assert_eq!(calls[0].1.pr_url, "https://github.com/o/r/pull/1");
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
    async fn trusted_operate_mention_uses_operator_task_origin() {
        let worktree = PathBuf::from("/tmp/maid-test-worktree");
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention(
            "dionysuzx",
            "@maid-bot /operate implement and push",
        ))));
        let worktrees = FakeWorktrees {
            worktree: worktree.clone(),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        let calls = codex.calls.lock().unwrap();
        assert_eq!(calls[0].0, worktree);
        assert_eq!(
            calls[0].1.origin,
            CodexTaskOrigin::OperatorMention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot /operate implement and push".to_string(),
                request_text: "implement and push".to_string(),
                trigger_author: "dionysuzx".to_string(),
                bot_login: "maid-bot".to_string(),
            }
        );
        let templates = crate::domain::CodexPromptTemplates {
            mention: String::new(),
            pull_request_opened: String::new(),
            operator_mention: "operate {{request_text}} for {{trigger_author}}".to_string(),
        };
        assert_eq!(
            calls[0].1.prompt(&templates).unwrap(),
            "operate implement and push for dionysuzx"
        );
    }

    #[tokio::test]
    async fn responds_to_opened_pr_from_auto_review_account() {
        let worktree = PathBuf::from("/tmp/maid-test-worktree");
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr()];
        let worktrees = FakeWorktrees {
            worktree: worktree.clone(),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
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
        assert_eq!(*worktrees.calls.lock().unwrap(), vec!["o/r"]);
        let calls = codex.calls.lock().unwrap();
        assert_eq!(calls[0].0, worktree);
        assert_eq!(calls[0].1.pr_url, "https://github.com/o/r/pull/1");
        assert_eq!(
            calls[0].1.origin,
            CodexTaskOrigin::PullRequestOpened {
                author: "dionysuzx".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn concurrent_run_returns_after_starting_work() {
        let worktree = PathBuf::from("/tmp/maid-test-worktree");
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr()];
        let worktrees = FakeWorktrees {
            worktree,
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = BlockingCodex::new();
        let entered = codex.entered.clone();
        let release = codex.release.clone();
        let maid = maid(github.clone(), worktrees, codex).into_concurrent(1);

        let report = maid.run_once().await.unwrap();

        assert_eq!(report.seen, 1);
        assert_eq!(report.started, 1);
        assert_eq!(report.responded, 0);
        assert_eq!(report.in_flight, 1);
        assert!(github.posts.lock().unwrap().is_empty());

        entered.wait().await;
        assert!(
            github.posts.lock().unwrap().is_empty(),
            "Codex is still running, so the response should not be posted yet"
        );

        release.notify_waiters();
        wait_until(|| github.posts.lock().unwrap().len() == 1).await;
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
    }

    #[tokio::test]
    async fn concurrent_run_respects_max_concurrent_requests() {
        let worktree = PathBuf::from("/tmp/maid-test-worktree");
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr_with_number(1), pr_with_number(2)];
        let worktrees = FakeWorktrees {
            worktree,
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = BlockingCodex::new();
        let entered = codex.entered.clone();
        let release = codex.release.clone();
        let maid = maid(github.clone(), worktrees, codex.clone()).into_concurrent(1);

        let first_report = maid.run_once().await.unwrap();

        assert_eq!(first_report.seen, 2);
        assert_eq!(first_report.started, 1);
        assert_eq!(first_report.skipped, 1);
        assert_eq!(first_report.in_flight, 1);
        entered.wait().await;
        assert_eq!(codex.calls.lock().unwrap().len(), 1);

        release.notify_waiters();
        wait_until(|| github.handled_prs.lock().unwrap().len() == 1).await;

        let second_report = maid.run_once().await.unwrap();

        assert_eq!(second_report.started, 1);
        assert_eq!(second_report.skipped, 1);
        entered.wait().await;
        release.notify_waiters();
        wait_until(|| github.handled_prs.lock().unwrap().len() == 2).await;
    }

    #[tokio::test]
    async fn leaves_eligible_mention_pending_when_task_limit_is_reached() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let task_starts = at_limit_recorder();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .with_task_start_recorder(task_starts.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(*task_starts.calls.lock().unwrap(), 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(github.events.lock().unwrap().is_empty());
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn leaves_auto_review_pr_pending_when_task_limit_is_reached() {
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr()];
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let task_starts = at_limit_recorder();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
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
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_opened_pr_from_account_without_auto_review() {
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr_with_author("mayushii")];
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(worktrees.calls.lock().unwrap().is_empty());
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
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(github.events.lock().unwrap().is_empty());
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_self_authored_opened_prs() {
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr_with_author("maid-bot")];
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.seen, 1);
        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_self_authored_mentions() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("maid-bot", "@maid-bot review"))));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn marks_irrelevant_unread_notifications_handled() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![irrelevant_notification("n1")];
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(github.posts.lock().unwrap().is_empty());
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_mentions_from_non_master_accounts() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() =
            Some(Ok(Some(mention("not-trusted", "@maid-bot review"))));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/unused"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn master_account_matching_is_case_insensitive() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("Dionysuzx", "@maid-bot review"))));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees, codex)
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
    }

    #[tokio::test]
    async fn skips_duplicate_latest_comment_after_success() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1"), notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), worktrees, codex);

        let first_report = maid.run_once().await.unwrap();
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
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
            "@maid-bot first",
            "2",
        ))));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), worktrees, codex);

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
        assert!(github.marks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn responds_to_pending_mention_hidden_behind_latest_bot_comment() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification_with_comment("n1", "4")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention_with_comment(
            "maid-bot",
            "codex response",
            "4",
        ))));
        github
            .handled_mentions
            .lock()
            .unwrap()
            .insert("https://api.github.com/repos/o/r/issues/comments/2".to_string());
        *github.mentions.lock().unwrap() = Some(Ok(vec![
            mention_with_comment("dionysuzx", "@maid-bot first", "2"),
            mention_with_comment("dionysuzx", "@maid-bot second", "3"),
            mention_with_comment("maid-bot", "codex response", "4"),
        ]));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees, codex)
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(
            *github.started_mentions.lock().unwrap(),
            vec!["https://api.github.com/repos/o/r/issues/comments/3"]
        );
        assert_eq!(github.posts.lock().unwrap().len(), 1);
        assert!(github.marks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_run_starts_multiple_pending_mentions_from_same_notification() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification_with_comment("n1", "4")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention_with_comment(
            "maid-bot",
            "codex response",
            "4",
        ))));
        *github.mentions.lock().unwrap() = Some(Ok(vec![
            mention_with_comment("dionysuzx", "@maid-bot first", "2"),
            mention_with_comment("dionysuzx", "@maid-bot second", "3"),
            mention_with_comment("dionysuzx", "@maid-bot third", "4"),
        ]));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), worktrees, codex).into_concurrent(4);

        let report = maid.run_once().await.unwrap();

        assert_eq!(report.started, 3);
        assert_eq!(
            *github.started_mentions.lock().unwrap(),
            vec![
                "https://api.github.com/repos/o/r/issues/comments/2",
                "https://api.github.com/repos/o/r/issues/comments/3",
                "https://api.github.com/repos/o/r/issues/comments/4"
            ]
        );
        assert!(github.marks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_run_scans_when_latest_mention_is_pending_trusted_request() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification_with_comment("n1", "4")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention_with_comment(
            "dionysuzx",
            "@maid-bot third",
            "4",
        ))));
        *github.mentions.lock().unwrap() = Some(Ok(vec![
            mention_with_comment("dionysuzx", "@maid-bot first", "2"),
            mention_with_comment("dionysuzx", "@maid-bot second", "3"),
            mention_with_comment("dionysuzx", "@maid-bot third", "4"),
        ]));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), worktrees, codex).into_concurrent(4);

        let report = maid.run_once().await.unwrap();

        assert_eq!(report.started, 3);
        assert_eq!(
            *github.started_mentions.lock().unwrap(),
            vec![
                "https://api.github.com/repos/o/r/issues/comments/2",
                "https://api.github.com/repos/o/r/issues/comments/3",
                "https://api.github.com/repos/o/r/issues/comments/4"
            ]
        );
    }

    #[tokio::test]
    async fn handled_latest_trusted_request_does_not_resurrect_older_mentions() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification_with_comment("n1", "4")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention_with_comment(
            "dionysuzx",
            "@maid-bot already handled",
            "4",
        ))));
        *github.mentions.lock().unwrap() = Some(Ok(vec![
            mention_with_comment("dionysuzx", "@maid-bot older pending", "2"),
            mention_with_comment("dionysuzx", "@maid-bot already handled", "4"),
        ]));
        github
            .handled_mentions
            .lock()
            .unwrap()
            .insert("https://api.github.com/repos/o/r/issues/comments/4".to_string());
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees, codex)
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert!(github.started_mentions.lock().unwrap().is_empty());
        assert!(github.posts.lock().unwrap().is_empty());
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
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
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees.clone(), codex.clone())
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.skipped, 1);
        assert!(github.posts.lock().unwrap().is_empty());
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1"]);
        assert!(worktrees.calls.lock().unwrap().is_empty());
        assert!(codex.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repo_prep_failure_does_not_post_or_mark() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(Some("clone failed".to_string()))),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees, codex.clone())
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
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        *codex.error.lock().unwrap() = Some("codex failed".to_string());

        let report = maid(github.clone(), worktrees, codex)
            .run_once()
            .await
            .unwrap();

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
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees, codex)
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.failed, 1);
        assert!(github.marks.lock().unwrap().is_empty());
        assert_eq!(*github.events.lock().unwrap(), vec!["start", "post"]);
    }

    #[tokio::test]
    async fn handled_marker_failure_after_post_leaves_notification_pending() {
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        *github.handled_error.lock().unwrap() = Some("reaction failed".to_string());
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();

        let report = maid(github.clone(), worktrees, codex)
            .run_once()
            .await
            .unwrap();

        assert_eq!(report.responded, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        assert!(github.marks.lock().unwrap().is_empty());
        assert!(github.handled_mentions.lock().unwrap().is_empty());
        assert_eq!(
            *github.events.lock().unwrap(),
            vec!["start", "post", "handled"]
        );
    }

    #[tokio::test]
    async fn retries_pending_mention_marker_without_rerunning_codex() {
        let temp = tempfile::tempdir().unwrap();
        let marker_path = temp.path().join("pending-handled-markers.json");
        let github = FakeGithub::default();
        *github.notifications.lock().unwrap() = vec![notification("n1")];
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        *github.handled_error.lock().unwrap() = Some("reaction failed".to_string());
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), worktrees, codex.clone())
            .with_pending_handled_marker_store(
                crate::handled_marker::FilePendingHandledMarkerStore::new(&marker_path),
            );

        let first_report = maid.run_once().await.unwrap();
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        let second_report = maid.run_once().await.unwrap();
        *github.mention.lock().unwrap() = Some(Ok(Some(mention("dionysuzx", "@maid-bot review"))));
        let third_report = maid.run_once().await.unwrap();

        assert_eq!(first_report.responded, 1);
        assert_eq!(second_report.skipped, 1);
        assert_eq!(third_report.skipped, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        assert_eq!(codex.calls.lock().unwrap().len(), 1);
        assert_eq!(*github.marks.lock().unwrap(), vec!["n1", "n1"]);
        assert!(
            github
                .handled_mentions
                .lock()
                .unwrap()
                .contains("https://api.github.com/repos/o/r/issues/comments/2")
        );
    }

    #[tokio::test]
    async fn retries_pending_mention_marker_without_notification() {
        let temp = tempfile::tempdir().unwrap();
        let marker_path = temp.path().join("pending-handled-markers.json");
        let marker = PendingHandledMarker::Mention {
            api_url: "https://api.github.com/repos/o/r/issues/comments/2".to_string(),
        };
        let store = crate::handled_marker::FilePendingHandledMarkerStore::new(&marker_path);
        store.record(&marker).unwrap();

        let github = FakeGithub::default();
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid =
            maid(github.clone(), worktrees, codex.clone()).with_pending_handled_marker_store(store);

        let report = maid.run_once().await.unwrap();

        assert_eq!(report.started, 0);
        assert_eq!(*github.posts.lock().unwrap(), Vec::<String>::new());
        assert_eq!(codex.calls.lock().unwrap().len(), 0);
        assert!(
            github
                .handled_mentions
                .lock()
                .unwrap()
                .contains("https://api.github.com/repos/o/r/issues/comments/2")
        );
        assert!(
            !crate::handled_marker::FilePendingHandledMarkerStore::new(&marker_path)
                .contains(&marker)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn retries_pending_auto_review_marker_without_rerunning_codex() {
        let temp = tempfile::tempdir().unwrap();
        let marker_path = temp.path().join("pending-handled-markers.json");
        let github = FakeGithub::default();
        *github.pull_requests.lock().unwrap() = vec![pr()];
        *github.handled_error.lock().unwrap() = Some("reaction failed".to_string());
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid = maid(github.clone(), worktrees, codex.clone())
            .with_pending_handled_marker_store(
                crate::handled_marker::FilePendingHandledMarkerStore::new(&marker_path),
            );

        let first_report = maid.run_once().await.unwrap();
        let second_report = maid.run_once().await.unwrap();

        assert_eq!(first_report.responded, 1);
        assert_eq!(second_report.skipped, 1);
        assert_eq!(*github.posts.lock().unwrap(), vec!["codex response"]);
        assert_eq!(codex.calls.lock().unwrap().len(), 1);
        assert!(
            github
                .handled_prs
                .lock()
                .unwrap()
                .contains("https://github.com/o/r/pull/1")
        );
    }

    #[tokio::test]
    async fn retries_pending_auto_review_marker_without_open_pull_request() {
        let temp = tempfile::tempdir().unwrap();
        let marker_path = temp.path().join("pending-handled-markers.json");
        let marker = PendingHandledMarker::PullRequest {
            html_url: "https://github.com/o/r/pull/1".to_string(),
        };
        let store = crate::handled_marker::FilePendingHandledMarkerStore::new(&marker_path);
        store.record(&marker).unwrap();

        let github = FakeGithub::default();
        let worktrees = FakeWorktrees {
            worktree: PathBuf::from("/tmp/worktree"),
            calls: Arc::new(StdMutex::new(Vec::new())),
            error: Arc::new(StdMutex::new(None)),
        };
        let codex = FakeCodex::default();
        let maid =
            maid(github.clone(), worktrees, codex.clone()).with_pending_handled_marker_store(store);

        let report = maid.run_once().await.unwrap();

        assert_eq!(report.started, 0);
        assert_eq!(*github.posts.lock().unwrap(), Vec::<String>::new());
        assert_eq!(codex.calls.lock().unwrap().len(), 0);
        assert!(
            github
                .handled_prs
                .lock()
                .unwrap()
                .contains("https://github.com/o/r/pull/1")
        );
        assert!(
            !crate::handled_marker::FilePendingHandledMarkerStore::new(&marker_path)
                .contains(&marker)
                .unwrap()
        );
    }
}
