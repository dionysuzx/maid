use crate::{
    domain::{CodexExecutionAccess, CodexPromptTemplates, CodexTask},
    maid::{CodexRun, CodexRunner},
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, watch};
use tokio::time::timeout;
use tracing::{info, warn};

const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const CODEX_EXIT_AFTER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
const CODEX_TASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_STDOUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
pub const DEFAULT_WORKER_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

#[derive(Clone, Debug)]
pub struct CodexCli {
    bin: String,
    codex_home: PathBuf,
    runtime_home: PathBuf,
    worker_path: String,
    model: String,
    reasoning_effort: String,
    prompts: CodexPromptTemplates,
    exit_after_completion_timeout: Duration,
    task_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct VerifiedCodexCli(CodexCli);

impl CodexCli {
    pub fn new(
        bin: impl Into<String>,
        codex_home: impl Into<PathBuf>,
        runtime_home: impl Into<PathBuf>,
        model: impl Into<String>,
        reasoning_effort: impl Into<String>,
        prompts: CodexPromptTemplates,
    ) -> Self {
        let bin = bin.into();
        Self {
            bin: resolve_executable(&bin),
            codex_home: codex_home.into(),
            runtime_home: runtime_home.into(),
            worker_path: DEFAULT_WORKER_PATH.to_string(),
            model: model.into(),
            reasoning_effort: reasoning_effort.into(),
            prompts,
            exit_after_completion_timeout: CODEX_EXIT_AFTER_COMPLETION_TIMEOUT,
            task_timeout: CODEX_TASK_TIMEOUT,
        }
    }

    pub fn with_options(
        bin: impl Into<String>,
        codex_home: impl Into<PathBuf>,
        runtime_home: impl Into<PathBuf>,
        model: impl Into<String>,
        reasoning_effort: impl Into<String>,
        prompts: CodexPromptTemplates,
    ) -> Self {
        Self::new(
            bin,
            codex_home,
            runtime_home,
            model,
            reasoning_effort,
            prompts,
        )
    }

