use crate::{
    domain::{
        CommentMention, CommitSha, Issue, Notification, OpenPullRequest, OpenPullRequestOrder,
        PullRequest, RepoSlug, ReviewState, TeamSlug, TreeSha, WorkTarget,
    },
    maid::GithubClient,
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use reqwest::{Client, Method, StatusCode, Url, header::HeaderMap};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use tracing::warn;

const STARTED_REACTION: &str = "eyes";
const HANDLED_REACTION: &str = "+1";
pub const DEFAULT_GITHUB_API_REQUESTS_PER_HOUR: u32 = 1_200;
pub const DEFAULT_GITHUB_NOTIFICATION_WINDOW_HOURS: u32 = 24;
const MAX_RATE_LIMIT_RETRIES: usize = 5;

/// How a request treats GitHub rate limiting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RateLimitPolicy {
    /// Wait out backoffs and retry, for requests Maid needs.
    Wait,
    /// Fail instead of waiting, for cosmetic requests. Backoffs are still recorded.
    Skip,
}

impl RateLimitPolicy {
    fn max_retries(self) -> usize {
        match self {
            Self::Wait => MAX_RATE_LIMIT_RETRIES,
            Self::Skip => 0,
        }
    }
}
const NOTIFICATION_PAGE_SIZE: usize = 50;
const SEARCH_PAGE_SIZE: usize = 100;
const MAX_SEARCH_RESULTS: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubNotificationWindow {
    hours: u32,
}

impl GitHubNotificationWindow {
    pub fn hours(hours: u32) -> Result<Self> {
        if hours == 0 {
            bail!("github_notification_window_hours must be at least 1");
        }

        Ok(Self { hours })
    }

    pub fn as_hours(self) -> u32 {
        self.hours
    }
}

impl Default for GitHubNotificationWindow {
    fn default() -> Self {
        Self {
            hours: DEFAULT_GITHUB_NOTIFICATION_WINDOW_HOURS,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitHubApiRequestRate {
    requests_per_hour: u32,
}

impl GitHubApiRequestRate {
    pub fn per_hour(requests_per_hour: u32) -> Result<Self> {
        if requests_per_hour == 0 {
            bail!("github_api_requests_per_hour must be at least 1");
        }

        Ok(Self { requests_per_hour })
    }

    pub fn requests_per_hour(self) -> u32 {
        self.requests_per_hour
    }

    fn interval(self) -> Duration {
        Duration::from_secs_f64(3_600.0 / f64::from(self.requests_per_hour))
    }
}

impl Default for GitHubApiRequestRate {
    fn default() -> Self {
        Self {
            requests_per_hour: DEFAULT_GITHUB_API_REQUESTS_PER_HOUR,
        }
    }
}

#[derive(Clone)]
pub struct GitHubRestClient {
    client: Client,
    token: String,
    traffic: GitHubTraffic,
    notifications: NotificationPolling,
    notification_window: GitHubNotificationWindow,
}

impl GitHubRestClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_api_ip(token, None)
    }

    pub fn with_api_ip(token: impl Into<String>, api_ip: Option<IpAddr>) -> Self {
        Self::with_options(
            token,
            api_ip,
            GitHubApiRequestRate::default(),
            GitHubNotificationWindow::default(),
        )
    }

    pub fn with_options(
        token: impl Into<String>,
        api_ip: Option<IpAddr>,
        request_rate: GitHubApiRequestRate,
        notification_window: GitHubNotificationWindow,
    ) -> Self {
        let mut builder = Client::builder().timeout(Duration::from_secs(30));
        if let Some(api_ip) = api_ip {
            builder = builder.resolve("api.github.com", (api_ip, 443).into());
        }

        Self {
            client: builder
                .build()
                .expect("GitHub HTTP client configuration should be valid"),
            token: token.into(),
            traffic: GitHubTraffic::new(request_rate),
            notifications: NotificationPolling::default(),
            notification_window,
        }
    }

    async fn get<T>(&self, url: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.request(
            Method::GET,
            url,
            Option::<&()>::None,
            HeaderMap::new(),
            RateLimitPolicy::Wait,
        )
        .await?
        .into_json()
    }

    async fn post_json<T>(&self, url: &str, body: &T) -> Result<()>
    where
        T: Serialize + Sync,
    {
        self.post_json_with(url, body, RateLimitPolicy::Wait).await
    }

    async fn post_json_with<T>(&self, url: &str, body: &T, policy: RateLimitPolicy) -> Result<()>
    where
        T: Serialize + Sync,
    {
        let _: serde::de::IgnoredAny = self
            .request(Method::POST, url, Some(body), HeaderMap::new(), policy)
            .await?
            .into_json()?;
        Ok(())
    }

    async fn request<T, B>(
        &self,
        method: Method,
        url: &str,
        body: Option<&B>,
        headers: HeaderMap,
        policy: RateLimitPolicy,
    ) -> Result<GitHubResponse<T>>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + Sync + ?Sized,
    {
        let method_for_error = method.clone();
        let max_retries = policy.max_retries();
        for attempt in 0..=max_retries {
            self.traffic
                .wait_for_turn(policy)
                .await
                .with_context(|| format!("{method_for_error} {url} skipped"))?;

            let mut request = self
                .client
                .request(method.clone(), url)
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("User-Agent", "maid");
            for (name, value) in headers.iter() {
                request = request.header(name, value);
            }
            if let Some(body) = body {
                request = request.json(body);
            }

            let response = request
                .send()
                .await
                .with_context(|| format!("{method_for_error} {url} failed"))?;

            let status = response.status();
            let response_headers = response.headers().clone();
            if status == StatusCode::NO_CONTENT || status == StatusCode::RESET_CONTENT {
                let body = serde_json::from_str("null")
                    .context("failed to decode empty GitHub response")?;
                return Ok(GitHubResponse::Json {
                    body,
                    headers: response_headers,
                });
            }

            if status.is_success() {
                let body = response
                    .json::<T>()
                    .await
                    .with_context(|| format!("invalid JSON from {method_for_error} {url}"))?;
                return Ok(GitHubResponse::Json {
                    body,
                    headers: response_headers,
                });
            }

            let response_body = response.text().await.unwrap_or_default();
            if let Some(backoff) =
                RateLimitBackoff::from_response(status, &response_headers, &response_body, attempt)
            {
                self.traffic.back_off(backoff.delay).await;
                if attempt == max_retries {
                    return Err(anyhow!(
                        "{method_for_error} {url} still rate limited after {max_retries} retries: {response_body}"
                    ));
                }

                warn!(
                    method = %method_for_error,
                    url,
                    status = %status,
                    backoff_seconds = backoff.delay.as_secs(),
                    reason = backoff.reason,
                    "GitHub rate limit reached; backing off"
                );
                continue;
            }

            return Err(anyhow!(
                "{method_for_error} {url} returned {status}: {response_body}"
            ));
        }

        Err(anyhow!(
            "{method_for_error} {url} still rate limited after {max_retries} retries"
        ))
    }
}

#[async_trait]
impl GithubClient for GitHubRestClient {
    async fn notifications(&self) -> Result<Vec<Notification>> {
        self.notifications.wait_until_allowed().await;
        let since = recent_notification_since(self.notification_window);
        let mut notifications = Vec::new();

        for page in 1.. {
            let response = self
                .request::<Vec<ApiNotification>, _>(
                    Method::GET,
                    &notification_page_url(&since, page),
                    Option::<&()>::None,
                    HeaderMap::new(),
                    RateLimitPolicy::Wait,
                )
                .await?;
            self.notifications.record_headers(response.headers()).await;

            let page_notifications = response.into_json()?;
            let done = page_notifications.len() < NOTIFICATION_PAGE_SIZE;
            notifications.extend(
                page_notifications
                    .into_iter()
                    .map(|notification| Notification {
                        id: notification.id,
                        reason: notification.reason,
                        subject_kind: notification.subject.kind,
                        subject_url: notification.subject.url,
                        latest_comment_url: notification.subject.latest_comment_url,
                        unread: notification.unread,
                        updated_at: notification.updated_at,
                    }),
            );
            if done {
                break;
            }
        }

        Ok(notifications)
    }

