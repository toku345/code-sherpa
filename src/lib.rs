//! code-sherpa pipeline primitives.
//!
//! Drives a GitHub Issue through planning, implementation, testing, and
//! review stages. This module holds the deterministic primitives the
//! pipeline manager builds on; stage orchestration is layered on top.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use wait_timeout::ChildExt;

/// Default timeout for deterministic shell commands.
pub const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_secs(120);
/// Default timeout for agent invocations.
pub const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(300);

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
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("{label}: failed to spawn"))?;

    let stdin_writer = if let Some(data) = stdin_data {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let owned = data.to_owned();
        Some(std::thread::spawn(move || {
            stdin.write_all(owned.as_bytes()).context("write stdin")
        }))
    } else {
        None
    };

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        stdout_pipe
            .read_to_string(&mut buf)
            .context("read stdout")?;
        Ok(buf)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        stderr_pipe
            .read_to_string(&mut buf)
            .context("read stderr")?;
        Ok(buf)
    });

    let status = match child.wait_timeout(timeout) {
        Ok(status) => status,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_capture_threads(stdin_writer, stdout_reader, stderr_reader, label);
            return Err(err).with_context(|| format!("{label}: failed while waiting"));
        }
    };

    match status {
        Some(status) => {
            let (stdout, stderr) =
                join_capture_threads(stdin_writer, stdout_reader, stderr_reader, label)?;
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
            child
                .kill()
                .with_context(|| format!("{label}: timed out and failed to kill child"))?;
            child
                .wait()
                .with_context(|| format!("{label}: timed out and failed to reap child"))?;
            if let Err(err) =
                join_capture_threads(stdin_writer, stdout_reader, stderr_reader, label)
            {
                bail!("{label}: timed out after {}s; {err:#}", timeout.as_secs());
            }
            bail!("{label}: timed out after {}s", timeout.as_secs())
        }
    }
}

fn join_capture_threads(
    stdin_writer: Option<JoinHandle<Result<()>>>,
    stdout_reader: JoinHandle<Result<String>>,
    stderr_reader: JoinHandle<Result<String>>,
    label: &str,
) -> Result<(String, String)> {
    let mut first_error = None;

    if let Some(writer) = stdin_writer
        && let Err(err) = join_io_thread(writer, label, "write stdin")
    {
        first_error = Some(err);
    }

    let stdout = match join_io_thread(stdout_reader, label, "read stdout") {
        Ok(stdout) => stdout,
        Err(err) => {
            if first_error.is_none() {
                first_error = Some(err);
            }
            String::new()
        }
    };

    let stderr = match join_io_thread(stderr_reader, label, "read stderr") {
        Ok(stderr) => stderr,
        Err(err) => {
            if first_error.is_none() {
                first_error = Some(err);
            }
            String::new()
        }
    };

    if let Some(err) = first_error {
        Err(err)
    } else {
        Ok((stdout, stderr))
    }
}

fn join_io_thread<T>(handle: JoinHandle<Result<T>>, label: &str, action: &str) -> Result<T> {
    match handle.join() {
        Ok(result) => result.with_context(|| format!("{label}: failed to {action}")),
        Err(_) => bail!("{label}: {action} thread panicked"),
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
        Some(other) => Ok(other.to_string()),
        None => bail!("claude: missing 'result' key: {}", truncate(stdout, 200)),
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
