//! code-sherpa pipeline primitives.
//!
//! Drives a GitHub Issue through planning, implementation, testing, and
//! review stages. This module holds the deterministic primitives the
//! pipeline manager builds on; stage orchestration is layered on top.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result, anyhow, bail};
use wait_timeout::ChildExt;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Default timeout for deterministic shell commands.
pub const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_secs(120);
/// Default timeout for agent invocations.
pub const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(300);

const CAPTURE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A stage in the pipeline, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    IssueFetch,
    PlanCreation,
    PlanReview,
    BranchCreation,
    Implementation,
    TestExecution,
    PrCreation,
    CodeReview,
}

impl Stage {
    /// All stages in pipeline order.
    pub const ALL: [Stage; 8] = [
        Stage::IssueFetch,
        Stage::PlanCreation,
        Stage::PlanReview,
        Stage::BranchCreation,
        Stage::Implementation,
        Stage::TestExecution,
        Stage::PrCreation,
        Stage::CodeReview,
    ];

    /// The serialization name for this stage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::IssueFetch => "issue_fetch",
            Stage::PlanCreation => "plan_creation",
            Stage::PlanReview => "plan_review",
            Stage::BranchCreation => "branch_creation",
            Stage::Implementation => "implementation",
            Stage::TestExecution => "test_execution",
            Stage::PrCreation => "pr_creation",
            Stage::CodeReview => "code_review",
        }
    }
}

/// Mutable state threaded through the pipeline stages.
#[derive(Debug, Clone, Default)]
pub struct PipelineContext {
    pub issue_number: u64,
    pub repo: String,
    pub issue_title: String,
    pub issue_body: String,
    pub plan: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub last_error: String,
}

impl PipelineContext {
    /// Create a context for `issue_number` in `repo` (`owner/repo`).
    pub fn new(issue_number: u64, repo: impl Into<String>) -> Self {
        Self {
            issue_number,
            repo: repo.into(),
            ..Self::default()
        }
    }
}

/// Run a command to completion and return its stdout. Errors loudly on a
/// non-zero exit or timeout (fail loud).
///
/// stdout/stderr are drained on separate threads, and stdin is written on a
/// separate thread when provided, so pipe I/O cannot deadlock the timeout wait.
fn capture(
    mut command: Command,
    stdin_data: Option<&str>,
    timeout: Duration,
    label: &str,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    }
    isolate_child_processes(&mut command);

    let mut child = command
        .spawn()
        .with_context(|| format!("{label}: failed to spawn"))?;

    let stdin_writer = if let Some(data) = stdin_data {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let owned = data.to_owned();
        Some(spawn_io_thread("write stdin", move || {
            stdin.write_all(owned.as_bytes()).context("write stdin")
        }))
    } else {
        None
    };

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = spawn_io_thread("read stdout", move || {
        let mut buf = String::new();
        stdout_pipe
            .read_to_string(&mut buf)
            .context("read stdout")?;
        Ok(buf)
    });
    let stderr_reader = spawn_io_thread("read stderr", move || {
        let mut buf = String::new();
        stderr_pipe
            .read_to_string(&mut buf)
            .context("read stderr")?;
        Ok(buf)
    });

    let status = match child.wait_timeout(timeout) {
        Ok(status) => status,
        Err(err) => {
            let mut cleanup_errors = Vec::new();
            if let Err(cleanup_err) =
                terminate_child_processes(&mut child, label, "failed while waiting")
            {
                cleanup_errors.push(format!("{cleanup_err:#}"));
            }
            if let Err(capture_err) = wait_capture_threads(
                stdin_writer,
                stdout_reader,
                stderr_reader,
                label,
                cleanup_deadline(),
            ) {
                cleanup_errors.push(format!("{capture_err:#}"));
            }

            if cleanup_errors.is_empty() {
                return Err(err).with_context(|| format!("{label}: failed while waiting"));
            }
            bail!(
                "{label}: failed while waiting: {err}; cleanup errors: {}",
                cleanup_errors.join("; ")
            );
        }
    };

    match status {
        Some(status) => {
            let (stdout, stderr) = match wait_capture_threads(
                stdin_writer,
                stdout_reader,
                stderr_reader,
                label,
                deadline,
            ) {
                Ok(output) => output,
                Err(CaptureError::TimedOut(err)) => {
                    terminate_child_processes(
                        &mut child,
                        label,
                        "timed out while waiting for captured output",
                    )?;
                    bail!("{label}: timed out after {}s; {err:#}", timeout.as_secs());
                }
                Err(CaptureError::Failed(err)) => return Err(err),
            };
            if status.success() {
                Ok(stdout)
            } else {
                let detail = if stderr.trim().is_empty() {
                    stdout
                } else {
                    stderr
                };
                bail!("{label}: {}", detail.trim())
            }
        }
        None => {
            terminate_child_processes(&mut child, label, "timed out")?;
            if let Err(err) = wait_capture_threads(
                stdin_writer,
                stdout_reader,
                stderr_reader,
                label,
                cleanup_deadline(),
            ) {
                bail!("{label}: timed out after {}s; {err:#}", timeout.as_secs());
            }
            bail!("{label}: timed out after {}s", timeout.as_secs())
        }
    }
}

fn isolate_child_processes(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }

    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

fn terminate_child_processes(child: &mut Child, label: &str, reason: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        // SAFETY: `pgid` is the just-spawned child process group. Passing a
        // negative pid asks POSIX `kill` to signal that process group.
        let kill_result = unsafe { kill(-pgid, SIGKILL) };
        if kill_result == -1 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("{label}: {reason} and failed to kill process group"));
        }
        child
            .wait()
            .with_context(|| format!("{label}: {reason} and failed to reap child"))?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        child
            .kill()
            .with_context(|| format!("{label}: {reason} and failed to kill child"))?;
        child
            .wait()
            .with_context(|| format!("{label}: {reason} and failed to reap child"))?;
        Ok(())
    }
}