    async fn mention_for(&self, notification: &Notification) -> Result<Option<CommentMention>> {
        let Some(comment_url) = comment_url(notification) else {
            return Ok(None);
        };

        let comment = self.get::<ApiComment>(comment_url).await?;
        let Some(target) = self
            .target_for_comment_notification(notification, &comment)
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(CommentMention {
            author: comment.user.login,
            body: comment.body,
            api_url: comment_url.to_string(),
            html_url: comment.html_url,
            target,
        }))
    }

    async fn mentions_for(&self, notification: &Notification) -> Result<Vec<CommentMention>> {
        let Some(target) = self.target_for_notification(notification).await? else {
            return Ok(Vec::new());
        };

        let comments = match &target {
            WorkTarget::PullRequest(pr) => {
                let mut comments = self.issue_comments_for_pr(pr).await?;
                comments.append(&mut self.review_comments_for_pr(pr).await?);
                comments
            }
            WorkTarget::Issue(issue) => self.issue_comments_for_issue(issue).await?,
        };

        Ok(recent_comments(comments, 20)
            .into_iter()
            .map(|comment| comment_mention(comment, &target))
            .collect())
    }

    async fn open_pull_requests(
        &self,
        repo: &RepoSlug,
        order: OpenPullRequestOrder,
    ) -> Result<Vec<OpenPullRequest>> {
        let pull_requests = self
            .get::<Vec<ApiPullRequest>>(&open_pull_requests_url(repo, order))
            .await?;
        pull_requests
            .into_iter()
            .map(Self::open_pull_request_from_api)
            .collect()
    }

    async fn open_public_pull_requests_by_author(&self, author: &str) -> Result<Vec<PullRequest>> {
        let mut pull_requests = Vec::new();
        let mut repositories = HashMap::new();

        for page in 1..=(MAX_SEARCH_RESULTS / SEARCH_PAGE_SIZE) {
            let search = self
                .get::<ApiIssueSearch>(&public_pull_request_search_url(author, page)?)
                .await?;
            if search.incomplete_results {
                warn!(
                    author,
                    page, "GitHub returned incomplete public pull request search results"
                );
            }
            if page == 1 && search.total_count > MAX_SEARCH_RESULTS {
                warn!(
                    author,
                    total_count = search.total_count,
                    max_results = MAX_SEARCH_RESULTS,
                    "GitHub capped public pull request search results"
                );
            }
            let done = search.items.len() < SEARCH_PAGE_SIZE;

            for item in search.items {
                let Some(repository_url) = item.repository_url.clone() else {
                    continue;
                };
                if !repositories.contains_key(&repository_url) {
                    let repository = self.get::<ApiRepo>(&repository_url).await?;
                    repositories.insert(repository_url.clone(), repository);
                }
                if let Some(pull_request_url) =
                    public_pull_request_url(item, &repositories[&repository_url])?
                {
                    pull_requests.push(self.pull_request_from_url(&pull_request_url).await?);
                }
            }

            if done {
                break;
            }
        }

        Ok(pull_requests)
    }

