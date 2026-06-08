use crate::{
    domain::{CommentMention, Notification, PullRequest, RepoSlug, ReviewState},
    maid::GithubClient,
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode, header::HeaderMap};
use serde::{Deserialize, Serialize};
use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use tracing::warn;

const STARTED_REACTION: &str = "eyes";
const HANDLED_REACTION: &str = "+1";
pub const DEFAULT_GITHUB_API_REQUESTS_PER_HOUR: u32 = 1_200;
const MAX_RATE_LIMIT_RETRIES: usize = 5;
const PARTICIPATING_NOTIFICATIONS_URL: &str =
    "https://api.github.com/notifications?participating=true&all=true&per_page=50";

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
}

impl GitHubRestClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_api_ip(token, None)
    }

    pub fn with_api_ip(token: impl Into<String>, api_ip: Option<IpAddr>) -> Self {
        Self::with_options(token, api_ip, GitHubApiRequestRate::default())
    }

    pub fn with_options(
        token: impl Into<String>,
        api_ip: Option<IpAddr>,
        request_rate: GitHubApiRequestRate,
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
        }
    }

    async fn get<T>(&self, url: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.request(Method::GET, url, Option::<&()>::None, HeaderMap::new())
            .await?
            .into_json()
    }

    async fn post_json<T>(&self, url: &str, body: &T) -> Result<()>
    where
        T: Serialize + Sync,
    {
        let _: serde::de::IgnoredAny = self
            .request(Method::POST, url, Some(body), HeaderMap::new())
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
    ) -> Result<GitHubResponse<T>>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + Sync + ?Sized,
    {
        let method_for_error = method.clone();
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            self.traffic.wait_for_turn().await;

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
                if attempt == MAX_RATE_LIMIT_RETRIES {
                    return Err(anyhow!(
                        "{method_for_error} {url} still rate limited after {MAX_RATE_LIMIT_RETRIES} retries: {response_body}"
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
                self.traffic.back_off(backoff.delay).await;
                continue;
            }

            return Err(anyhow!(
                "{method_for_error} {url} returned {status}: {response_body}"
            ));
        }

        Err(anyhow!(
            "{method_for_error} {url} still rate limited after {MAX_RATE_LIMIT_RETRIES} retries"
        ))
    }
}

#[async_trait]
impl GithubClient for GitHubRestClient {
    async fn notifications(&self) -> Result<Vec<Notification>> {
        self.notifications.wait_until_allowed().await;

        let response = self
            .request::<Vec<ApiNotification>, _>(
                Method::GET,
                PARTICIPATING_NOTIFICATIONS_URL,
                Option::<&()>::None,
                HeaderMap::new(),
            )
            .await?;
        self.notifications.record_headers(response.headers()).await;

        let notifications = response.into_json()?;

        Ok(notifications
            .into_iter()
            .map(|notification| Notification {
                id: notification.id,
                reason: notification.reason,
                subject_kind: notification.subject.kind,
                subject_url: notification.subject.url,
                latest_comment_url: notification.subject.latest_comment_url,
            })
            .collect())
    }

    async fn mention_for(&self, notification: &Notification) -> Result<Option<CommentMention>> {
        let Some(comment_url) = notification.latest_comment_url.as_deref() else {
            return Ok(None);
        };

        let comment = self.get::<ApiComment>(comment_url).await?;
        let pr_url = if let Some(pull_request_url) = comment.pull_request_url {
            pull_request_url
        } else if let Some(subject_url) = notification.subject_url.as_deref() {
            subject_url.to_string()
        } else if let Some(issue_url) = comment.issue_url {
            let issue = self.get::<ApiIssue>(&issue_url).await?;
            match issue.pull_request {
                Some(pull_request) => pull_request.url,
                None => return Ok(None),
            }
        } else {
            return Ok(None);
        };

        let pr = self.pull_request_from_url(&pr_url).await?;

        Ok(Some(CommentMention {
            author: comment.user.login,
            body: comment.body,
            api_url: comment_url.to_string(),
            html_url: comment.html_url,
            pr,
        }))
    }

    async fn mentions_for(&self, notification: &Notification) -> Result<Vec<CommentMention>> {
        let Some(pr_url) = self.pr_url_for_notification(notification).await? else {
            return Ok(Vec::new());
        };

        let pr = self.pull_request_from_url(&pr_url).await?;
        let mut comments = self.issue_comments_for_pr(&pr).await?;
        comments.append(&mut self.review_comments_for_pr(&pr).await?);

        Ok(recent_comments(comments, 20)
            .into_iter()
            .map(|comment| comment_mention(comment, &pr))
            .collect())
    }

