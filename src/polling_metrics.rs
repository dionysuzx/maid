use anyhow::{Context, Result};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const METRIC_NAME: &str = "maid_last_successful_poll_timestamp_seconds";

#[derive(Clone, Debug, Default)]
pub struct PollingMetrics {
    last_successful_poll: Arc<AtomicU64>,
}

impl PollingMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_success(&self) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after the Unix epoch")
            .as_secs();
        self.last_successful_poll
            .store(timestamp, Ordering::Relaxed);
    }

    fn render(&self) -> String {
        let timestamp = self.last_successful_poll.load(Ordering::Relaxed);
        let mut body = format!(
            "# HELP {METRIC_NAME} Unix timestamp of Maid's last successful GitHub poll.\n\
             # TYPE {METRIC_NAME} gauge\n"
        );
        if timestamp != 0 {
            body.push_str(&format!("{METRIC_NAME} {timestamp}\n"));
        }
        body
    }
}

pub struct PollingMetricsEndpoint {
    listener: TcpListener,
    metrics: PollingMetrics,
}

impl PollingMetricsEndpoint {
    pub async fn bind(address: SocketAddr, metrics: PollingMetrics) -> Result<Self> {
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("failed to bind metrics endpoint to {address}"))?;
        Ok(Self { listener, metrics })
    }

    pub async fn serve(self) -> Result<()> {
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .context("failed to accept metrics connection")?;
            let metrics = self.metrics.clone();
            tokio::spawn(async move {
                let _ = respond(stream, metrics).await;
            });
        }
    }
}

async fn respond(mut stream: TcpStream, metrics: PollingMetrics) -> Result<()> {
    let mut request = [0_u8; 1024];
    let length = stream
        .read(&mut request)
        .await
        .context("failed to read metrics request")?;
    let is_metrics_request = request[..length].starts_with(b"GET /metrics ");
    let (status, body) = if is_metrics_request {
        ("200 OK", metrics.render())
    } else {
        ("404 Not Found", "not found\n".to_string())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write metrics response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omits_sample_before_a_successful_poll() {
        let rendered = PollingMetrics::new().render();

        assert!(rendered.contains(&format!("# TYPE {METRIC_NAME} gauge")));
        assert!(!rendered.lines().any(|line| line.starts_with(METRIC_NAME)));
    }

    #[test]
    fn renders_the_last_successful_poll_timestamp() {
        let metrics = PollingMetrics::new();
        metrics.last_successful_poll.store(42, Ordering::Relaxed);

        assert!(metrics.render().contains(&format!("{METRIC_NAME} 42\n")));
    }
}