    async fn head_tree(&self, pr: &PullRequest) -> Result<TreeSha> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/git/commits/{}",
            pr.owner, pr.repo, pr.head
        );
        let commit = self.get::<ApiGitCommit>(&url).await?;
        TreeSha::parse(&commit.tree.sha)
    }

    async fn post_comment(&self, target: &WorkTarget, body: &str) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/comments",
            target.owner(),
            target.repo(),
            target.number()
        );
        self.post_json(&url, &PostComment { body }).await
    }

    async fn post_pr_comment(&self, pr: &PullRequest, body: &str) -> Result<()> {
        self.post_comment(&WorkTarget::PullRequest(pr.clone()), body)
            .await
    }

    async fn mention_state(
        &self,
        mention: &CommentMention,
        bot_login: &str,
    ) -> Result<ReviewState> {
        if self
            .mention_has_reaction(mention, bot_login, HANDLED_REACTION)
            .await?
        {
            Ok(ReviewState::Handled)
        } else {
            Ok(ReviewState::Pending)
        }
    }

    async fn mark_mention_started(&self, mention: &CommentMention) -> Result<()> {
        self.add_reaction(mention, STARTED_REACTION).await
    }

    async fn mark_mention_handled(&self, mention: &CommentMention) -> Result<()> {
        self.add_reaction(mention, HANDLED_REACTION).await
    }

    async fn mark_pr_started(&self, pr: &PullRequest) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/reactions",
            pr.owner, pr.repo, pr.number
        );
        // The reaction is cosmetic, so it must never hold up the poll in a backoff.
        self.post_json_with(
            &url,
            &PostReaction {
                content: STARTED_REACTION,
            },
            RateLimitPolicy::Skip,
        )
        .await
    }

    async fn mark_mention_api_url_handled(&self, api_url: &str) -> Result<()> {
        self.add_reaction_to_api_url(api_url, HANDLED_REACTION)
            .await
    }

    async fn mark_notification_handled(&self, notification: &Notification) -> Result<()> {
        let url = format!(
            "https://api.github.com/notifications/threads/{}",
            notification.id
        );
        let _: serde::de::IgnoredAny = self
            .request(
                Method::PATCH,
                &url,
                Option::<&()>::None,
                HeaderMap::new(),
                RateLimitPolicy::Wait,
            )
            .await?
            .into_json()?;
        Ok(())
    }
}

enum GitHubResponse<T> {
    Json { body: T, headers: HeaderMap },
}

impl<T> GitHubResponse<T> {
    fn headers(&self) -> &HeaderMap {
        match self {
            Self::Json { headers, .. } => headers,
        }
    }

    fn into_json(self) -> Result<T> {
        match self {
            Self::Json { body, .. } => Ok(body),
        }
    }
}

#[derive(Clone)]
struct GitHubTraffic {
    interval: Duration,
    next_request_at: Arc<Mutex<Option<tokio::time::Instant>>>,
    backoff_until: Arc<Mutex<Option<tokio::time::Instant>>>,
}

impl GitHubTraffic {
    fn new(rate: GitHubApiRequestRate) -> Self {
        Self {
            interval: rate.interval(),
            next_request_at: Arc::new(Mutex::new(None)),
            backoff_until: Arc::new(Mutex::new(None)),
        }
    }

    async fn wait_for_turn(&self, policy: RateLimitPolicy) -> Result<()> {
        loop {
            let wait = self.backoff_delay().await;
            if !wait.is_zero() {
                if policy == RateLimitPolicy::Skip {
                    bail!("GitHub rate limit backoff is active");
                }
                tokio::time::sleep(wait).await;
                continue;
            }

            let wait = self.request_slot_delay().await;
            if wait.is_zero() {
                return Ok(());
            }
            tokio::time::sleep(wait).await;
        }
    }

    async fn back_off(&self, delay: Duration) {
        let until = tokio::time::Instant::now() + delay;
        let mut backoff_until = self.backoff_until.lock().await;
        if backoff_until.is_none_or(|current| until > current) {
            *backoff_until = Some(until);
        }
    }

    async fn backoff_delay(&self) -> Duration {
        let mut backoff_until = self.backoff_until.lock().await;
        let Some(until) = *backoff_until else {
            return Duration::ZERO;
        };

        let now = tokio::time::Instant::now();
        if until <= now {
            *backoff_until = None;
            Duration::ZERO
        } else {
            until - now
        }
    }

    async fn request_slot_delay(&self) -> Duration {
        let mut next_request_at = self.next_request_at.lock().await;
        let now = tokio::time::Instant::now();
        match *next_request_at {
            Some(next) if next > now => next - now,
            _ => {
                *next_request_at = Some(now + self.interval);
                Duration::ZERO
            }
        }
    }
}

#[derive(Clone, Default)]
struct NotificationPolling {
    state: Arc<Mutex<NotificationPollingState>>,
}

#[derive(Default)]
struct NotificationPollingState {
    next_allowed_at: Option<tokio::time::Instant>,
}

impl NotificationPolling {
    async fn wait_until_allowed(&self) {
        loop {
            let wait = {
                let state = self.state.lock().await;
                let Some(next_allowed_at) = state.next_allowed_at else {
                    return;
                };
                let now = tokio::time::Instant::now();
                if next_allowed_at <= now {
                    return;
                }
                next_allowed_at - now
            };
            tokio::time::sleep(wait).await;
        }
    }

