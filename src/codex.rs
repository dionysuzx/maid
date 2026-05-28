use crate::{
    domain::{CodexPromptTemplates, CodexTask},
    maid::{CodexRun, CodexRunMetadata, CodexRunner},
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::{path::Path, process::Stdio};
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::info;

#[derive(Clone, Debug)]
pub struct CodexCli {
    bin: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    prompts: CodexPromptTemplates,
}

impl CodexCli {
    pub fn new(bin: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            model: None,
            reasoning_effort: None,
            prompts: CodexPromptTemplates::default(),
        }
    }

    pub fn with_options(
        bin: impl Into<String>,
        model: Option<String>,
        reasoning_effort: Option<String>,
        prompts: CodexPromptTemplates,
    ) -> Self {
        Self {
            bin: bin.into(),
            model,
            reasoning_effort,
            prompts,
        }
    }
}

#[async_trait]
impl CodexRunner for CodexCli {
    async fn run(&self, checkout: &Path, task: &CodexTask) -> Result<CodexRun> {
        let output_file = NamedTempFile::new().context("failed to create Codex output file")?;
        let output_path = output_file.path().to_path_buf();

        let prompt = task.prompt(&self.prompts)?;

        let mut command = Command::new(&self.bin);
        command.arg("--ask-for-approval").arg("never");
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        if let Some(reasoning_effort) = &self.reasoning_effort {
            command.arg("--config").arg(codex_config_string(
                "model_reasoning_effort",
                reasoning_effort,
            ));
        }
        let mut child = command
            .arg("exec")
            .arg("--color")
            .arg("never")
            .arg("--json")
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

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open Codex stdin"))?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write Codex prompt")?;
        drop(stdin);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to open Codex stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to open Codex stderr"))?;
        let stderr_task = tokio::spawn(async move {
            let mut stderr_bytes = Vec::new();
            stderr.read_to_end(&mut stderr_bytes).await?;
            Ok::<_, std::io::Error>(stderr_bytes)
        });

        let json = CodexJsonEvents::read(stdout, checkout, task).await?;
        let status = child.wait().await.context("Codex failed to run")?;
        let stderr = stderr_task
            .await
            .context("failed to join Codex stderr reader")?
            .context("failed to read Codex stderr")?;
        if !status.success() {
            return Err(anyhow!(
                "Codex exited with {}: {}",
                status,
                String::from_utf8_lossy(&stderr).trim()
            ));
        }

        let response = match tokio::fs::read_to_string(&output_path).await {
            Ok(response) if !response.trim().is_empty() => response,
            Err(_) => json.last_message.unwrap_or_default(),
            Ok(_) => json.last_message.unwrap_or_default(),
        };
        let response = response.trim().to_string();
        if response.is_empty() {
            return Err(anyhow!("Codex produced an empty response"));
        }

        Ok(CodexRun {
            response,
            session_id: json.session_id,
            metadata: Some(CodexRunMetadata {
                model: self.model.clone(),
                reasoning_effort: self.reasoning_effort.clone(),
                prompt,
            }),
        })
    }
}

fn codex_config_string(key: &str, value: &str) -> String {
    format!("{key}={}", toml::Value::String(value.to_string()))
}

#[derive(Debug, Default, Eq, PartialEq)]
struct CodexJsonEvents {
    session_id: Option<String>,
    last_message: Option<String>,
}

impl CodexJsonEvents {
    #[cfg(test)]
    fn parse(stdout: &[u8]) -> Self {
        let mut events = Self::default();
        for line in String::from_utf8_lossy(stdout).lines() {
            events.observe_line(line);
        }
        events
    }

    async fn read(
        stdout: impl AsyncRead + Unpin,
        checkout: &Path,
        task: &CodexTask,
    ) -> Result<Self> {
        let mut events = Self::default();
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .context("failed to read Codex stdout")?
        {
            if let Some(session_id) = events.observe_line(&line) {
                info!(
                    pr = %task.pr_url,
                    trigger = %task.trigger_url(),
                    checkout = %checkout.display(),
                    codex_session_id = %session_id,
                    "codex session started"
                );
            }
        }
        Ok(events)
    }

    fn observe_line(&mut self, line: &str) -> Option<String> {
        let Ok(event) = serde_json::from_str::<CodexJsonEvent>(line) else {
            return None;
        };
        match event {
            CodexJsonEvent::ThreadStarted { thread_id } => {
                let is_first_session = self.session_id.is_none();
                self.session_id = Some(thread_id.clone());
                if is_first_session {
                    Some(thread_id)
                } else {
                    None
                }
            }
            CodexJsonEvent::ItemCompleted { item } => {
                if let CodexJsonItem::AgentMessage { text } = item {
                    self.last_message = Some(text);
                }
                None
            }
            CodexJsonEvent::Other => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexJsonEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexJsonItem },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexJsonItem {
    #[serde(rename = "agent_message")]
    AgentMessage { text: String },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_session_id_and_last_agent_message_from_json_events() {
        let events = CodexJsonEvents::parse(
            br#"{"type":"thread.started","thread_id":"019e64fd-8369-7453-9cdc-4b14b388f618"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"first"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"final"}}
{"type":"turn.completed","usage":{"input_tokens":1}}"#,
        );

        assert_eq!(
            events.session_id.as_deref(),
            Some("019e64fd-8369-7453-9cdc-4b14b388f618")
        );
        assert_eq!(events.last_message.as_deref(), Some("final"));
    }

    #[test]
    fn ignores_non_json_lines_and_unknown_events() {
        let events = CodexJsonEvents::parse(
            br#"not json
{"type":"unknown.event","value":1}
{"type":"item.completed","item":{"id":"item_0","type":"tool_call","text":"ignored"}}"#,
        );

        assert_eq!(events, CodexJsonEvents::default());
    }

    #[test]
    fn formats_codex_config_string_values_as_toml() {
        assert_eq!(
            codex_config_string("model_reasoning_effort", "high"),
            "model_reasoning_effort=\"high\""
        );
    }
}
