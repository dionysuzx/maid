use anyhow::Result;
use axum::{Router, routing::get};
use maid::{
    codex::CodexCli, config::Config, github::GitHubRestClient, maid::Maid,
    repo_cache::GitRepoCache, task_limit::FileTaskStartRecorder,
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
    let maid = Maid::new(
        GitHubRestClient::with_api_ip(config.github_token.clone(), config.github_api_ip),
        GitRepoCache::new(config.cache_dir.clone(), config.github_token.clone()),
        CodexCli::new(config.codex_bin.clone()),
        config.bot_login.clone(),
        config.master_accounts.clone(),
        config.auto_review_accounts.clone(),
        config.auto_review_repos.clone(),
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