    #[cfg(test)]
    fn with_exit_after_completion_timeout(mut self, timeout: Duration) -> Self {
        self.exit_after_completion_timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_task_timeout(mut self, timeout: Duration) -> Self {
        self.task_timeout = timeout;
        self
    }

    pub fn with_worker_path(mut self, worker_path: impl Into<String>) -> Self {
        self.worker_path = worker_path.into();
        self
    }

    pub fn verify(self) -> Result<VerifiedCodexCli> {
        self.verify_worker_isolation()?;
        Ok(VerifiedCodexCli(self))
    }

    fn verify_worker_isolation(&self) -> Result<()> {
        create_private_directory(&self.codex_home)?;
        create_private_directory(&self.runtime_home)?;

        let runtime_home = tempfile::tempdir_in(&self.runtime_home)
            .context("failed to create isolated Codex verification home")?;
        let workspace = tempfile::tempdir_in(&self.runtime_home)
            .context("failed to create Codex verification workspace")?;
        let workspace_marker = NamedTempFile::new_in(workspace.path())
            .context("failed to create Codex verification workspace marker")?;
        let codex_home_marker = NamedTempFile::new_in(&self.codex_home)
            .context("failed to create Codex home isolation marker")?;
        let runtime_root_marker = NamedTempFile::new_in(&self.runtime_home)
            .context("failed to create Codex runtime root isolation marker")?;
        let runtime_home_marker = NamedTempFile::new_in(runtime_home.path())
            .context("failed to create Codex runtime home isolation marker")?;
        let slash_tmp =
            tempfile::tempdir_in("/tmp").context("failed to create /tmp isolation fixture")?;
        let slash_tmp_marker = NamedTempFile::new_in(slash_tmp.path())
            .context("failed to create /tmp isolation marker")?;

        let mut denied_markers = vec![
            ("codex-home", codex_home_marker.path()),
            ("runtime-root", runtime_root_marker.path()),
            ("runtime-home", runtime_home_marker.path()),
            ("slash-tmp", slash_tmp_marker.path()),
        ];
        #[cfg(target_os = "macos")]
        let private_tmp = tempfile::tempdir_in("/private/tmp")
            .context("failed to create /private/tmp isolation fixture")?;
        #[cfg(target_os = "macos")]
        let private_tmp_marker = NamedTempFile::new_in(private_tmp.path())
            .context("failed to create /private/tmp isolation marker")?;
        #[cfg(target_os = "macos")]
        denied_markers.push(("private-tmp", private_tmp_marker.path()));

        let mut failures = Vec::new();
        for access in [CodexExecutionAccess::Inspect, CodexExecutionAccess::Operate] {
            let policy = CodexPolicy::for_access(access);
            let mut command = std::process::Command::new(&self.bin);
            command
                .env_clear()
                .env("HOME", runtime_home.path())
                .env("CODEX_HOME", &self.codex_home);
            for config in policy.config_overrides() {
                command.arg("--config").arg(config);
            }
            command
                .arg("sandbox")
                .arg("--permission-profile")
                .arg("maid-task")
                .arg("--cd")
                .arg(workspace.path())
                .arg("--")
                .arg("/bin/sh")
                .arg("-c")
                .arg(
                    "/bin/cat \"$1\" >/dev/null 2>&1 || exit 20; \
                     shift; unsafe=0; \
                     while [ \"$#\" -gt 0 ]; do \
                       label=\"$1\"; path=\"$2\"; shift 2; \
                       if /bin/cat \"$path\" >/dev/null 2>&1; then \
                         printf '%s\\n' \"$label\" >&2; unsafe=1; \
                       fi; \
                     done; \
                     exit \"$unsafe\"",
                )
                .arg("sandbox-probe")
                .arg(workspace_marker.path());
            for (label, path) in &denied_markers {
                command.arg(label).arg(path);
            }

            let output = command
                .output()
                .context("failed to start Codex worker isolation probe")?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exposed = denied_markers
                .iter()
                .map(|(label, _)| *label)
                .filter(|label| stderr.lines().any(|line| line == *label))
                .collect::<Vec<_>>();
            if !exposed.is_empty() {
                failures.push(format!(
                    "Codex {access:?} worker sandbox could read denied synthetic fixtures: {}",
                    exposed.join(", ")
                ));
            } else if !output.status.success() {
                failures.push(format!(
                    "could not verify Codex {access:?} worker isolation ({}): {}",
                    output.status,
                    stderr.trim()
                ));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(failures.join("; "))
        }
    }
}

#[async_trait]
impl CodexRunner for VerifiedCodexCli {
    async fn run(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun> {
        self.0.run_unverified(worktree, task).await
    }
}

impl CodexCli {
    async fn run_unverified(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun> {
        create_private_directory(&self.codex_home)?;
        create_private_directory(&self.runtime_home)?;
        let runtime_home = tempfile::tempdir_in(&self.runtime_home)
            .context("failed to create isolated Codex runtime home")?;
        let output_file = NamedTempFile::new_in(runtime_home.path())
            .context("failed to create Codex output file")?;
        let output_path = output_file.path().to_path_buf();

        let prompt = task.prompt(&self.prompts)?;

        let mut command = Command::new(&self.bin);
        configure_command(&mut command, self, runtime_home.path(), worktree, task);
        let mut child = command
            .arg("exec")
            .arg("--color")
            .arg("never")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--output-last-message")
            .arg(&output_path)
            .arg("-")
            .current_dir(worktree)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start {}", self.bin))?;
        let mut process_group = ProcessGroupGuard::for_child(&child);

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
        let mut stderr_task =
            tokio::spawn(async move { read_bounded(&mut stderr, MAX_STDERR_BYTES).await });

        let events = Arc::new(Mutex::new(CodexJsonEvents::default()));
        let (completed_tx, mut completed_rx) = watch::channel(false);
        let mut stdout_task = tokio::spawn(read_codex_stdout(
            stdout,
            events.clone(),
            completed_tx,
            worktree.display().to_string(),
            task.pr_url.clone(),
            task.trigger_url().to_string(),
            task.task_kind(),
        ));
        let status = match timeout(
            self.task_timeout,
            wait_for_codex(
                &mut child,
                &mut completed_rx,
                self.exit_after_completion_timeout,
            ),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                terminate_process_group(&mut child).await;
                process_group.disarm();
                stdout_task.abort();
                stderr_task.abort();
                bail!(
                    "Codex task exceeded its {} second time limit",
                    self.task_timeout.as_secs()
                );
            }
        };
        process_group.terminate();
        if status.is_none() {
            let _ = timeout(Duration::from_secs(5), child.wait()).await;
        }
        process_group.disarm();
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
                .context("failed to read bounded Codex stderr")?,
            Err(_) => {
                stderr_task.abort();
                warn!("Codex stderr did not close after process exit");
                Vec::new()
            }
        };
        if let Some(status) = status
            && !status.success()
        {
            return Err(anyhow!(
                "Codex exited with {}: {}",
                status,
                String::from_utf8_lossy(&stderr).trim()
            ));
        }

        let json = events.lock().await.clone();
        let file_response = read_small_output(&output_path).await?;
        let response = match file_response {
            response if !response.trim().is_empty() => response,
            _ => json.last_message.unwrap_or_default(),
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

fn resolve_executable(bin: &str) -> String {
    if Path::new(bin).components().count() > 1 {
        return bin.to_string();
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(bin))
        .find(|candidate| candidate.is_file())
        .map_or_else(
            || bin.to_string(),
            |path| path.to_string_lossy().to_string(),
        )
}

fn configure_command(
    command: &mut Command,
    codex: &CodexCli,
    runtime_home: &Path,
    worktree: &Path,
    task: &CodexTask,
) {
    let policy = CodexPolicy::for_access(task.execution_access());
    command
        .env_clear()
        .env("PATH", &codex.worker_path)
        .env("HOME", runtime_home)
        .env("CODEX_HOME", &codex.codex_home)
        .arg("--strict-config")
        .arg("--model")
        .arg(&codex.model)
        .arg("--config")
        .arg(codex_config_string(
            "model_reasoning_effort",
            &codex.reasoning_effort,
        ))
        .arg("--config")
        .arg("shell_environment_policy.inherit=\"none\"")
        .arg("--config")
        .arg("allow_login_shell=false")
        .arg("--config")
        .arg("web_search=\"disabled\"")
        .arg("--config")
        .arg("project_doc_max_bytes=0")
        .arg("--config")
        .arg("default_permissions=\"maid-task\"")
        .arg("--config")
        .arg(format!(
            "projects.{}.trust_level=\"untrusted\"",
            toml::Value::String(worktree.display().to_string())
        ));
    for config in policy.config_overrides() {
        command.arg("--config").arg(config);
    }
    configure_child_limits(command);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CodexPolicy {
    workspace_access: &'static str,
    approval_policy: &'static str,
    approvals_reviewer: Option<&'static str>,
}

impl CodexPolicy {
    fn for_access(access: CodexExecutionAccess) -> Self {
        match access {
            CodexExecutionAccess::Inspect => Self {
                workspace_access: "read",
                approval_policy: "never",
                approvals_reviewer: None,
            },
            CodexExecutionAccess::Operate => Self {
                workspace_access: "write",
                approval_policy: "on-request",
                approvals_reviewer: Some("auto_review"),
            },
        }
    }

    fn config_overrides(self) -> Vec<String> {
        let mut overrides = vec![
            self.filesystem_config(),
            "permissions.maid-task.network={ enabled = false }".to_string(),
            format!("approval_policy=\"{}\"", self.approval_policy),
        ];
        if let Some(reviewer) = self.approvals_reviewer {
            overrides.push(format!("approvals_reviewer=\"{reviewer}\""));
        }
        overrides
    }

    fn filesystem_config(self) -> String {
        format!(
            "permissions.maid-task.filesystem={{ \":root\" = \"deny\", \":minimal\" = \"read\", \":tmpdir\" = \"deny\", \":slash_tmp\" = \"deny\", \":workspace_roots\" = {{ \".\" = \"{}\" }} }}",
            self.workspace_access
        )
    }
}

async fn wait_for_codex(
    child: &mut Child,
    completed_rx: &mut watch::Receiver<bool>,
    exit_timeout: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    tokio::select! {
        status = child.wait() => Ok(Some(status.context("Codex failed to run")?)),
        completed = async {
            completed_rx.wait_for(|completed| *completed).await.map(|_| ())
        } => {
            completed.context("Codex completion channel closed")?;
            match timeout(exit_timeout, child.wait()).await {
                Ok(status) => Ok(Some(status.context("Codex failed to run")?)),
                Err(_) => {
                    warn!("Codex process group did not exit after task completion; terminating it");
                    Ok(None)
                }
            }
        }
    }
}

async fn read_bounded(reader: &mut (impl AsyncRead + Unpin), limit: usize) -> io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    if exceeded {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process output exceeded limit",
        ));
    }
    Ok(captured)
}

async fn read_small_output(path: &Path) -> Result<String> {
    let metadata = tokio::fs::metadata(path).await?;
    if metadata.len() > MAX_OUTPUT_BYTES {
        bail!("Codex final response exceeded output limit");
    }
    tokio::fs::read_to_string(path)
        .await
        .context("failed to read Codex final response")
}

fn create_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect private directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "private directory must be a real directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure private directory {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn configure_child_limits(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
    unsafe {
        command.pre_exec(|| {
            set_resource_limit(libc::RLIMIT_CPU, 20 * 60)?;
            #[cfg(not(target_os = "macos"))]
            set_resource_limit(libc::RLIMIT_AS, 8 * 1024 * 1024 * 1024)?;
            set_resource_limit(libc::RLIMIT_FSIZE, 1024 * 1024 * 1024)?;
            set_resource_limit(libc::RLIMIT_NOFILE, 256)?;
            set_resource_limit(libc::RLIMIT_CORE, 0)?;
            Ok(())
        });
    }
}

#[cfg(unix)]
#[cfg(target_os = "linux")]
type RlimitResource = libc::__rlimit_resource_t;

#[cfg(unix)]
#[cfg(not(target_os = "linux"))]
type RlimitResource = libc::c_int;

#[cfg(unix)]
fn set_resource_limit(resource: RlimitResource, requested: libc::rlim_t) -> io::Result<()> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe {
        if libc::getrlimit(resource, &mut current) != 0 {
            return Err(io::Error::last_os_error());
        }
        current.rlim_cur = requested.min(current.rlim_max);
        if libc::setrlimit(resource, &current) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_child_limits(_command: &mut Command) {}

#[cfg(unix)]
async fn terminate_process_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

#[cfg(not(unix))]
async fn terminate_process_group(child: &mut Child) {
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

struct ProcessGroupGuard {
    #[cfg(unix)]
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    fn for_child(child: &Child) -> Self {
        Self {
            #[cfg(unix)]
            pid: child.id(),
        }
    }

    fn terminate(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pid = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn read_codex_stdout(
    stdout: impl AsyncRead + Unpin,
    events: Arc<Mutex<CodexJsonEvents>>,
    completed: watch::Sender<bool>,
    worktree: String,
    pr_url: String,
    trigger_url: String,
    task_kind: &'static str,
) -> Result<()> {
    let mut stdout = stdout;
    let mut total: usize = 0;
    let mut exceeded = false;
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stdout
            .read(&mut buffer)
            .await
            .context("failed to read Codex stdout")?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if total > MAX_STDOUT_BYTES {
            exceeded = true;
            continue;
        }
        pending.extend_from_slice(&buffer[..read]);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=newline).collect::<Vec<_>>();
            observe_stdout_line(
                &line,
                &events,
                &completed,
                &worktree,
                &pr_url,
                &trigger_url,
                task_kind,
            )
            .await;
        }
    }
    if !pending.is_empty() {
        observe_stdout_line(
            &pending,
            &events,
            &completed,
            &worktree,
            &pr_url,
            &trigger_url,
            task_kind,
        )
        .await;
    }
    if exceeded {
        bail!("Codex stdout exceeded capture limit");
    }
    Ok(())
}

async fn observe_stdout_line(
    line: &[u8],
    events: &Arc<Mutex<CodexJsonEvents>>,
    completed: &watch::Sender<bool>,
    worktree: &str,
    pr_url: &str,
    trigger_url: &str,
    task_kind: &'static str,
) {
    let Ok(line) = std::str::from_utf8(line) else {
        return;
    };
    let observed = events
        .lock()
        .await
        .observe_line(line.trim_end_matches(['\r', '\n']));
    if observed.completed {
        let _ = completed.send(true);
    }
    if let Some(session_id) = observed.session_id {
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

    fn observe_line(&mut self, line: &str) -> ObservedCodexLine {
        let Ok(event) = serde_json::from_str::<CodexJsonEvent>(line) else {
            return ObservedCodexLine::default();
        };
        match event {
            CodexJsonEvent::ThreadStarted { thread_id } => {
                let is_first_session = self.session_id.is_none();
                self.session_id = Some(thread_id.clone());
                ObservedCodexLine {
                    session_id: is_first_session.then_some(thread_id),
                    completed: false,
                }
            }
            CodexJsonEvent::ItemCompleted { item } => {
                if let CodexJsonItem::AgentMessage { text } = item {
                    self.last_message = Some(text);
                }
                ObservedCodexLine::default()
            }
            CodexJsonEvent::TurnCompleted | CodexJsonEvent::TaskComplete => ObservedCodexLine {
                session_id: None,
                completed: true,
            },
            CodexJsonEvent::Other => ObservedCodexLine::default(),
        }
    }
}

#[derive(Default)]
struct ObservedCodexLine {
    session_id: Option<String>,
    completed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexJsonEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexJsonItem },
    #[serde(rename = "turn.completed")]
    TurnCompleted,
    #[serde(rename = "task_complete")]
    TaskComplete,
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
        let mut events = CodexJsonEvents::default();
        assert!(
            events
                .observe_line(r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#)
                .completed
        );

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

    #[test]
    fn assigns_least_privilege_by_task_kind() {
        assert_eq!(
            CodexPolicy::for_access(CodexExecutionAccess::Inspect),
            CodexPolicy {
                workspace_access: "read",
                approval_policy: "never",
                approvals_reviewer: None,
            }
        );
        assert_eq!(
            CodexPolicy::for_access(CodexExecutionAccess::Operate),
            CodexPolicy {
                workspace_access: "write",
                approval_policy: "on-request",
                approvals_reviewer: Some("auto_review"),
            }
        );
    }

    #[test]
    fn configures_the_actual_worker_environment_and_approval_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let codex = CodexCli::new(
            "codex",
            temp.path().join("codex-home"),
            temp.path().join("runtime-home"),
            "test-model",
            "low",
            CodexPromptTemplates {
                mention: "{{cleaned_text}}".to_string(),
                pull_request_opened: "{{author}}".to_string(),
                operator_mention: "{{request_text}}".to_string(),
            },
        );
        let review = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot review".to_string(),
                cleaned_text: "review".to_string(),
            },
        };
        let operate = CodexTask {
            pr_url: review.pr_url.clone(),
            origin: CodexTaskOrigin::OperatorMention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-3".to_string(),
                raw_body: "@maid-bot /operate fix".to_string(),
                request_text: "fix".to_string(),
                trigger_author: "operator".to_string(),
                bot_login: "maid-bot".to_string(),
            },
        };

        let mut review_command = Command::new("codex");
        configure_command(
            &mut review_command,
            &codex,
            temp.path().join("review-home").as_path(),
            temp.path(),
            &review,
        );
        let review_args = command_arguments(&review_command);
        assert!(
            review_args
                .iter()
                .any(|argument| argument == "approval_policy=\"never\"")
        );
        assert!(!review_args.iter().any(|argument| argument == "--sandbox"));
        assert!(
            !review_args
                .iter()
                .any(|argument| argument == "--ask-for-approval")
        );
        assert_eq!(review_command.as_std().get_envs().count(), 3);
        assert!(
            review_command
                .as_std()
                .get_envs()
                .all(|(key, _)| { matches!(key.to_str(), Some("PATH" | "HOME" | "CODEX_HOME")) })
        );

        let mut operate_command = Command::new("codex");
        configure_command(
            &mut operate_command,
            &codex,
            temp.path().join("operate-home").as_path(),
            temp.path(),
            &operate,
        );
        let operate_args = command_arguments(&operate_command);
        assert!(
            operate_args
                .iter()
                .any(|argument| argument == "approval_policy=\"on-request\"")
        );
        assert!(
            operate_args
                .iter()
                .any(|argument| argument == "approvals_reviewer=\"auto_review\"")
        );
        assert!(
            operate_args
                .iter()
                .any(|argument| argument.contains("\":workspace_roots\" = { \".\" = \"write\" }"))
        );
        assert!(
            !operate_args
                .iter()
                .any(|argument| argument == "--approve-for-me")
        );
        assert!(!operate_args.iter().any(|argument| argument == "--sandbox"));
    }

