use anyhow::Result;
use maid::{
    codex::CodexCli,
    config::{Config, ImplementationCommitIdentity, ImplementationGitAuth},
    github::GitHubRestClient,
    maid::{Maid, MaidSettings},
    repo_cache::{
        ExpectedGitIdentity as ExpectedRepoGitIdentity, GitRepoCache, IssueCommitIdentity,
        IssueGitAuth,
    },
    task_limit::FileTaskStartRecorder,
};
use std::time::Duration;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env()?;
    let bot_github =
        GitHubRestClient::with_api_ip(config.github_token.clone(), config.github_api_ip);
    let implementation_github = GitHubRestClient::with_api_ip(
        config.implementation_actor.github_token.clone(),
        config.github_api_ip,
    );
    let issue_git_auth = match config.implementation_actor.git_auth {
        ImplementationGitAuth::Bot => IssueGitAuth::Bot,
        ImplementationGitAuth::Host => IssueGitAuth::Host,
    };
    let issue_commit_identity = match config.implementation_actor.commit_identity {
        ImplementationCommitIdentity::Bot => IssueCommitIdentity::Bot,
        ImplementationCommitIdentity::Host => IssueCommitIdentity::Host,
    };
    let expected_git_identity = config
        .implementation_actor
        .expected_git_identity
        .clone()
        .map(|identity| ExpectedRepoGitIdentity {
            name: identity.name,
            email: identity.email,
            gpgsign: identity.gpgsign,
            gpg_format: identity.gpg_format,
        });

    let maid = Maid::new(
        bot_github,
        implementation_github,
        GitRepoCache::new(
            config.cache_dir.clone(),
            config.github_token.clone(),
            config.bot_login.clone(),
        )
        .with_issue_publish_mode(issue_git_auth, issue_commit_identity)
        .with_expected_git_identity(expected_git_identity),
        CodexCli::new(config.codex_bin.clone()),
        MaidSettings {
            bot_login: config.bot_login.clone(),
            master_accounts: config.master_accounts.clone(),
            auto_review_accounts: config.auto_review_accounts.clone(),
            auto_review_repos: config.auto_review_repos.clone(),
            auto_implement_accounts: config.auto_implement_accounts.clone(),
            auto_implement_repos: config.auto_implement_repos.clone(),
            auto_implement_label: config.auto_implement_label.clone(),
            auto_implement_window: Duration::from_secs(
                config.auto_implement_window_days * 24 * 60 * 60,
            ),
        },
    )
    .with_task_start_recorder(FileTaskStartRecorder::new(
        config.task_limit_per_24h,
        config.task_start_ledger_path.clone(),
    ));

    let poll_interval = config.poll_interval;
    let task_limit_per_24h = config
        .task_limit_per_24h
        .map_or_else(|| "none".to_string(), |limit| limit.to_string());
    let poller = tokio::spawn(async move {
        loop {
            match maid.run_once().await {
                Ok(report) => info!(
                    seen = report.seen,
                    skipped = report.skipped,
                    responded = report.responded,
                    failed = report.failed,
                    "poll complete"
                ),
                Err(err) => error!(error = ?err, "poll failed"),
            }
            tokio::time::sleep(poll_interval).await;
        }
    });

    info!(
        cache_dir = %config.cache_dir.display(),
        task_start_ledger = %config.task_start_ledger_path.display(),
        poll_seconds = config.poll_interval.as_secs(),
        task_limit_per_24h = %task_limit_per_24h,
        master_accounts = config.master_accounts.len(),
        auto_review_accounts = config.auto_review_accounts.len(),
        auto_review_repos = config.auto_review_repos.len(),
        auto_implement_accounts = config.auto_implement_accounts.len(),
        auto_implement_repos = config.auto_implement_repos.len(),
        auto_implement_label = %config.auto_implement_label,
        auto_implement_window_days = config.auto_implement_window_days,
        implementation_actor = %config.implementation_actor.login,
        implementation_git_auth = ?config.implementation_actor.git_auth,
        implementation_commit_identity = ?config.implementation_actor.commit_identity,
        "maid started"
    );

    shutdown_signal().await;
    poller.abort();
    let _ = poller.await;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl-c");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to listen for terminate signal")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }

    tokio::time::sleep(Duration::from_millis(50)).await;
}
