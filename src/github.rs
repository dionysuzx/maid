use crate::{
    domain::{CommentMention, Issue, Notification, PullRequest, RepoSlug},
    maid::{GithubClient, ReviewState},
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::{
    net::IpAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const STARTED_REACTION: &str = "eyes";
const HANDLED_REACTION: &str = "+1";

#[derive(Clone)]
pub struct GitHubRestClient {
    client: Client,
    token: String,
}

impl GitHubRestClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_api_ip(token, None)
    }

    pub fn with_api_ip(token: impl Into<String>, api_ip: Option<IpAddr>) -> Self {
        let mut builder = Client::builder().timeout(Duration::from_secs(30));
        if let Some(api_ip) = api_ip {
            builder = builder.resolve("api.github.com", (api_ip, 443).into());
        }

        Self {
            client: builder
                .build()
                .expect("GitHub HTTP client configuration should be valid"),
            token: token.into(),
        }
    }

    async fn get<T>(&self, url: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.request(Method::GET, url, Option::<&()>::None).await
    }

    async fn post_json<T>(&self, url: &str, body: &T) -> Result<()>
    where
        T: Serialize + Sync,
    {
        let _: serde::de::IgnoredAny = self.request(Method::POST, url, Some(body)).await?;
        Ok(())
    }

    async fn request<T, B>(&self, method: Method, url: &str, body: Option<&B>) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + Sync + ?Sized,
    {
        let method_for_error = method.clone();
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "maid");
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("{method_for_error} {url} failed"))?;

        let status = response.status();
        if status == StatusCode::NO_CONTENT || status == StatusCode::RESET_CONTENT {
            return serde_json::from_str("null").context("failed to decode empty GitHub response");
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "{method_for_error} {url} returned {status}: {body}"
            ));
        }

        response
            .json::<T>()
            .await
            .with_context(|| format!("invalid JSON from {method_for_error} {url}"))
    }
}

#[async_trait]
impl GithubClient for GitHubRestClient {
    async fn notifications(&self) -> Result<Vec<Notification>> {
        let notifications = self
            .get::<Vec<ApiNotification>>(
                "https://api.github.com/notifications?participating=true&per_page=50",
            )
            .await?;

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

    async fn recent_labeled_issues(
        &self,
        repo: &RepoSlug,
        label: &str,
        since: SystemTime,
    ) -> Result<Vec<Issue>> {
        let repo_details = self.repo_details(repo).await?;
        let mut url = Url::parse(&format!(
            "https://api.github.com/repos/{}/{}/issues",
            repo.owner, repo.repo
        ))?;
        let since = github_timestamp(since)?;
        url.query_pairs_mut()
            .append_pair("state", "open")
            .append_pair("labels", label)
            .append_pair("since", &since)
            .append_pair("sort", "updated")
            .append_pair("direction", "desc")
            .append_pair("per_page", "50");

        let issues = self.get::<Vec<ApiIssueListItem>>(url.as_str()).await?;
        Ok(issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none())
            .map(|issue| Issue {
                owner: repo.owner.clone(),
                repo: repo.repo.clone(),
                number: issue.number,
                author: issue.user.login,
                title: issue.title,
                body: issue.body.unwrap_or_default(),
                api_url: issue.url,
                html_url: issue.html_url,
                clone_url: repo_details.clone_url.clone(),
                default_branch: repo_details.default_branch.clone(),
            })
            .collect())
    }

