use anyhow::Result;
use axum::{Router, routing::get};
use maid::{
    codex::CodexCli, config::Config, github::GitHubRestClient, maid::Maid, repo_cache::GitRepoCache,
};
use std::time::Duration;
use tokio::net::TcpListener;
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
    );

    let app = Router::new().route("/healthz", get(|| async { "ok" }));
    let listener = TcpListener::bind(config.bind_addr).await?;
    let poll_interval = config.poll_interval;
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
        addr = %config.bind_addr,
        cache_dir = %config.cache_dir.display(),
        poll_seconds = config.poll_interval.as_secs(),
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