struct IoThread<T> {
    action: &'static str,
    receiver: Receiver<Result<T>>,
    handle: JoinHandle<()>,
}

enum CaptureError {
    TimedOut(Error),
    Failed(Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::TimedOut(err) | CaptureError::Failed(err) => err.fmt(f),
        }
    }
}

impl std::fmt::Debug for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

fn spawn_io_thread<T>(
    action: &'static str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> IoThread<T>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _ = sender.send(work());
    });

    IoThread {
        action,
        receiver,
        handle,
    }
}

fn cleanup_deadline() -> Instant {
    Instant::now() + CAPTURE_CLEANUP_TIMEOUT
}

fn wait_capture_threads(
    stdin_writer: Option<IoThread<()>>,
    stdout_reader: IoThread<String>,
    stderr_reader: IoThread<String>,
    label: &str,
    deadline: Instant,
) -> std::result::Result<(String, String), CaptureError> {
    let mut first_error = None;

    if let Some(writer) = stdin_writer
        && let Err(err) = wait_io_thread(writer, label, deadline)
    {
        match err {
            CaptureError::TimedOut(err) => return Err(CaptureError::TimedOut(err)),
            CaptureError::Failed(err) => first_error = Some(err),
        }
    }

    let stdout = match wait_io_thread(stdout_reader, label, deadline) {
        Ok(stdout) => stdout,
        Err(err) => match err {
            CaptureError::TimedOut(err) => return Err(CaptureError::TimedOut(err)),
            CaptureError::Failed(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
                String::new()
            }
        },
    };

    let stderr = match wait_io_thread(stderr_reader, label, deadline) {
        Ok(stderr) => stderr,
        Err(err) => match err {
            CaptureError::TimedOut(err) => return Err(CaptureError::TimedOut(err)),
            CaptureError::Failed(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
                String::new()
            }
        },
    };

    if let Some(err) = first_error {
        Err(CaptureError::Failed(err))
    } else {
        Ok((stdout, stderr))
    }
}

fn wait_io_thread<T>(
    thread: IoThread<T>,
    label: &str,
    deadline: Instant,
) -> std::result::Result<T, CaptureError> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(CaptureError::TimedOut(anyhow!(
            "{label}: timed out while waiting to {}",
            thread.action
        )));
    };

    let result = match thread.receiver.recv_timeout(remaining) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            return Err(CaptureError::TimedOut(anyhow!(
                "{label}: timed out while waiting to {}",
                thread.action
            )));
        }
        Err(RecvTimeoutError::Disconnected) => {
            let err = match thread.handle.join() {
                Ok(()) => anyhow!("{label}: {} thread exited without result", thread.action),
                Err(_) => anyhow!("{label}: {} thread panicked", thread.action),
            };
            return Err(CaptureError::Failed(err));
        }
    };

    match thread.handle.join() {
        Ok(()) => result
            .with_context(|| format!("{label}: failed to {}", thread.action))
            .map_err(CaptureError::Failed),
        Err(_) => Err(CaptureError::Failed(anyhow!(
            "{label}: {} thread panicked",
            thread.action
        ))),
    }
}

/// Run a deterministic shell command, returning its stdout.
pub fn run_cmd(cmd: &[&str], cwd: Option<&Path>, timeout: Duration) -> Result<String> {
    let (program, args) = cmd
        .split_first()
        .ok_or_else(|| anyhow!("run_cmd: empty command"))?;
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    capture(command, None, timeout, program)
}

/// Invoke the Claude Code agent headlessly, feeding `prompt` on stdin and
/// returning the agent's `result` field.
pub fn run_agent(prompt: &str, cwd: Option<&Path>, timeout: Duration) -> Result<String> {
    let mut command = Command::new("claude");
    command.args(["-p", "--output-format", "json"]);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let stdout = capture(command, Some(prompt), timeout, "claude")?;
    parse_agent_output(&stdout)
}

/// Parse the JSON envelope emitted by `claude -p --output-format json` and
/// extract its `result` field.
pub fn parse_agent_output(stdout: &str) -> Result<String> {
    let data: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|_| anyhow!("claude: invalid JSON: {}", truncate(stdout, 200)))?;
    match data.get("result") {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(other) => bail!(
            "claude: 'result' must be a string, got {}: {}",
            json_value_kind(other),
            truncate(stdout, 200)
        ),
        None => bail!("claude: missing 'result' key: {}", truncate(stdout, 200)),
    }
}

fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Load a prompt template from `prompts_dir` and substitute `{{var}}`
/// placeholders. Unknown placeholders are left intact; substituted values
/// are not re-expanded.
pub fn load_prompt(
    template_name: &str,
    prompts_dir: &Path,
    variables: &HashMap<&str, &str>,
) -> Result<String> {
    let path = prompts_dir.join(template_name);
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("load_prompt: cannot read {}", path.display()))?;
    Ok(substitute(&template, variables))
}

fn substitute(template: &str, variables: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = &after[..end];
                if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    match variables.get(key) {
                        Some(value) => out.push_str(value),
                        None => {
                            out.push_str("{{");
                            out.push_str(key);
                            out.push_str("}}");
                        }
                    }
                    rest = &after[end + 2..];
                } else {
                    // Not a valid placeholder; emit the literal "{{" and keep scanning.
                    out.push_str("{{");
                    rest = after;
                }
            }
            None => {
                // No closing braces; emit the remainder verbatim.
                out.push_str("{{");
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}
