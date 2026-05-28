use anyhow::Result;
use axum::{Router, routing::get};
use maid::{
    codex::CodexCli,
    config::{Config, ImplementationCommitIdentity, ImplementationGitAuth},
    github::GitHubRestClient,
    maid::{Maid, MaidSettings},
    repo_cache::{GitRepoCache, IssueCommitIdentity, IssueGitAuth},
    task_limit::FileTaskStartRecorder,
};
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{debug, error, info};
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

    let maid = Maid::new(
        bot_github,
        implementation_github,
        GitRepoCache::new(
            config.cache_dir.clone(),
            config.github_token.clone(),
            config.bot_login.clone(),
        )
        .with_issue_publish_mode(issue_git_auth, issue_commit_identity),
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

    let app = Router::new().route("/healthz", get(|| async { "ok" }));
    let listener = TcpListener::bind(config.bind_addr).await?;
    let poll_interval = config.poll_interval;
    let poller = tokio::spawn(async move {
        loop {
            match maid.run_once().await {
                Ok(report) => {
                    let skip_breakdown = &report.skip_breakdown;
                    if report.has_actionable_result() {
                        info!(
                            seen = report.seen,
                            responded = report.responded,
                            failed = report.failed,
                            skipped = report.skipped,
                            skip_duplicate_notification = skip_breakdown.duplicate_notification,
                            skip_non_pr_notification = skip_breakdown.non_pr_notification,
                            skip_missing_mention = skip_breakdown.missing_mention,
                            skip_self_authored_mention = skip_breakdown.self_authored_mention,
                            skip_non_master_mention = skip_breakdown.non_master_mention,
                            skip_no_bot_request = skip_breakdown.no_bot_request,
                            skip_already_handled_mention = skip_breakdown.already_handled_mention,
                            skip_self_authored_pr = skip_breakdown.self_authored_pr,
                            skip_auto_review_disabled = skip_breakdown.auto_review_disabled,
                            skip_already_handled_pr = skip_breakdown.already_handled_pr,
                            skip_self_authored_issue = skip_breakdown.self_authored_issue,
                            skip_auto_implement_disabled = skip_breakdown.auto_implement_disabled,
                            skip_already_handled_issue = skip_breakdown.already_handled_issue,
                            skip_existing_issue_pr = skip_breakdown.existing_issue_pr,
                            skip_issue_without_changes = skip_breakdown.issue_without_changes,
                            next_poll_seconds = poll_interval.as_secs(),
                            "poll completed with actionable result"
                        );
                    } else {
                        debug!(
                            seen = report.seen,
                            skipped = report.skipped,
                            skip_duplicate_notification = skip_breakdown.duplicate_notification,
                            skip_non_pr_notification = skip_breakdown.non_pr_notification,
                            skip_missing_mention = skip_breakdown.missing_mention,
                            skip_self_authored_mention = skip_breakdown.self_authored_mention,
                            skip_non_master_mention = skip_breakdown.non_master_mention,
                            skip_no_bot_request = skip_breakdown.no_bot_request,
                            skip_already_handled_mention = skip_breakdown.already_handled_mention,
                            skip_self_authored_pr = skip_breakdown.self_authored_pr,
                            skip_auto_review_disabled = skip_breakdown.auto_review_disabled,
                            skip_already_handled_pr = skip_breakdown.already_handled_pr,
                            skip_self_authored_issue = skip_breakdown.self_authored_issue,
                            skip_auto_implement_disabled = skip_breakdown.auto_implement_disabled,
                            skip_already_handled_issue = skip_breakdown.already_handled_issue,
                            skip_existing_issue_pr = skip_breakdown.existing_issue_pr,
                            skip_issue_without_changes = skip_breakdown.issue_without_changes,
                            next_poll_seconds = poll_interval.as_secs(),
                            "poll completed without actionable work"
                        );
                    }
                }
                Err(err) => error!(error = ?err, "poll failed"),
            }
            tokio::time::sleep(poll_interval).await;
        }
    });

    info!(
        addr = %config.bind_addr,
        cache_dir = %config.cache_dir.display(),
        task_start_ledger = %config.task_start_ledger_path.display(),
        poll_seconds = config.poll_interval.as_secs(),
        task_limit_per_24h = ?config.task_limit_per_24h,
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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    poller.abort();

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
