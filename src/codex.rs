use crate::{
    domain::{CodexPromptTemplates, CodexTask},
    maid::{CodexRun, CodexRunner},
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{Mutex, watch},
    time::timeout,
};
use tracing::warn;

pub const CODEX_WORKER_IMAGE: &str = "maid-codex-worker:0.153.4";
const DOCKER_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const EXIT_AFTER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
const TASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_STDOUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const CODEX_HOME_IN_WORKER: &str = "/run/maid/codex";
const WORKSPACE_IN_WORKER: &str = "/workspace";
const REVIEW_PROFILE: &str = "maid-review";
static CONTAINER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Runs each review in a disposable, resource-bounded Linux container.
/// GitHub credentials remain in Maid; the worker receives only a read-only
/// repository and a dedicated Codex authentication directory.
#[derive(Clone, Debug)]
pub struct CodexWorker {
    runtime_bin: String,
    image: String,
    codex_home: PathBuf,
    model: String,
    reasoning_effort: String,
    prompts: CodexPromptTemplates,
    exit_after_completion_timeout: Duration,
    task_timeout: Duration,
}

impl CodexWorker {
    pub fn new(
        codex_home: impl Into<PathBuf>,
        model: impl Into<String>,
        reasoning_effort: impl Into<String>,
        prompts: CodexPromptTemplates,
    ) -> Self {
        Self {
            runtime_bin: "docker".to_string(),
            image: CODEX_WORKER_IMAGE.to_string(),
            codex_home: codex_home.into(),
            model: model.into(),
            reasoning_effort: reasoning_effort.into(),
            prompts,
            exit_after_completion_timeout: EXIT_AFTER_COMPLETION_TIMEOUT,
            task_timeout: TASK_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_runtime_bin(mut self, value: impl Into<String>) -> Self {
        self.runtime_bin = value.into();
        self
    }

    #[cfg(test)]
    fn with_exit_after_completion_timeout(mut self, value: Duration) -> Self {
        self.exit_after_completion_timeout = value;
        self
    }

    #[cfg(test)]
    fn with_task_timeout(mut self, value: Duration) -> Self {
        self.task_timeout = value;
        self
    }

    fn command(&self, worktree: &Path, container_name: &str) -> Result<Command> {
        let worktree = mount_source(worktree, "task repository")?;
        let codex_home = mount_source(&self.codex_home, "Codex auth directory")?;
        let (uid, gid) = worker_identity()?;
        let mut command = Command::new(&self.runtime_bin);
        command
            .env_clear()
            .env("PATH", DOCKER_PATH)
            .args([
                "run",
                "--rm",
                "--init",
                "--name",
                container_name,
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--security-opt=seccomp=unconfined",
                "--pids-limit",
                "256",
                "--memory",
                "8g",
                "--cpus",
                "2",
                "--user",
                &format!("{uid}:{gid}"),
                "--workdir",
                WORKSPACE_IN_WORKER,
                "--env",
                "HOME=/tmp/maid-home",
                "--env",
                &format!("CODEX_HOME={CODEX_HOME_IN_WORKER}"),
                "--mount",
                &format!("type=bind,src={worktree},dst={WORKSPACE_IN_WORKER},readonly"),
                "--mount",
                &format!("type=bind,src={codex_home},dst={CODEX_HOME_IN_WORKER},readonly"),
                "--tmpfs",
                &format!("/tmp:rw,nosuid,nodev,noexec,size=64m,uid={uid},gid={gid},mode=700"),
                &self.image,
                "--strict-config",
                "--model",
                &self.model,
                "--config",
                &codex_config_string("model_reasoning_effort", &self.reasoning_effort),
                "--config",
                "shell_environment_policy.inherit=\"none\"",
                "--config",
                "allow_login_shell=false",
                "--config",
                "web_search=\"disabled\"",
                "--config",
                "project_doc_max_bytes=0",
                "--config",
                &format!("default_permissions=\"{REVIEW_PROFILE}\""),
                "--config",
                &review_filesystem_policy(),
                "--config",
                &format!("permissions.{REVIEW_PROFILE}.network={{ enabled = false }}"),
                "--config",
                "approval_policy=\"never\"",
                "exec",
                "--ignore-user-config",
                "--ignore-rules",
                "--ephemeral",
                "--color",
                "never",
                "--json",
                "--skip-git-repo-check",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_group(&mut command);
        Ok(command)
    }

    async fn run_container(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun> {
        create_private_directory(&self.codex_home)?;
        let prompt = task.prompt(&self.prompts)?;
        let container_name = next_container_name();
        let mut command = self.command(worktree, &container_name)?;
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start review worker; build {} with `just worker-image`",
                self.image
            )
        })?;
        let mut cleanup = ContainerGuard::new(self.runtime_bin.clone(), container_name);
        let mut process_group = ProcessGroupGuard::for_child(&child);

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open review worker stdin"))?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write review prompt")?;
        drop(stdin);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to open review worker stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to open review worker stderr"))?;
        let mut stderr_task =
            tokio::spawn(async move { read_bounded(&mut stderr, MAX_STDERR_BYTES).await });
        let events = Arc::new(Mutex::new(CodexJsonEvents::default()));
        let (completed_tx, mut completed_rx) = watch::channel(false);
        let mut stdout_task = tokio::spawn(read_codex_stdout(stdout, events.clone(), completed_tx));

        let status = match timeout(
            self.task_timeout,
            wait_for_worker(
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
                cleanup.terminate().await;
                stdout_task.abort();
                stderr_task.abort();
                bail!(
                    "review worker exceeded its {} second time limit",
                    self.task_timeout.as_secs()
                );
            }
        };

        if status.is_none() {
            terminate_process_group(&mut child).await;
            cleanup.terminate().await;
        } else {
            cleanup.disarm();
        }
        process_group.disarm();
        match timeout(PIPE_DRAIN_TIMEOUT, &mut stdout_task).await {
            Ok(result) => result.context("failed to join worker stdout reader")??,
            Err(_) => {
                stdout_task.abort();
                warn!("review worker stdout did not close; using captured events");
            }
        }
        let stderr = match timeout(PIPE_DRAIN_TIMEOUT, &mut stderr_task).await {
            Ok(result) => result
                .context("failed to join worker stderr reader")?
                .context("failed to read bounded worker stderr")?,
            Err(_) => {
                stderr_task.abort();
                warn!("review worker stderr did not close");
                Vec::new()
            }
        };
        if let Some(status) = status
            && !status.success()
        {
            return Err(anyhow!(
                "review worker exited with {}: {}",
                status,
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        let response = events
            .lock()
            .await
            .last_message
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string();
        if response.is_empty() {
            bail!("Codex produced an empty response");
        }
        Ok(CodexRun { response })
    }
}

#[async_trait]
impl CodexRunner for CodexWorker {
    async fn run(&self, worktree: &Path, task: &CodexTask) -> Result<CodexRun> {
        self.run_container(worktree, task).await
    }
}

fn review_filesystem_policy() -> String {
    format!(
        "permissions.{REVIEW_PROFILE}.filesystem={{ \":root\" = \"deny\", \":minimal\" = \"read\", \":tmpdir\" = \"deny\", \":slash_tmp\" = \"deny\", \":workspace_roots\" = {{ \".\" = \"read\" }} }}"
    )
}

fn mount_source(path: &Path, label: &str) -> Result<String> {
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {label} {}", path.display()))?;
    let value = path.to_string_lossy().to_string();
    if value.contains(',') {
        bail!("{label} cannot contain a comma: {}", path.display());
    }
    Ok(value)
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

fn worker_identity() -> Result<(u32, u32)> {
    #[cfg(unix)]
    {
        Ok((unsafe { libc::geteuid() }, unsafe { libc::getegid() }))
    }
    #[cfg(not(unix))]
    {
        bail!("the container review worker requires a Unix controller host")
    }
}

fn next_container_name() -> String {
    let sequence = CONTAINER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("maid-review-{}-{sequence}", std::process::id())
}

fn codex_config_string(key: &str, value: &str) -> String {
    format!("{key}={}", toml::Value::String(value.to_string()))
}

async fn wait_for_worker(
    child: &mut Child,
    completed_rx: &mut watch::Receiver<bool>,
    exit_timeout: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    tokio::select! {
        status = child.wait() => Ok(Some(status.context("review worker failed to run")?)),
        completed = async { completed_rx.wait_for(|completed| *completed).await.map(|_| ()) } => {
            completed.context("worker completion channel closed")?;
            match timeout(exit_timeout, child.wait()).await {
                Ok(status) => Ok(Some(status.context("review worker failed to run")?)),
                Err(_) => { warn!("review worker did not exit after task completion; terminating it"); Ok(None) }
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

async fn read_codex_stdout(
    mut stdout: impl AsyncRead + Unpin,
    events: Arc<Mutex<CodexJsonEvents>>,
    completed: watch::Sender<bool>,
) -> Result<()> {
    let mut total = 0_usize;
    let mut exceeded = false;
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stdout
            .read(&mut buffer)
            .await
            .context("failed to read worker stdout")?;
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
            observe_stdout_line(&line, &events, &completed).await;
        }
    }
    if !pending.is_empty() {
        observe_stdout_line(&pending, &events, &completed).await;
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
) {
    let Ok(line) = std::str::from_utf8(line) else {
        return;
    };
    if events
        .lock()
        .await
        .observe_line(line.trim_end_matches(['\r', '\n']))
    {
        let _ = completed.send(true);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CodexJsonEvents {
    last_message: Option<String>,
}

impl CodexJsonEvents {
    fn observe_line(&mut self, line: &str) -> bool {
        let Ok(event) = serde_json::from_str::<CodexJsonEvent>(line) else {
            return false;
        };
        match event {
            CodexJsonEvent::ItemCompleted {
                item: CodexJsonItem::AgentMessage { text },
            } => {
                self.last_message = Some(text);
                false
            }
            CodexJsonEvent::TurnCompleted | CodexJsonEvent::TaskComplete => true,
            CodexJsonEvent::ItemCompleted { .. } | CodexJsonEvent::Other => false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexJsonEvent {
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

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}
#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

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
    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
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

async fn terminate_process_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

struct ContainerGuard {
    runtime_bin: String,
    name: Option<String>,
}
impl ContainerGuard {
    fn new(runtime_bin: String, name: String) -> Self {
        Self {
            runtime_bin,
            name: Some(name),
        }
    }
    fn disarm(&mut self) {
        self.name = None;
    }
    async fn terminate(&mut self) {
        let Some(name) = self.name.take() else {
            return;
        };
        let mut command = Command::new(&self.runtime_bin);
        command
            .env_clear()
            .env("PATH", DOCKER_PATH)
            .args(["rm", "--force", &name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(mut child) = command.spawn() {
            let _ = timeout(CLEANUP_TIMEOUT, child.wait()).await;
        }
    }
}
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let Some(name) = self.name.take() else {
            return;
        };
        let _ = std::process::Command::new(&self.runtime_bin)
            .env_clear()
            .env("PATH", DOCKER_PATH)
            .args(["rm", "--force", &name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CodexTask, CodexTaskOrigin};
    use std::{fs, os::unix::fs::PermissionsExt};

    fn prompts() -> CodexPromptTemplates {
        CodexPromptTemplates {
            mention: "{{cleaned_text}}".into(),
            pull_request_opened: "{{author}}".into(),
        }
    }
    fn task() -> CodexTask {
        CodexTask {
            pr_url: "https://github.com/o/r/pull/1".into(),
            origin: CodexTaskOrigin::Mention {
                mention_url: "https://github.com/o/r/pull/1#issuecomment-2".into(),
                raw_body: "@maid-bot review".into(),
                cleaned_text: "review".into(),
            },
        }
    }
    fn executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut p = fs::metadata(path).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(path, p).unwrap();
    }

    #[test]
    fn command_has_one_fixed_review_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        let worker = CodexWorker::new(&codex_home, "test-model", "low", prompts());
        let command = worker.command(&worktree, "maid-review-test").unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        for required in [
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--security-opt=seccomp=unconfined",
            "--ignore-user-config",
            "--ignore-rules",
            "--ephemeral",
            "approval_policy=\"never\"",
        ] {
            assert!(args.iter().any(|arg| arg == required));
        }
        assert!(args.iter().any(|arg| arg.contains("maid-review.filesystem")
            && arg.contains("\":root\" = \"deny\"")
            && arg.contains("\".\" = \"read\"")));
        assert!(
            args.iter()
                .any(|arg| arg == "permissions.maid-review.network={ enabled = false }")
        );
        assert!(
            args.iter()
                .any(|arg| arg.ends_with("dst=/workspace,readonly")
                    && arg.contains(worktree.to_string_lossy().as_ref()))
        );
        assert!(!args.iter().any(|arg| arg.contains("github")));
        assert_eq!(command.as_std().get_envs().count(), 1);
    }

    #[test]
    fn parses_only_the_last_agent_message() {
        let mut events = CodexJsonEvents::default();
        assert!(!events.observe_line("not json"));
        assert!(!events.observe_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}"#
        ));
        assert!(!events.observe_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"final"}}"#
        ));
        assert!(events.observe_line(r#"{"type":"turn.completed"}"#));
        assert_eq!(events.last_message.as_deref(), Some("final"));
    }

    #[tokio::test]
    async fn task_deadline_terminates_the_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        let runtime = temp.path().join("fake-runtime");
        executable(&runtime, "#!/bin/sh\ncat >/dev/null\nsleep 5\n");
        let worker = CodexWorker::new(&codex_home, "test-model", "low", prompts())
            .with_runtime_bin(runtime.to_string_lossy())
            .with_task_timeout(Duration::from_millis(100));
        assert!(
            worker
                .run_container(&worktree, &task())
                .await
                .unwrap_err()
                .to_string()
                .contains("time limit")
        );
    }

    #[tokio::test]
    async fn completion_uses_json_without_persisting_a_session() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let codex_home = temp.path().join("codex");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        let runtime = temp.path().join("fake-runtime");
        executable(
            &runtime,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"review complete\"}}'\nprintf '%s\\n' '{\"type\":\"turn.completed\"}'\n",
        );
        let worker = CodexWorker::new(&codex_home, "test-model", "low", prompts())
            .with_runtime_bin(runtime.to_string_lossy())
            .with_exit_after_completion_timeout(Duration::from_secs(1));
        assert_eq!(
            worker
                .run_container(&worktree, &task())
                .await
                .unwrap()
                .response,
            "review complete"
        );
    }
}
