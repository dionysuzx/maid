use crate::agent::{Agent, AgentKind, AgentLabel, AgentRun};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::{path::Path, process::Stdio};
use tokio::{io::AsyncWriteExt, process::Command};

/// Runs Claude Code in print mode without permission checks, matching Codex's
/// full-access sandbox.
#[derive(Clone, Debug)]
pub struct ClaudeCli {
    bin: String,
    label: AgentLabel,
}

impl ClaudeCli {
    pub fn new(
        bin: impl Into<String>,
        model: impl Into<String>,
        effort: impl Into<String>,
    ) -> Self {
        Self {
            bin: bin.into(),
            label: AgentLabel {
                kind: AgentKind::Claude,
                model: model.into(),
                effort: effort.into(),
            },
        }
    }
}

#[async_trait]
impl Agent for ClaudeCli {
    fn label(&self) -> &AgentLabel {
        &self.label
    }

    async fn run(&self, worktree: &Path, prompt: &str) -> Result<AgentRun> {
        let mut child = Command::new(&self.bin)
            .arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--dangerously-skip-permissions")
            .arg("--model")
            .arg(&self.label.model)
            .arg("--effort")
            .arg(&self.label.effort)
            .current_dir(worktree)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start {}", self.bin))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open Claude stdin"))?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write Claude prompt")?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .context("Claude failed to run")?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let result = serde_json::from_slice::<ClaudeResult>(&output.stdout)
            .with_context(|| format!("Claude printed no JSON result: {}", stderr.trim()))?;
        let response = result.result.trim();
        if !output.status.success() || result.is_error {
            return Err(anyhow!("Claude exited with {}: {response}", output.status));
        }
        if response.is_empty() {
            return Err(anyhow!("Claude produced an empty response"));
        }

        Ok(AgentRun {
            response: response.to_string(),
            session_id: result.session_id,
        })
    }
}

#[derive(Deserialize)]
struct ClaudeResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: String,
    session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fake_claude(dir: &Path, script: &str) -> String {
        let bin = dir.join("fake-claude");
        fs::write(&bin, script).unwrap();
        let mut permissions = fs::metadata(&bin).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).unwrap();
        bin.display().to_string()
    }

    #[tokio::test]
    async fn run_passes_the_prompt_on_stdin_and_reads_the_json_result() {
        let temp = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            temp.path(),
            r#"#!/bin/sh
printf '%s\n' "$*" > args
cat > prompt
printf '%s' '{"type":"result","subtype":"success","is_error":false,"result":"  review  ","session_id":"session-1"}'
"#,
        );

        let run = ClaudeCli::new(bin, "claude-opus-5-5", "high")
            .run(temp.path(), "please review")
            .await
            .unwrap();

        assert_eq!(
            run,
            AgentRun {
                response: "review".to_string(),
                session_id: Some("session-1".to_string()),
            }
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("args")).unwrap(),
            "--print --output-format json --dangerously-skip-permissions --model claude-opus-5-5 --effort high\n"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("prompt")).unwrap(),
            "please review"
        );
    }

    #[tokio::test]
    async fn run_fails_with_the_error_claude_reports() {
        let temp = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            temp.path(),
            r#"#!/bin/sh
cat >/dev/null
printf '%s' '{"type":"result","is_error":true,"result":"Not logged in"}'
exit 1
"#,
        );

        let err = ClaudeCli::new(bin, "opus", "high")
            .run(temp.path(), "p")
            .await
            .unwrap_err();

        assert!(err.to_string().ends_with(": Not logged in"), "{err}");
    }
}