    #[test]
    #[ignore = "requires an installed Codex CLI and host sandbox support"]
    fn real_codex_worker_isolation_is_verified_or_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let codex_bin =
            std::env::var("MAID_CODEX_BIN").unwrap_or_else(|_| resolve_executable("codex"));
        let codex = CodexCli::new(
            codex_bin,
            temp.path().join("codex-home"),
            temp.path().join("runtime-home"),
            "test-model",
            "low",
            CodexPromptTemplates {
                mention: "{{cleaned_text}}".to_string(),
                pull_request_opened: "{{author}}".to_string(),
                operator_mention: "{{request_text}}".to_string(),
            },
        );

        if let Err(error) = codex.verify_worker_isolation() {
            let error = error.to_string();
            assert!(
                error.contains("could read denied synthetic fixtures"),
                "{error}"
            );
            assert!(error.contains("Codex Inspect"), "{error}");
            assert!(error.contains("Codex Operate"), "{error}");
            assert!(error.contains("slash-tmp"), "{error}");
            #[cfg(target_os = "macos")]
            assert!(error.contains("private-tmp"), "{error}");
            eprintln!("{error}");
        }
    }

    #[test]
    fn verification_rejects_a_worker_that_can_read_denied_fixtures() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("fake-codex");
        fs::write(
            &bin,
            "#!/bin/sh\nprintf 'codex-home\\nruntime-home\\nslash-tmp\\n' >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&bin).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).unwrap();
        let codex = CodexCli::new(
            bin.display().to_string(),
            temp.path().join("codex-home"),
            temp.path().join("runtime-home"),
            "test-model",
            "low",
            CodexPromptTemplates {
                mention: "{{cleaned_text}}".to_string(),
                pull_request_opened: "{{author}}".to_string(),
                operator_mention: "{{request_text}}".to_string(),
            },
        );

        let error = codex.verify().unwrap_err().to_string();
        assert!(error.contains("Codex Inspect"));
        assert!(error.contains("Codex Operate"));
        assert!(error.contains("could read denied synthetic fixtures"));
        assert!(error.contains("codex-home"));
        assert!(error.contains("runtime-home"));
        assert!(error.contains("slash-tmp"));
    }