    async fn record_headers(&self, headers: &HeaderMap) {
        let mut state = self.state.lock().await;
        if let Some(poll_interval) = header_duration(headers, "x-poll-interval") {
            state.next_allowed_at = Some(tokio::time::Instant::now() + poll_interval);
        }
    }
}

struct RateLimitBackoff {
    delay: Duration,
    reason: &'static str,
}

impl RateLimitBackoff {
    fn from_response(
        status: StatusCode,
        headers: &HeaderMap,
        body: &str,
        attempt: usize,
    ) -> Option<Self> {
        if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
            return None;
        }

        if let Some(delay) = header_duration(headers, "retry-after") {
            return Some(Self {
                delay,
                reason: "retry-after",
            });
        }

        if header_u64(headers, "x-ratelimit-remaining") == Some(0)
            && let Some(reset_epoch) = header_u64(headers, "x-ratelimit-reset")
        {
            return Some(Self {
                delay: delay_until_epoch(reset_epoch) + Duration::from_secs(1),
                reason: "primary",
            });
        }

        let lower_body = body.to_ascii_lowercase();
        if lower_body.contains("secondary rate limit") {
            return Some(Self {
                delay: secondary_backoff(attempt),
                reason: "secondary",
            });
        }

        if lower_body.contains("rate limit") {
            return Some(Self {
                delay: Duration::from_secs(60),
                reason: "rate-limit",
            });
        }

        None
    }
}

fn header_duration(headers: &HeaderMap, name: &str) -> Option<Duration> {
    header_u64(headers, name).map(Duration::from_secs)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn delay_until_epoch(epoch_seconds: u64) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Duration::from_secs(epoch_seconds.saturating_sub(now))
}

