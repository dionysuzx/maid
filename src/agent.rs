use crate::jev;
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use std::{fmt, path::Path, sync::Arc};
use tokio::process::Command;

/// Environment variables that hold Maid's own credentials.
const MAID_SECRETS: &[&str] = &[jev::API_KEY_ENV];

/// Starts an agent CLI without Maid's credentials in its environment, since
/// agents run commands from the pull requests they review.
pub fn agent_command(bin: &str) -> Command {
    let mut command = Command::new(bin);
    for secret in MAID_SECRETS {
        command.env_remove(secret);
    }
    command
}

#[async_trait]
pub trait Agent: Send + Sync {
    fn label(&self) -> &AgentLabel;
    async fn run(&self, worktree: &Path, prompt: &str) -> Result<AgentRun>;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Codex,
    Claude,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn resume_command(self, session_id: &str) -> String {
        match self {
            Self::Codex => format!("codex resume --include-non-interactive --all {session_id}"),
            Self::Claude => format!("claude --resume {session_id}"),
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLabel {
    pub kind: AgentKind,
    pub model: String,
    pub effort: String,
}

impl fmt::Display for AgentLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} · {} · {}",
            self.kind, self.model, self.effort
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRun {
    pub response: String,
    pub session_id: Option<String>,
}

/// The agents that answer every review and mention in parallel.
#[derive(Clone)]
pub struct AgentPanel {
    agents: Vec<Arc<dyn Agent>>,
}

impl AgentPanel {
    /// Kinds must be unique because each kind gets its own worktree for a task.
    pub fn new(agents: Vec<Arc<dyn Agent>>) -> Result<Self> {
        if agents.is_empty() {
            bail!("at least one agent is required");
        }
        for (index, agent) in agents.iter().enumerate() {
            let kind = agent.label().kind;
            if agents[..index]
                .iter()
                .any(|earlier| earlier.label().kind == kind)
            {
                bail!("agents must have unique kinds; {kind} is listed twice");
            }
        }

        Ok(Self { agents })
    }

    pub fn all(&self) -> &[Arc<dyn Agent>] {
        &self.agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LabelOnly(AgentLabel);

    #[async_trait]
    impl Agent for LabelOnly {
        fn label(&self) -> &AgentLabel {
            &self.0
        }

        async fn run(&self, _worktree: &Path, _prompt: &str) -> Result<AgentRun> {
            unreachable!("panel tests do not run agents")
        }
    }

    fn agent(kind: AgentKind) -> Arc<dyn Agent> {
        Arc::new(LabelOnly(AgentLabel {
            kind,
            model: "model".to_string(),
            effort: "high".to_string(),
        }))
    }

    #[test]
    fn agent_commands_drop_maids_credentials() {
        let command = agent_command("codex");

        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(name, value)| name == "TYPESAFE_API_KEY" && value.is_none())
        );
    }

    #[test]
    fn panel_requires_one_agent_of_each_listed_kind() {
        assert!(AgentPanel::new(Vec::new()).is_err());
        assert!(AgentPanel::new(vec![agent(AgentKind::Claude), agent(AgentKind::Codex)]).is_ok());
        assert_eq!(
            AgentPanel::new(vec![agent(AgentKind::Codex), agent(AgentKind::Codex)])
                .err()
                .unwrap()
                .to_string(),
            "agents must have unique kinds; codex is listed twice"
        );
    }
}