    async fn open_pull_requests(&self, repo: &RepoSlug) -> Result<Vec<PullRequest>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/pulls?state=open&sort=created&direction=desc&per_page=50",
            repo.owner, repo.repo
        );
        let pull_requests = self.get::<Vec<ApiPullRequest>>(&url).await?;
        pull_requests
            .into_iter()
            .map(Self::pull_request_from_api)
            .collect()
    }

    async fn post_pr_comment(&self, pr: &PullRequest, body: &str) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/comments",
            pr.owner, pr.repo, pr.number
        );
        self.post_json(&url, &PostComment { body }).await
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

    async fn mark_mention_api_url_handled(&self, api_url: &str) -> Result<()> {
        self.add_reaction_to_api_url(api_url, HANDLED_REACTION)
            .await
    }

    async fn pr_state(&self, pr: &PullRequest, bot_login: &str) -> Result<ReviewState> {
        if self
            .pr_has_reaction(pr, bot_login, HANDLED_REACTION)
            .await?
        {
            Ok(ReviewState::Handled)
        } else {
            Ok(ReviewState::Pending)
        }
    }

    async fn mark_pr_started(&self, pr: &PullRequest) -> Result<()> {
        self.add_pr_reaction(pr, STARTED_REACTION).await
    }

    async fn mark_pr_handled(&self, pr: &PullRequest) -> Result<()> {
        self.add_pr_reaction(pr, HANDLED_REACTION).await
    }

    async fn mark_pull_request_html_url_handled(&self, html_url: &str) -> Result<()> {
        let (owner, repo, number) = parse_pull_request_html_url(html_url)?;
        let url = format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}/reactions");
        self.post_json(
            &url,
            &PostReaction {
                content: HANDLED_REACTION,
            },
        )
        .await
    }

    async fn mark_notification_handled(&self, notification: &Notification) -> Result<()> {
        let url = format!(
            "https://api.github.com/notifications/threads/{}",
            notification.id
        );
        let _: serde::de::IgnoredAny = self
            .request(Method::PATCH, &url, Option::<&()>::None, HeaderMap::new())
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

    async fn wait_for_turn(&self) {
        loop {
            let wait = self.backoff_delay().await;
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
                continue;
            }

            let wait = self.request_slot_delay().await;
            if wait.is_zero() {
                return;
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
    fn request_rate_rejects_zero() {
        assert!(GitHubApiRequestRate::per_hour(0).is_err());
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
    fn polls_recent_participating_notifications_including_read_ones() {
        assert_eq!(
            PARTICIPATING_NOTIFICATIONS_URL,
            "https://api.github.com/notifications?participating=true&all=true&per_page=50"
        );
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
}

impl GitHubRestClient {
    async fn pr_url_for_notification(&self, notification: &Notification) -> Result<Option<String>> {
        if let Some(subject_url) = notification.subject_url.as_deref() {
            return Ok(Some(subject_url.to_string()));
        }

        let Some(comment_url) = notification.latest_comment_url.as_deref() else {
            return Ok(None);
        };
        let comment = self.get::<ApiComment>(comment_url).await?;
        if let Some(pull_request_url) = comment.pull_request_url {
            return Ok(Some(pull_request_url));
        }
        if let Some(issue_url) = comment.issue_url {
            let issue = self.get::<ApiIssue>(&issue_url).await?;
            return Ok(issue.pull_request.map(|pull_request| pull_request.url));
        }

        Ok(None)
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
        Self::pull_request_from_api(pr)
    }

    fn pull_request_from_api(pr: ApiPullRequest) -> Result<PullRequest> {
        let Some(owner) = pr.base.repo.owner else {
            return Err(anyhow!("pull request base repo has no owner"));
        };

        Ok(PullRequest {
            owner: owner.login,
            repo: pr.base.repo.name,
            number: pr.number,
            author: pr.user.login,
            api_url: pr.url,
            html_url: pr.html_url,
            clone_url: pr.base.repo.clone_url,
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

    async fn pr_has_reaction(
        &self,
        pr: &PullRequest,
        bot_login: &str,
        content: &str,
    ) -> Result<bool> {
        self.reactions_include(&self.pr_reactions_url(pr), bot_login, content)
            .await
    }

    async fn add_pr_reaction(&self, pr: &PullRequest, content: &str) -> Result<()> {
        self.post_json(&self.pr_reactions_url(pr), &PostReaction { content })
            .await
    }

    fn pr_reactions_url(&self, pr: &PullRequest) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/issues/{}/reactions",
            pr.owner, pr.repo, pr.number
        )
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

fn reaction_page_url(reactions_url: &str, page: u64) -> String {
    format!("{reactions_url}?per_page=100&page={page}")
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

fn comment_mention(comment: ApiComment, pr: &PullRequest) -> CommentMention {
    CommentMention {
        author: comment.user.login,
        body: comment.body,
        api_url: comment.url,
        html_url: comment.html_url,
        pr: pr.clone(),
    }
}

fn parse_pull_request_html_url(html_url: &str) -> Result<(&str, &str, u64)> {
    let Some(path) = html_url.strip_prefix("https://github.com/") else {
        bail!("pull request URL must start with https://github.com/: {html_url}");
    };
    let mut parts = path.split('/');
    let owner = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| anyhow!("pull request URL is missing owner: {html_url}"))?;
    let repo = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| anyhow!("pull request URL is missing repo: {html_url}"))?;
    match parts.next() {
        Some("pull") => {}
        _ => bail!("pull request URL must contain /pull/: {html_url}"),
    }
    let number = parts
        .next()
        .ok_or_else(|| anyhow!("pull request URL is missing number: {html_url}"))?
        .parse()
        .with_context(|| format!("pull request URL has invalid number: {html_url}"))?;
    if parts.next().is_some() {
        bail!("pull request URL has unexpected path after number: {html_url}");
    }

    Ok((owner, repo, number))
}

#[derive(Debug, Deserialize)]
struct ApiNotification {
    id: String,
    reason: String,
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
    pull_request: Option<ApiIssuePullRequest>,
}

#[derive(Debug, Deserialize)]
struct ApiIssuePullRequest {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ApiPullRequest {
    url: String,
    html_url: String,
    number: u64,
    user: ApiUser,
    base: ApiPullRequestBase,
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
}

#[derive(Debug, Serialize)]
struct PostComment<'a> {
    body: &'a str,
}

#[derive(Debug, Serialize)]
struct PostReaction<'a> {
    content: &'a str,
}
