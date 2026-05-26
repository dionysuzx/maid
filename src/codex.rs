use crate::{
    domain::CodexTask,
    maid::{CodexRun, CodexRunner},
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
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
    async fn run(&self, checkout: &Path, task: &CodexTask) -> Result<CodexRun> {
        let output_file = NamedTempFile::new().context("failed to create Codex output file")?;
        let output_path = output_file.path().to_path_buf();

        let mut child = Command::new(&self.bin)
            .arg("--ask-for-approval")
            .arg("never")
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

        let json = CodexJsonEvents::parse(&output.stdout);
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
        })
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct CodexJsonEvents {
    session_id: Option<String>,
    last_message: Option<String>,
}

impl CodexJsonEvents {
    fn parse(stdout: &[u8]) -> Self {
        let mut events = Self::default();
        for line in String::from_utf8_lossy(stdout).lines() {
            let Ok(event) = serde_json::from_str::<CodexJsonEvent>(line) else {
                continue;
            };
            match event {
                CodexJsonEvent::ThreadStarted { thread_id } => {
                    events.session_id = Some(thread_id);
                }
                CodexJsonEvent::ItemCompleted { item } => {
                    if let CodexJsonItem::AgentMessage { text } = item {
                        events.last_message = Some(text);
                    }
                }
                CodexJsonEvent::Other => {}
            }
        }
        events
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
}