    async fn post_pr_comment(&self, pr: &PullRequest, body: &str) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/comments",
            pr.owner, pr.repo, pr.number
        );
        self.post_json(&url, &PostComment { body }).await
    }

    async fn post_issue_comment(&self, issue: &Issue, body: &str) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/comments",
            issue.owner, issue.repo, issue.number
        );
        self.post_json(&url, &PostComment { body }).await
    }

    async fn open_pull_request_for_branch(
        &self,
        issue: &Issue,
        branch: &str,
    ) -> Result<Option<PullRequest>> {
        let mut url = Url::parse(&format!(
            "https://api.github.com/repos/{}/{}/pulls",
            issue.owner, issue.repo
        ))?;
        let head = format!("{}:{branch}", issue.owner);
        url.query_pairs_mut()
            .append_pair("state", "open")
            .append_pair("head", &head)
            .append_pair("base", &issue.default_branch)
            .append_pair("per_page", "10");
        let pull_requests = self.get::<Vec<ApiPullRequest>>(url.as_str()).await?;
        pull_requests
            .into_iter()
            .next()
            .map(Self::pull_request_from_api)
            .transpose()
    }

    async fn create_pull_request(
        &self,
        issue: &Issue,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/pulls",
            issue.owner, issue.repo
        );
        let pr = self
            .request::<ApiPullRequest, _>(
                Method::POST,
                &url,
                Some(&CreatePullRequest {
                    title,
                    head: branch,
                    base: &issue.default_branch,
                    body,
                }),
            )
            .await?;
        Self::pull_request_from_api(pr)
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

    async fn issue_state(&self, issue: &Issue, bot_login: &str) -> Result<ReviewState> {
        if self
            .issue_has_reaction(issue, bot_login, HANDLED_REACTION)
            .await?
        {
            Ok(ReviewState::Handled)
        } else {
            Ok(ReviewState::Pending)
        }
    }

    async fn mark_issue_started(&self, issue: &Issue) -> Result<()> {
        self.add_issue_reaction(issue, STARTED_REACTION).await
    }

    async fn mark_issue_handled(&self, issue: &Issue) -> Result<()> {
        self.add_issue_reaction(issue, HANDLED_REACTION).await
    }

    async fn mark_notification_handled(&self, notification: &Notification) -> Result<()> {
        let url = format!(
            "https://api.github.com/notifications/threads/{}",
            notification.id
        );
        let _: serde::de::IgnoredAny = self
            .request(Method::PATCH, &url, Option::<&()>::None)
            .await?;
        Ok(())
    }
}

impl GitHubRestClient {
    async fn repo_details(&self, repo: &RepoSlug) -> Result<ApiRepoDetails> {
        let url = format!("https://api.github.com/repos/{}/{}", repo.owner, repo.repo);
        self.get(&url).await
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
        let reactions = self
            .get::<Vec<ApiReaction>>(&format!("{}/reactions?per_page=100", mention.api_url))
            .await?;

        Ok(reactions.into_iter().any(|reaction| {
            reaction.content == content && reaction.user.login.eq_ignore_ascii_case(bot_login)
        }))
    }

    async fn add_reaction(&self, mention: &CommentMention, content: &str) -> Result<()> {
        let url = format!("{}/reactions", mention.api_url);
        self.post_json(&url, &PostReaction { content }).await
    }

    async fn pr_has_reaction(
        &self,
        pr: &PullRequest,
        bot_login: &str,
        content: &str,
    ) -> Result<bool> {
        let reactions = self
            .get::<Vec<ApiReaction>>(&format!("{}?per_page=100", self.pr_reactions_url(pr)))
            .await?;

        Ok(reactions.into_iter().any(|reaction| {
            reaction.content == content && reaction.user.login.eq_ignore_ascii_case(bot_login)
        }))
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

    async fn issue_has_reaction(
        &self,
        issue: &Issue,
        bot_login: &str,
        content: &str,
    ) -> Result<bool> {
        let reactions = self
            .get::<Vec<ApiReaction>>(&format!("{}?per_page=100", self.issue_reactions_url(issue)))
            .await?;

        Ok(reactions.into_iter().any(|reaction| {
            reaction.content == content && reaction.user.login.eq_ignore_ascii_case(bot_login)
        }))
    }

    async fn add_issue_reaction(&self, issue: &Issue, content: &str) -> Result<()> {
        self.post_json(&self.issue_reactions_url(issue), &PostReaction { content })
            .await
    }

    fn issue_reactions_url(&self, issue: &Issue) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/issues/{}/reactions",
            issue.owner, issue.repo, issue.number
        )
    }
}

fn github_timestamp(time: SystemTime) -> Result<String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .context("GitHub timestamp is before the Unix epoch")?
        .as_secs();
    let datetime = OffsetDateTime::from_unix_timestamp(seconds as i64)
        .context("failed to convert timestamp for GitHub")?;
    datetime
        .format(&Rfc3339)
        .context("failed to format GitHub timestamp")
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
    body: String,
    html_url: String,
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

#[derive(Debug, Deserialize)]
struct ApiRepoDetails {
    clone_url: String,
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct ApiIssueListItem {
    url: String,
    html_url: String,
    number: u64,
    title: String,
    body: Option<String>,
    user: ApiUser,
    pull_request: Option<ApiIssuePullRequest>,
}

#[derive(Debug, Serialize)]
struct PostComment<'a> {
    body: &'a str,
}

#[derive(Debug, Serialize)]
struct PostReaction<'a> {
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct CreatePullRequest<'a> {
    title: &'a str,
    head: &'a str,
    base: &'a str,
    body: &'a str,
}
