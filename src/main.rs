use anyhow::Result;
use maid::{
    codex::CodexCli,
    config::Config,
    daemon_lock::DaemonLock,
    github::GitHubRestClient,
    handled_marker::FilePendingHandledMarkerStore,
    maid::Maid,
    observed_notification::FileObservedNotificationStore,
    polling_metrics::{PollingMetrics, PollingMetricsEndpoint},
    task_limit::FileTaskStartRecorder,
    worktree::GitWorktrees,
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
    let _daemon_lock = DaemonLock::acquire(config.daemon_pid_path.clone())?;
    let polling_metrics = PollingMetrics::new();
    let metrics_endpoint =
        PollingMetricsEndpoint::bind(config.metrics_bind_address, polling_metrics.clone()).await?;
    let metrics_server = tokio::spawn(metrics_endpoint.serve());
    let maid = Maid::new(
        GitHubRestClient::with_options(
            config.github_token.clone(),
            config.github_api_ip,
            config.github_api_requests_per_hour,
            config.github_notification_window,
        ),
        GitWorktrees::new(config.git_dir.clone(), config.github_token.clone()),
        CodexCli::with_options(
            config.codex_bin.clone(),
            config.codex_model.clone(),
            config.codex_reasoning_effort.clone(),
            config.codex_prompts.clone(),
        ),
        config.bot_login.clone(),
        config.master_accounts.clone(),
        config.auto_review_accounts.clone(),
        config.auto_review_repos.clone(),
    )
    .with_task_start_recorder(FileTaskStartRecorder::new(
        config.task_limit_per_24h,
        config.task_start_ledger_path.clone(),
    ))
    .with_pending_handled_marker_store(FilePendingHandledMarkerStore::new(
        config.pending_handled_marker_ledger_path.clone(),
    ))
    .with_observed_notification_store(FileObservedNotificationStore::new(
        config.observed_notification_ledger_path.clone(),
    ))
    .into_concurrent(config.max_concurrent_requests);

    let task_limit_per_24h = config
        .task_limit_per_24h
        .map_or_else(|| "none".to_string(), |limit| limit.to_string());
    let poller_metrics = polling_metrics.clone();
    let poller = tokio::spawn(async move {
        loop {
            match maid.run_once().await {
                Ok(report) => {
                    poller_metrics.record_success();
                    info!(
                        seen = report.seen,
                        skipped = report.skipped,
                        started = report.started,
                        responded = report.responded,
                        failed = report.failed,
                        in_flight = report.in_flight,
                        "poll complete"
                    )
                }
                Err(err) => error!(error = ?err, "poll failed"),
            }
        }
    });

    info!(
        git_dir = %config.git_dir.display(),
        daemon_pid = %config.daemon_pid_path.display(),
        task_start_ledger = %config.task_start_ledger_path.display(),
        pending_handled_marker_ledger = %config.pending_handled_marker_ledger_path.display(),
        observed_notification_ledger = %config.observed_notification_ledger_path.display(),
        github_api_requests_per_hour = config.github_api_requests_per_hour.requests_per_hour(),
        github_notification_window_hours = config.github_notification_window.as_hours(),
        task_limit_per_24h = %task_limit_per_24h,
        max_concurrent_requests = config.max_concurrent_requests,
        master_accounts = config.master_accounts.len(),
        auto_review_accounts = config.auto_review_accounts.len(),
        auto_review_repos = config.auto_review_repos.len(),
        metrics_bind_address = %config.metrics_bind_address,
        "maid started"
    );

    shutdown_signal().await;
    poller.abort();
    let _ = poller.await;
    metrics_server.abort();
    let _ = metrics_server.await;

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