    fn command_arguments(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect()
    }

    #[tokio::test]
    async fn run_terminates_a_task_that_exceeds_its_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("fake-codex");
        fs::write(&bin, "#!/bin/sh\ncat >/dev/null\nsleep 5\n").unwrap();
        let mut permissions = fs::metadata(&bin).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).unwrap();

        let codex = CodexCli::new(
            bin.display().to_string(),
            temp.path().join("codex-home"),
            temp.path().join("runtime-home"),
            "test-model",
            "low",
            CodexPromptTemplates {
                mention: "{{cleaned_text}}".to_string(),
                pull_request_opened: "{{author}}".to_string(),
                operator_mention: "{{request_text}}".to_string(),
            },
        )
        .with_task_timeout(Duration::from_millis(100));
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot test".to_string(),
                cleaned_text: "test".to_string(),
            },
        };

        let error = codex.run_unverified(temp.path(), &task).await.unwrap_err();
        assert!(error.to_string().contains("time limit"));
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
            temp.path().join("codex-home"),
            temp.path().join("runtime-home"),
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

        let run = timeout(
            Duration::from_secs(3),
            codex.run_unverified(temp.path(), &task),
        )
        .await
        .expect("Codex run should not wait for inherited stdout forever")
        .unwrap();

        assert_eq!(run.response, "file final");
        assert_eq!(run.session_id.as_deref(), Some("session-1"));
    }

    #[tokio::test]
    async fn run_finishes_after_task_completion_when_wrapper_keeps_running() {
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
python3 -c 'import sys; output_path = sys.argv[1]; print("{\"type\":\"thread.started\",\"thread_id\":\"session-1\"}"); print("{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"json final\"}}"); print("{\"type\":\"turn.completed\"}"); sys.stdout.flush(); open(output_path, "w").write("file final\n")' "$output_path"
sleep 5
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&bin).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).unwrap();

        let codex = CodexCli::new(
            bin.display().to_string(),
            temp.path().join("codex-home"),
            temp.path().join("runtime-home"),
            "test-model",
            "low",
            CodexPromptTemplates {
                mention: "{{cleaned_text}}".to_string(),
                pull_request_opened: "{{author}}".to_string(),
                operator_mention: "{{request_text}}".to_string(),
            },
        )
        .with_exit_after_completion_timeout(Duration::from_millis(100));
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot test".to_string(),
                cleaned_text: "test".to_string(),
            },
        };

        let run = timeout(
            Duration::from_secs(6),
            codex.run_unverified(temp.path(), &task),
        )
        .await
        .expect("Codex run should not wait forever after task completion")
        .unwrap();

        assert_eq!(run.response, "file final");
        assert_eq!(run.session_id.as_deref(), Some("session-1"));
    }

    #[tokio::test]
    async fn run_waits_for_normal_exit_after_task_completion() {
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
python3 -c 'import sys, time; output_path = sys.argv[1]; print("{\"type\":\"thread.started\",\"thread_id\":\"session-1\"}"); print("{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"json final\"}}"); print("{\"type\":\"turn.completed\"}"); sys.stdout.flush(); time.sleep(0.2); open(output_path, "w").write("file final after graceful exit\n")' "$output_path"
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&bin).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).unwrap();

        let codex = CodexCli::new(
            bin.display().to_string(),
            temp.path().join("codex-home"),
            temp.path().join("runtime-home"),
            "test-model",
            "low",
            CodexPromptTemplates {
                mention: "{{cleaned_text}}".to_string(),
                pull_request_opened: "{{author}}".to_string(),
                operator_mention: "{{request_text}}".to_string(),
            },
        )
        .with_exit_after_completion_timeout(Duration::from_secs(2));
        let task = CodexTask {
            pr_url: "https://github.com/o/r/pull/1".to_string(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".to_string(),
                raw_body: "@maid-bot test".to_string(),
                cleaned_text: "test".to_string(),
            },
        };

        let run = timeout(
            Duration::from_secs(3),
            codex.run_unverified(temp.path(), &task),
        )
        .await
        .expect("Codex run should wait for ordinary graceful exit after completion")
        .unwrap();

        assert_eq!(run.response, "file final after graceful exit");
        assert_eq!(run.session_id.as_deref(), Some("session-1"));
    }
}
