use crate::{
    domain::{CodexPromptTemplates, CodexTask},
    maid::{CodexRun, CodexRunner},
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{info, warn};

const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct CodexCli {
    bin: String,
    model: String,
    reasoning_effort: String,
    prompts: CodexPromptTemplates,
}

impl CodexCli {
    pub fn new(
        bin: impl Into<String>,
        model: impl Into<String>,
        reasoning_effort: impl Into<String>,
        prompts: CodexPromptTemplates,
    ) -> Self {
        Self {
            bin: bin.into(),
            model: model.into(),
            reasoning_effort: reasoning_effort.into(),
            prompts,
        }
    }

    pub fn with_options(
        bin: impl Into<String>,
        model: impl Into<String>,
        reasoning_effort: impl Into<String>,
        prompts: CodexPromptTemplates,
    ) -> Self {
        Self::new(bin, model, reasoning_effort, prompts)
    }
}

#[async_trait]
impl CodexRunner for CodexCli {
    async fn run(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun> {
        let output_file = NamedTempFile::new().context("failed to create Codex output file")?;
        let output_path = output_file.path().to_path_buf();

        let prompt = task.prompt(&self.prompts)?;

        let mut command = Command::new(&self.bin);
        command.arg("--ask-for-approval").arg("never");
        command.arg("--model").arg(&self.model);
        command.arg("--config").arg(codex_config_string(
            "model_reasoning_effort",
            &self.reasoning_effort,
        ));
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
            .current_dir(worktree)
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
        let mut stderr_task = tokio::spawn(async move {
            let mut stderr_bytes = Vec::new();
            stderr.read_to_end(&mut stderr_bytes).await?;
            Ok::<_, std::io::Error>(stderr_bytes)
        });

        let events = Arc::new(Mutex::new(CodexJsonEvents::default()));
        let mut stdout_task = tokio::spawn(read_codex_stdout(
            stdout,
            events.clone(),
            worktree.display().to_string(),
            task.pr_url.clone(),
            task.trigger_url().to_string(),
            task.task_kind(),
        ));
        let status = child.wait().await.context("Codex failed to run")?;
        match timeout(PIPE_DRAIN_TIMEOUT, &mut stdout_task).await {
            Ok(result) => result.context("failed to join Codex stdout reader")??,
            Err(_) => {
                stdout_task.abort();
                warn!("Codex stdout did not close after process exit; using captured events");
            }
        }
        let stderr = match timeout(PIPE_DRAIN_TIMEOUT, &mut stderr_task).await {
            Ok(result) => result
                .context("failed to join Codex stderr reader")?
                .context("failed to read Codex stderr")?,
            Err(_) => {
                stderr_task.abort();
                warn!("Codex stderr did not close after process exit");
                Vec::new()
            }
        };
        if !status.success() {
            return Err(anyhow!(
                "Codex exited with {}: {}",
                status,
                String::from_utf8_lossy(&stderr).trim()
            ));
        }

        let json = events.lock().await.clone();
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

async fn read_codex_stdout(
    stdout: impl AsyncRead + Unpin,
    events: Arc<Mutex<CodexJsonEvents>>,
    worktree: String,
    pr_url: String,
    trigger_url: String,
    task_kind: &'static str,
) -> Result<()> {
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read Codex stdout")?
    {
        let session_id = events.lock().await.observe_line(&line);
        if let Some(session_id) = session_id {
            info!(
                pr = %pr_url,
                trigger = %trigger_url,
                task_kind,
                worktree,
                codex_session_id = %session_id,
                "codex session started"
            );
        }
    }
    Ok(())
}

fn codex_config_string(key: &str, value: &str) -> String {
    format!("{key}={}", toml::Value::String(value.to_string()))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
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
    use crate::domain::{CodexPromptTemplates, CodexTask, CodexTaskOrigin};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tokio::time::timeout;

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

    #[tokio::test]
    async fn run_finishes_when_child_leaves_stdout_open_after_exit() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("fake-codex");
        fs::write(
            &bin,
            r#"#!/bin/sh
output_path=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "--output-last-message" ]; then
    output_path="$argument"
  fi
  previous="$argument"
done

cat >/dev/null
printf '%s\n' '{"type":"thread.started","thread_id":"session-1"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"json final"}}'
printf '%s\n' 'file final' > "$output_path"
(sleep 5) &
exit 0
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&bin).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).unwrap();

        let codex = CodexCli::new(
            bin.display().to_string(),
            "test-model",
            "low",
            CodexPromptTemplates {
                mention: "{{cleaned_text}}".to_string(),
                pull_request_opened: "{{author}}".to_string(),
                operator_mention: "{{request_text}}".to_string(),
            },
        );
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot test".to_string(),
                cleaned_text: "test".to_string(),
            },
        };

        let run = timeout(Duration::from_secs(3), codex.run(temp.path(), &task))
            .await
            .expect("Codex run should not wait for inherited stdout forever")
            .unwrap();

        assert_eq!(run.response, "file final");
        assert_eq!(run.session_id.as_deref(), Some("session-1"));
    }
}