fn secondary_backoff(attempt: usize) -> Duration {
    let multiplier = 1_u64 << attempt.min(4);
    Duration::from_secs(60 * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rate_converts_to_spacing() {
        let rate = GitHubApiRequestRate::per_hour(1_200).unwrap();

        assert_eq!(rate.requests_per_hour(), 1_200);
        assert_eq!(rate.interval(), Duration::from_secs(3));
    }

    #[test]
    fn validates_notification_window() {
        assert!(GitHubNotificationWindow::hours(0).is_err());
        assert_eq!(
            GitHubNotificationWindow::default().as_hours(),
            DEFAULT_GITHUB_NOTIFICATION_WINDOW_HOURS
        );
        assert_eq!(GitHubNotificationWindow::hours(96).unwrap().as_hours(), 96);
    }

    #[test]
    fn request_rate_rejects_zero() {
        assert!(GitHubApiRequestRate::per_hour(0).is_err());
    }

    #[tokio::test]
    async fn skip_policy_records_a_backoff_without_waiting_for_it() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/reactions", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served = requests.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buffer = [0; 4096];
                let _ = stream.read(&mut buffer).await;
                let body = "You have exceeded a secondary rate limit.";
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let client = GitHubRestClient::new("token");

        let reaction = PostReaction { content: "eyes" };

        for _ in 0..2 {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                client.post_json_with(&url, &reaction, RateLimitPolicy::Skip),
            )
            .await
            .expect("a skipped request must not wait out a backoff");
            assert!(result.is_err());
        }

        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!client.traffic.backoff_delay().await.is_zero());
    }

    #[test]
    fn secondary_backoff_is_bounded_exponential() {
        assert_eq!(secondary_backoff(0), Duration::from_secs(60));
        assert_eq!(secondary_backoff(1), Duration::from_secs(120));
        assert_eq!(secondary_backoff(4), Duration::from_secs(960));
        assert_eq!(secondary_backoff(5), Duration::from_secs(960));
    }

    #[test]
    fn builds_reaction_page_urls() {
        assert_eq!(
            reaction_page_url("https://api.github.com/repos/o/r/issues/1/reactions", 2),
            "https://api.github.com/repos/o/r/issues/1/reactions?per_page=100&page=2"
        );
    }

    #[test]
    fn builds_recent_participating_notification_page_urls() {
        assert_eq!(
            notification_page_url("2026-06-08T04:00:00Z", 2),
            "https://api.github.com/notifications?participating=true&all=true&per_page=50&page=2&since=2026-06-08T04:00:00Z"
        );
    }

    #[test]
    fn lists_one_page_of_open_pull_requests_for_each_order() {
        let repo = RepoSlug {
            owner: "some-org".to_string(),
            repo: "maid".to_string(),
        };

        assert_eq!(
            open_pull_requests_url(&repo, OpenPullRequestOrder::NewestCreated),
            "https://api.github.com/repos/some-org/maid/pulls?state=open&sort=created&direction=desc&per_page=50"
        );
        assert_eq!(
            open_pull_requests_url(&repo, OpenPullRequestOrder::RecentlyUpdated),
            "https://api.github.com/repos/some-org/maid/pulls?state=open&sort=updated&direction=desc&per_page=50"
        );
    }

    #[test]
    fn reads_requested_teams_from_a_pull_request_list_payload() {
        let open = GitHubRestClient::open_pull_request_from_api(
            serde_json::from_str(PULL_REQUEST_WITH_TEAM_FIXTURE).unwrap(),
        )
        .unwrap();

        assert_eq!(open.pull_request.author, "octocat");
        assert_eq!(open.pull_request.number, 7);
        assert_eq!(
            open.requested_teams,
            vec![TeamSlug::parse("some-org/eng-infra").unwrap()]
        );
        assert_eq!(
            open.created_at,
            Some(
                DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        assert_eq!(
            open.updated_at,
            Some(
                DateTime::parse_from_rfc3339("2026-10-21T18:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );

        let missing_teams = GitHubRestClient::open_pull_request_from_api(
            serde_json::from_str(PULL_REQUEST_WITHOUT_TEAMS_FIXTURE).unwrap(),
        )
        .unwrap();
        assert!(missing_teams.requested_teams.is_empty());

        let null_teams = GitHubRestClient::open_pull_request_from_api(
            serde_json::from_str(PULL_REQUEST_WITH_NULL_TEAMS_FIXTURE).unwrap(),
        )
        .unwrap();
        assert!(null_teams.requested_teams.is_empty());

        let dropped = GitHubRestClient::open_pull_request_from_api(
            serde_json::from_str(PULL_REQUEST_WITH_UNUSABLE_TEAM_FIXTURE).unwrap(),
        )
        .unwrap();
        assert_eq!(
            dropped.requested_teams,
            vec![TeamSlug::parse("some-org/docs").unwrap()]
        );
        assert!(dropped.created_at.is_some());
        assert!(dropped.updated_at.is_none());
    }

    #[test]
    fn builds_public_pull_request_author_search_urls() {
        let url = Url::parse(&public_pull_request_search_url("Dionysuzx", 2).unwrap()).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(url.path(), "/search/issues");
        assert_eq!(
            query.get("q").map(|value| value.as_ref()),
            Some("is:pr is:open author:Dionysuzx")
        );
        assert_eq!(
            query.get("sort").map(|value| value.as_ref()),
            Some("created")
        );
        assert_eq!(query.get("order").map(|value| value.as_ref()), Some("desc"));
        assert_eq!(
            query.get("per_page").map(|value| value.as_ref()),
            Some("100")
        );
        assert_eq!(query.get("page").map(|value| value.as_ref()), Some("2"));
    }

    #[test]
    fn public_author_discovery_requires_explicitly_public_base_repository() {
        for visibility in [None, Some(ApiRepoVisibility::NonPublic)] {
            assert!(
                public_pull_request_url(api_pull_request_search_item(), &api_repo(visibility))
                    .unwrap()
                    .is_none()
            );
        }

        let public_repo = api_repo(Some(ApiRepoVisibility::Public));
        assert_eq!(
            public_pull_request_url(api_pull_request_search_item(), &public_repo)
                .unwrap()
                .as_deref(),
            Some("https://api.github.com/repos/o/r/pulls/1")
        );
    }

    #[tokio::test]
    async fn mention_for_does_not_fetch_a_pull_request_as_a_comment() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served = Arc::clone(&requests);
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut stream, _)) = listener.accept().await {
                served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buffer = [0; 1024];
                let _ = stream.read(&mut buffer).await;
                let body = "{}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let pr_url = format!("http://{address}/repos/dionysuzx/maid/pulls/52");
        let notification = Notification {
            id: "1".to_string(),
            reason: "comment".to_string(),
            subject_kind: "PullRequest".to_string(),
            subject_url: Some(pr_url.clone()),
            latest_comment_url: Some(pr_url),
            unread: true,
            updated_at: "2026-06-08T04:00:00Z".to_string(),
        };

        let mention = GitHubRestClient::new("token")
            .mention_for(&notification)
            .await
            .unwrap();

        assert!(mention.is_none());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn keeps_recent_comments_across_issue_and_review_comment_streams() {
        let mut comments = (0..21)
            .map(|index| {
                api_comment(
                    &format!("https://api.github.com/repos/o/r/issues/comments/{index}"),
                    &format!("https://github.com/o/r/pull/1#issuecomment-{index}"),
                    &format!("2026-06-08T00:00:{index:02}Z"),
                )
            })
            .collect::<Vec<_>>();
        comments.push(api_comment(
            "https://api.github.com/repos/o/r/pulls/comments/99",
            "https://github.com/o/r/pull/1#discussion_r99",
            "2026-06-08T00:00:21Z",
        ));

        let recent = recent_comments(comments, 20);

        assert_eq!(recent.len(), 20);
        assert!(
            !recent
                .iter()
                .any(|comment| comment.url.ends_with("/issues/comments/0"))
        );
        assert_eq!(
            recent.last().map(|comment| comment.url.as_str()),
            Some("https://api.github.com/repos/o/r/pulls/comments/99")
        );
    }

    fn api_comment(url: &str, html_url: &str, created_at: &str) -> ApiComment {
        ApiComment {
            url: url.to_string(),
            body: "@maid-bot review".to_string(),
            html_url: html_url.to_string(),
            created_at: created_at.to_string(),
            user: ApiUser {
                login: "dionysuzx".to_string(),
            },
            issue_url: None,
            pull_request_url: Some("https://api.github.com/repos/o/r/pulls/1".to_string()),
        }
    }

    fn api_pull_request_search_item() -> ApiIssue {
        ApiIssue {
            url: Some("https://api.github.com/repos/o/r/issues/1".to_string()),
            html_url: Some("https://github.com/o/r/pull/1".to_string()),
            repository_url: Some("https://api.github.com/repos/o/r".to_string()),
            number: Some(1),
            user: Some(ApiUser {
                login: "dionysuzx".to_string(),
            }),
            pull_request: Some(ApiIssuePullRequest {
                url: "https://api.github.com/repos/o/r/pulls/1".to_string(),
            }),
        }
    }

    const PULL_REQUEST_WITH_TEAM_FIXTURE: &str = r#"
{
  "url": "https://api.github.com/repos/some-org/maid/pulls/7",
  "html_url": "https://github.com/some-org/maid/pull/7",
  "number": 7,
  "user": { "login": "octocat" },
  "created_at": "2026-10-01T00:00:00Z",
  "updated_at": "2026-10-21T18:00:00Z",
  "requested_teams": [
    {
      "name": "Engineering Infra",
      "slug": "Eng-Infra",
      "html_url": "https://github.com/orgs/Some-Org/teams/eng-infra"
    },
    {
      "name": "Engineering Infra",
      "slug": "eng-infra",
      "html_url": "https://github.com/orgs/Some-Org/teams/eng-infra"
    }
  ],
  "head": { "sha": "0123456789abcdef0123456789abcdef01234567" },
  "base": {
    "repo": {
      "name": "maid",
      "clone_url": "https://github.com/some-org/maid.git",
      "owner": { "login": "some-org" }
    }
  }
}
"#;

    const PULL_REQUEST_WITHOUT_TEAMS_FIXTURE: &str = r#"
{
  "url": "https://api.github.com/repos/some-org/maid/pulls/7",
  "html_url": "https://github.com/some-org/maid/pull/7",
  "number": 7,
  "user": { "login": "octocat" },
  "created_at": "2026-10-21T18:00:00Z",
  "updated_at": "2026-10-21T18:00:00Z",
  "head": { "sha": "0123456789abcdef0123456789abcdef01234567" },
  "base": {
    "repo": {
      "name": "maid",
      "clone_url": "https://github.com/some-org/maid.git",
      "owner": { "login": "some-org" }
    }
  }
}
"#;

    const PULL_REQUEST_WITH_NULL_TEAMS_FIXTURE: &str = r#"
{
  "url": "https://api.github.com/repos/some-org/maid/pulls/7",
  "html_url": "https://github.com/some-org/maid/pull/7",
  "number": 7,
  "user": { "login": "octocat" },
  "created_at": "2026-10-21T18:00:00Z",
  "updated_at": "2026-10-21T18:00:00Z",
  "requested_teams": null,
  "head": { "sha": "0123456789abcdef0123456789abcdef01234567" },
  "base": {
    "repo": {
      "name": "maid",
      "clone_url": "https://github.com/some-org/maid.git",
      "owner": { "login": "some-org" }
    }
  }
}
"#;

    const PULL_REQUEST_WITH_UNUSABLE_TEAM_FIXTURE: &str = r#"
{
  "url": "https://api.github.com/repos/some-org/maid/pulls/7",
  "html_url": "https://github.com/some-org/maid/pull/7",
  "number": 7,
  "user": { "login": "octocat" },
  "created_at": "2026-10-01T00:00:00Z",
  "updated_at": "not-a-timestamp",
  "requested_teams": [
    {
      "slug": "eng-infra",
      "html_url": "https://example.com/not-a-team"
    },
    {
      "slug": "docs",
      "html_url": "https://github.com/orgs/Some-Org/teams/docs"
    }
  ],
  "head": { "sha": "0123456789abcdef0123456789abcdef01234567" },
  "base": {
    "repo": {
      "name": "maid",
      "clone_url": "https://github.com/some-org/maid.git",
      "owner": { "login": "some-org" }
    }
  }
}
"#;

    fn api_repo(visibility: Option<ApiRepoVisibility>) -> ApiRepo {
        ApiRepo {
            name: "r".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
            owner: Some(ApiUser {
                login: "o".to_string(),
            }),
            default_branch: Some("main".to_string()),
            visibility,
        }
    }
}

impl GitHubRestClient {
    async fn target_for_notification(
        &self,
        notification: &Notification,
    ) -> Result<Option<WorkTarget>> {
        if let Some(subject_url) = notification.subject_url.as_deref() {
            return self.target_for_subject_url(subject_url).await;
        }

        let Some(comment_url) = comment_url(notification) else {
            return Ok(None);
        };
        let comment = self.get::<ApiComment>(comment_url).await?;
        self.target_for_comment_notification(notification, &comment)
            .await
    }

    async fn target_for_comment_notification(
        &self,
        notification: &Notification,
        comment: &ApiComment,
    ) -> Result<Option<WorkTarget>> {
        if let Some(pull_request_url) = comment.pull_request_url.as_deref() {
            return Ok(Some(WorkTarget::PullRequest(
                self.pull_request_from_url(pull_request_url).await?,
            )));
        }
        if let Some(subject_url) = notification.subject_url.as_deref() {
            return self.target_for_subject_url(subject_url).await;
        }
        if let Some(issue_url) = comment.issue_url.as_deref() {
            let issue = self.get::<ApiIssue>(issue_url).await?;
            return self.target_from_issue(issue).await;
        }

        Ok(None)
    }

    async fn target_for_subject_url(&self, subject_url: &str) -> Result<Option<WorkTarget>> {
        if subject_url.contains("/pulls/") {
            return Ok(Some(WorkTarget::PullRequest(
                self.pull_request_from_url(subject_url).await?,
            )));
        }
        if subject_url.contains("/issues/") {
            let issue = self.get::<ApiIssue>(subject_url).await?;
            return self.target_from_issue(issue).await;
        }

        Ok(None)
    }

    async fn target_from_issue(&self, issue: ApiIssue) -> Result<Option<WorkTarget>> {
        if let Some(pull_request) = issue.pull_request.as_ref() {
            return Ok(Some(WorkTarget::PullRequest(
                self.pull_request_from_url(&pull_request.url).await?,
            )));
        }

        Ok(Some(WorkTarget::Issue(self.issue_from_api(issue).await?)))
    }

    async fn issue_comments_for_pr(&self, pr: &PullRequest) -> Result<Vec<ApiComment>> {
        let mut comments = Vec::new();
        for page in 1.. {
            let url = format!(
                "https://api.github.com/repos/{}/{}/issues/{}/comments?per_page=100&page={}",
                pr.owner, pr.repo, pr.number, page
            );
            let mut page_comments = self.get::<Vec<ApiComment>>(&url).await?;
            let done = page_comments.len() < 100;
            comments.append(&mut page_comments);
            if done {
                return Ok(comments);
            }
        }

        Ok(comments)
    }

    async fn issue_comments_for_issue(&self, issue: &Issue) -> Result<Vec<ApiComment>> {
        let mut comments = Vec::new();
        for page in 1.. {
            let url = format!(
                "https://api.github.com/repos/{}/{}/issues/{}/comments?per_page=100&page={}",
                issue.owner, issue.repo, issue.number, page
            );
            let mut page_comments = self.get::<Vec<ApiComment>>(&url).await?;
            let done = page_comments.len() < 100;
            comments.append(&mut page_comments);
            if done {
                return Ok(comments);
            }
        }

        Ok(comments)
    }

    async fn review_comments_for_pr(&self, pr: &PullRequest) -> Result<Vec<ApiComment>> {
        let mut comments = Vec::new();
        for page in 1.. {
            let url = format!(
                "https://api.github.com/repos/{}/{}/pulls/{}/comments?per_page=100&page={}",
                pr.owner, pr.repo, pr.number, page
            );
            let mut page_comments = self.get::<Vec<ApiComment>>(&url).await?;
            let done = page_comments.len() < 100;
            comments.append(&mut page_comments);
            if done {
                return Ok(comments);
            }
        }

        Ok(comments)
    }

    async fn pull_request_from_url(&self, pr_url: &str) -> Result<PullRequest> {
        let pr = self.get::<ApiPullRequest>(pr_url).await?;
        Ok(Self::open_pull_request_from_api(pr)?.pull_request)
    }

    fn open_pull_request_from_api(pr: ApiPullRequest) -> Result<OpenPullRequest> {
        let Some(owner) = pr.base.repo.owner else {
            return Err(anyhow!("pull request base repo has no owner"));
        };

        let pull_request = PullRequest {
            owner: owner.login,
            repo: pr.base.repo.name,
            number: pr.number,
            author: pr.user.login,
            api_url: pr.url,
            html_url: pr.html_url,
            clone_url: pr.base.repo.clone_url,
            head: CommitSha::parse(&pr.head.sha)?,
        };
        let created_at = timestamp_from_api(&pr.created_at);
        let updated_at = timestamp_from_api(&pr.updated_at);
        if updated_at.is_none() {
            warn!(
                pr = %pull_request.html_url,
                updated_at = %pr.updated_at,
                "pull request updated_at could not be parsed; team review will skip it"
            );
        }

        Ok(OpenPullRequest {
            requested_teams: requested_teams_from_api(pr.requested_teams),
            created_at,
            updated_at,
            pull_request,
        })
    }

    async fn issue_from_api(&self, issue: ApiIssue) -> Result<Issue> {
        let repository_url = issue
            .repository_url
            .ok_or_else(|| anyhow!("issue has no repository URL"))?;
        let repo = self.get::<ApiRepo>(&repository_url).await?;
        let owner = repo
            .owner
            .ok_or_else(|| anyhow!("issue repository has no owner"))?;
        let default_branch = repo
            .default_branch
            .ok_or_else(|| anyhow!("issue repository has no default branch"))?;

        Ok(Issue {
            owner: owner.login,
            repo: repo.name,
            number: issue.number.ok_or_else(|| anyhow!("issue has no number"))?,
            author: issue
                .user
                .ok_or_else(|| anyhow!("issue has no author"))?
                .login,
            api_url: issue.url.ok_or_else(|| anyhow!("issue has no URL"))?,
            html_url: issue
                .html_url
                .ok_or_else(|| anyhow!("issue has no HTML URL"))?,
            clone_url: repo.clone_url,
            default_branch,
        })
    }

    async fn mention_has_reaction(
        &self,
        mention: &CommentMention,
        bot_login: &str,
        content: &str,
    ) -> Result<bool> {
        self.reactions_include(
            &format!("{}/reactions", mention.api_url),
            bot_login,
            content,
        )
        .await
    }

    async fn add_reaction(&self, mention: &CommentMention, content: &str) -> Result<()> {
        self.add_reaction_to_api_url(&mention.api_url, content)
            .await
    }

    async fn add_reaction_to_api_url(&self, api_url: &str, content: &str) -> Result<()> {
        let url = format!("{api_url}/reactions");
        self.post_json(&url, &PostReaction { content }).await
    }

    async fn reactions_include(
        &self,
        reactions_url: &str,
        bot_login: &str,
        content: &str,
    ) -> Result<bool> {
        for page in 1.. {
            let reactions = self
                .get::<Vec<ApiReaction>>(&reaction_page_url(reactions_url, page))
                .await?;
            let done = reactions.len() < 100;
            if reactions.into_iter().any(|reaction| {
                reaction.content == content && reaction.user.login.eq_ignore_ascii_case(bot_login)
            }) {
                return Ok(true);
            }
            if done {
                return Ok(false);
            }
        }

        Ok(false)
    }
}

/// The pull request API URL of a search hit whose base repository is public.
fn public_pull_request_url(item: ApiIssue, repo: &ApiRepo) -> Result<Option<String>> {
    if repo.visibility != Some(ApiRepoVisibility::Public) {
        return Ok(None);
    }

    Ok(Some(
        item.pull_request
            .ok_or_else(|| anyhow!("pull request search result has no pull request URL"))?
            .url,
    ))
}

fn reaction_page_url(reactions_url: &str, page: u64) -> String {
    format!("{reactions_url}?per_page=100&page={page}")
}

fn open_pull_requests_url(repo: &RepoSlug, order: OpenPullRequestOrder) -> String {
    // GitHub defaults sort=updated to ascending, so direction=desc is required
    // for the team-review page to be the most recently updated pull requests.
    let sort = match order {
        OpenPullRequestOrder::NewestCreated => "created",
        OpenPullRequestOrder::RecentlyUpdated => "updated",
    };
    format!(
        "https://api.github.com/repos/{}/{}/pulls?state=open&sort={sort}&direction=desc&per_page=50",
        repo.owner, repo.repo
    )
}

fn timestamp_from_api(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn requested_teams_from_api(teams: Option<Vec<ApiRequestedTeam>>) -> Vec<TeamSlug> {
    let mut parsed = Vec::new();
    for team in teams.unwrap_or_default() {
        let Some(html_url) = team
            .html_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        else {
            warn!(slug = team.slug, "ignoring requested team without html_url");
            continue;
        };

        match TeamSlug::from_github_team(&team.slug, html_url) {
            Ok(slug) if !parsed.contains(&slug) => parsed.push(slug),
            Ok(_) => {}
            Err(err) => {
                warn!(
                    slug = team.slug,
                    html_url,
                    error = %err,
                    "ignoring requested team"
                );
            }
        }
    }
    parsed
}

fn notification_page_url(since: &str, page: usize) -> String {
    format!(
        "https://api.github.com/notifications?participating=true&all=true&per_page={NOTIFICATION_PAGE_SIZE}&page={page}&since={since}"
    )
}

fn public_pull_request_search_url(author: &str, page: usize) -> Result<String> {
    let mut url = Url::parse("https://api.github.com/search/issues")?;
    url.query_pairs_mut()
        .append_pair("q", &format!("is:pr is:open author:{author}"))
        .append_pair("sort", "created")
        .append_pair("order", "desc")
        .append_pair("per_page", &SEARCH_PAGE_SIZE.to_string())
        .append_pair("page", &page.to_string());
    Ok(url.into())
}

fn recent_notification_since(window: GitHubNotificationWindow) -> String {
    (Utc::now() - ChronoDuration::hours(i64::from(window.as_hours())))
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn recent_comments(mut comments: Vec<ApiComment>, limit: usize) -> Vec<ApiComment> {
    comments.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.url.cmp(&right.url))
    });
    let start = comments.len().saturating_sub(limit);
    comments.into_iter().skip(start).collect()
}

// GitHub sets latest_comment_url to the subject URL when the notification has no comment.
fn comment_url(notification: &Notification) -> Option<&str> {
    let url = notification.latest_comment_url.as_deref()?;
    let (parent, id) = url.rsplit_once('/')?;
    (parent.ends_with("/comments") && !id.is_empty()).then_some(url)
}

fn comment_mention(comment: ApiComment, target: &WorkTarget) -> CommentMention {
    CommentMention {
        author: comment.user.login,
        body: comment.body,
        api_url: comment.url,
        html_url: comment.html_url,
        target: target.clone(),
    }
}

#[derive(Debug, Deserialize)]
struct ApiNotification {
    id: String,
    reason: String,
    unread: bool,
    updated_at: String,
    subject: ApiNotificationSubject,
}

#[derive(Debug, Deserialize)]
struct ApiNotificationSubject {
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
    latest_comment_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiComment {
    url: String,
    body: String,
    html_url: String,
    created_at: String,
    user: ApiUser,
    issue_url: Option<String>,
    pull_request_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct ApiReaction {
    content: String,
    user: ApiUser,
}

#[derive(Debug, Deserialize)]
struct ApiIssue {
    url: Option<String>,
    html_url: Option<String>,
    repository_url: Option<String>,
    number: Option<u64>,
    user: Option<ApiUser>,
    pull_request: Option<ApiIssuePullRequest>,
}

#[derive(Debug, Deserialize)]
struct ApiIssuePullRequest {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ApiIssueSearch {
    total_count: usize,
    incomplete_results: bool,
    items: Vec<ApiIssue>,
}

#[derive(Debug, Deserialize)]
struct ApiPullRequest {
    url: String,
    html_url: String,
    number: u64,
    user: ApiUser,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    requested_teams: Option<Vec<ApiRequestedTeam>>,
    head: ApiCommitRef,
    base: ApiPullRequestBase,
}

#[derive(Debug, Deserialize)]
struct ApiCommitRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct ApiGitCommit {
    tree: ApiGitObject,
}

#[derive(Debug, Deserialize)]
struct ApiGitObject {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct ApiRequestedTeam {
    slug: String,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiPullRequestBase {
    repo: ApiRepo,
}

#[derive(Debug, Deserialize)]
struct ApiRepo {
    name: String,
    clone_url: String,
    owner: Option<ApiUser>,
    default_branch: Option<String>,
    visibility: Option<ApiRepoVisibility>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ApiRepoVisibility {
    Public,
    #[serde(other)]
    NonPublic,
}

#[derive(Debug, Serialize)]
struct PostComment<'a> {
    body: &'a str,
}

#[derive(Debug, Serialize)]
struct PostReaction<'a> {
    content: &'a str,
}
