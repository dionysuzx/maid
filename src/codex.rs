use crate::{domain::CodexTask, maid::CodexRunner};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::{path::Path, process::Stdio};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct CodexCli {
    bin: String,
}

impl CodexCli {
    pub fn new(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

#[async_trait]
impl CodexRunner for CodexCli {
    async fn run(&self, checkout: &Path, task: &CodexTask) -> Result<String> {
        let output_file = NamedTempFile::new().context("failed to create Codex output file")?;
        let output_path = output_file.path().to_path_buf();

        let mut child = Command::new(&self.bin)
            .arg("--ask-for-approval")
            .arg("never")
            .arg("exec")
            .arg("--color")
            .arg("never")
            .arg("--skip-git-repo-check")
            .arg("--sandbox")
            .arg("danger-full-access")
            .arg("--output-last-message")
            .arg(&output_path)
            .arg("-")
            .current_dir(checkout)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start {}", self.bin))?;

        let prompt = task.prompt();
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open Codex stdin"))?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write Codex prompt")?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .context("Codex failed to run")?;
        if !output.status.success() {
            return Err(anyhow!(
                "Codex exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let response = match tokio::fs::read_to_string(&output_path).await {
            Ok(response) => response,
            Err(_) => String::from_utf8_lossy(&output.stdout).to_string(),
        };
        let response = response.trim().to_string();
        if response.is_empty() {
            return Err(anyhow!("Codex produced an empty response"));
        }

        Ok(response)
    }
}
