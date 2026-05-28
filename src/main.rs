use anyhow::Result;
use maid::{
    codex::CodexCli, config::Config, github::GitHubRestClient, maid::Maid,
    repo_cache::GitRepoCache, task_limit::FileTaskStartRecorder,
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
