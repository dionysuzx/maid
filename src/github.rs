use crate::{
    domain::{CommentMention, Notification, PullRequest},
    maid::GithubClient,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, time::Duration};

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
        let pr_url = if let Some(pull_request_url) = comment.pull_request_url.clone() {
            pull_request_url
        } else if let Some(issue_url) = comment.issue_url.clone() {
            let issue = self.get::<ApiIssue>(&issue_url).await?;
            match issue.pull_request {
                Some(pull_request) => pull_request.url,
                None => return Ok(None),
            }
        } else {
            return Ok(None);
        };

        let pr = self.get::<ApiPullRequest>(&pr_url).await?;
        let Some(owner) = pr.base.repo.owner else {
            return Err(anyhow!("pull request base repo has no owner"));
        };

        Ok(Some(CommentMention {
            author: comment.user.login,
            body: comment.body,
            api_url: comment_url.to_string(),
            html_url: comment.html_url,
            pr: PullRequest {
                owner: owner.login,
                repo: pr.base.repo.name,
                number: pr.number,
                api_url: pr.url,
                html_url: pr.html_url,
                clone_url: pr.base.repo.clone_url,
            },
        }))
    }

    async fn post_pr_comment(&self, pr: &PullRequest, body: &str) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/comments",
            pr.owner, pr.repo, pr.number
        );
        self.post_json(&url, &PostComment { body }).await
    }

    async fn mention_has_handled_marker(
        &self,
        mention: &CommentMention,
        bot_login: &str,
    ) -> Result<bool> {
        self.mention_has_reaction(mention, bot_login, HANDLED_REACTION)
            .await
    }

    async fn mark_mention_started(&self, mention: &CommentMention) -> Result<()> {
        self.add_reaction(mention, STARTED_REACTION).await
    }

    async fn mark_mention_handled(&self, mention: &CommentMention) -> Result<()> {
        self.add_reaction(mention, HANDLED_REACTION).await
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
