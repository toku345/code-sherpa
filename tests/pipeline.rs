//! Tests for the pipeline primitives.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
#[cfg(unix)]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use code_sherpa::{PipelineContext, Stage, load_prompt, parse_agent_output, run_agent, run_cmd};

#[cfg(unix)]
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn stage_values_and_order() {
    let expected = [
        "issue_fetch",
        "plan_creation",
        "plan_review",
        "branch_creation",
        "implementation",
        "test_execution",
        "pr_creation",
        "code_review",
    ];
    let actual: Vec<&str> = Stage::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(actual, expected);
}

#[test]
fn pipeline_context_defaults() {
    let ctx = PipelineContext::new(1, "owner/repo", "/tmp/worktree");
    assert_eq!(ctx.issue_number, 1);
    assert_eq!(ctx.repo, "owner/repo");
    assert_eq!(ctx.issue_title, "");
    assert_eq!(ctx.issue_body, "");
    assert_eq!(ctx.plan, "");
    assert_eq!(ctx.worktree_path, "/tmp/worktree");
    assert_eq!(ctx.branch_name, "");
    assert_eq!(ctx.last_error, "");
}

#[test]
fn run_cmd_success() {
    let out = run_cmd(&["echo", "hello"], None, Duration::from_secs(10)).unwrap();
    assert_eq!(out, "hello\n");
}

#[test]
fn run_cmd_honors_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_cmd(&["pwd"], Some(dir.path()), Duration::from_secs(10)).unwrap();

    assert_eq!(
        std::fs::canonicalize(out.trim()).unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
}

#[test]
fn run_cmd_timeout() {
    let err = run_cmd(&["sleep", "5"], None, Duration::from_secs(1)).unwrap_err();
    assert!(err.to_string().contains("timed out after 1s"), "{err}");
}

#[cfg(unix)]
#[test]
fn run_cmd_timeout_kills_descendants_holding_pipes() {
    let start = Instant::now();
    let err = run_cmd(
        &["/bin/sh", "-c", "(sleep 30) & sleep 30"],
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();

    assert!(err.to_string().contains("timed out after 1s"), "{err}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "timeout waited for descendant pipe holders: {:?}",
        start.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn run_cmd_timeout_covers_early_parent_exit_with_descendant_pipe_holder() {
    let start = Instant::now();
    let err = run_cmd(
        &["/bin/sh", "-c", "(sleep 30) & exit 0"],
        None,
        Duration::from_secs(1),
    )
    .unwrap_err();

    assert!(err.to_string().contains("timed out after 1s"), "{err}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "timeout waited for descendant pipe holders after parent exit: {:?}",
        start.elapsed()
    );
}

#[test]
fn run_cmd_failure() {
    let err = run_cmd(
        &["ls", "nonexistent_path_that_does_not_exist"],
        None,
        Duration::from_secs(10),
    )
    .unwrap_err();
    assert!(err.to_string().starts_with("ls:"), "{err}");
}

#[cfg(unix)]
#[test]
fn run_cmd_drains_large_stdout_without_deadlock() {
    let out = run_cmd(
        &["/bin/sh", "-c", "yes x | head -c 200000"],
        None,
        Duration::from_secs(10),
    )
    .unwrap();

    assert_eq!(out.len(), 200_000);
}

#[cfg(unix)]
#[test]
fn run_agent_passes_prompt_to_claude_and_parses_result() {
    let work = tempfile::tempdir().unwrap();
    let result = with_fake_claude(
        r#"#!/bin/sh
printf '%s\n' "$@" > args.txt
cat > prompt.txt
printf '{"result":"agent done"}'
"#,
        || run_agent("please plan", Some(work.path()), Duration::from_secs(10)),
    );

    assert_eq!(result.unwrap(), "agent done");
    assert_eq!(
        std::fs::read_to_string(work.path().join("prompt.txt")).unwrap(),
        "please plan"
    );
    assert_eq!(
        std::fs::read_to_string(work.path().join("args.txt")).unwrap(),
        "-p\n--output-format\njson\n"
    );
}

#[cfg(unix)]
#[test]
fn run_agent_surfaces_claude_stderr_on_failure() {
    let err = with_fake_claude(
        r#"#!/bin/sh
cat >/dev/null
printf 'agent failed\n' >&2
exit 7
"#,
        || run_agent("please plan", None, Duration::from_secs(10)),
    )
    .unwrap_err();

    assert!(err.to_string().starts_with("claude: agent failed"), "{err}");
}

#[cfg(unix)]
#[test]
fn run_agent_times_out() {
    let err = with_fake_claude(
        r#"#!/bin/sh
sleep 5
"#,
        || run_agent("please plan", None, Duration::from_secs(1)),
    )
    .unwrap_err();

    assert!(err.to_string().contains("timed out after 1s"), "{err}");
}

#[test]
fn parse_agent_output_success() {
    assert_eq!(parse_agent_output(r#"{"result": "done"}"#).unwrap(), "done");
}

#[test]
fn parse_agent_output_invalid_json() {
    let err = parse_agent_output("not json").unwrap_err();
    assert!(err.to_string().contains("invalid JSON"), "{err}");
}

#[test]
fn parse_agent_output_missing_result() {
    let err = parse_agent_output(r#"{"other": "value"}"#).unwrap_err();
    assert!(err.to_string().contains("missing 'result' key"), "{err}");
}

#[test]
fn parse_agent_output_rejects_non_string_result() {
    let err = parse_agent_output(r#"{"result": null}"#).unwrap_err();
    assert!(
        err.to_string()
            .contains("'result' must be a string, got null"),
        "{err}"
    );
}

#[test]
fn cli_fails_until_pipeline_orchestration_exists() {
    let dir = tempfile::tempdir().unwrap();
    run_git(&["init"], dir.path());
    run_git(
        &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        dir.path(),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_sherpa"))
        .arg("123")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("sherpa: issue #123 in owner/repo at "),
        "{stderr}"
    );
    assert!(
        stderr.contains("pipeline orchestration is not implemented yet"),
        "{stderr}"
    );
}

#[test]
fn cli_requires_current_git_origin() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sherpa"))
        .arg("123")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("current directory must be inside a git repository"),
        "{stderr}"
    );
}

#[test]
fn load_prompt_substitution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.md"), "Hello {{name}}, issue #{{num}}").unwrap();
    let vars = HashMap::from([("name", "Alice"), ("num", "7")]);
    let out = load_prompt("test.md", dir.path(), &vars).unwrap();
    assert_eq!(out, "Hello Alice, issue #7");
}

#[test]
fn load_prompt_missing_variable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.md"), "Hello {{name}}, issue #{{num}}").unwrap();
    let vars = HashMap::from([("name", "Alice")]);
    let out = load_prompt("test.md", dir.path(), &vars).unwrap();
    assert_eq!(out, "Hello Alice, issue #{{num}}");
}

#[test]
fn load_prompt_no_reexpansion() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.md"), "{{first}} and {{second}}").unwrap();
    let vars = HashMap::from([("first", "{{second}}"), ("second", "BOOM")]);
    let out = load_prompt("test.md", dir.path(), &vars).unwrap();
    assert_eq!(out, "{{second}} and BOOM");
}

#[test]
fn load_prompt_unterminated_placeholder() {
    // An opening "{{" with no closing "}}" is emitted verbatim.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.md"), "Hello {{name").unwrap();
    let vars = HashMap::from([("name", "Alice")]);
    let out = load_prompt("test.md", dir.path(), &vars).unwrap();
    assert_eq!(out, "Hello {{name");
}

fn run_git(args: &[&str], cwd: &Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn load_prompt_invalid_placeholder_keys_preserved() {
    // Keys with spaces, punctuation, or empty are not valid placeholders;
    // the literal "{{" is emitted and the rest is preserved verbatim.
    let dir = tempfile::tempdir().unwrap();
    for (template, expected) in [
        ("{{ name }}", "{{ name }}"),
        ("{{a-b}}", "{{a-b}}"),
        ("{{}}", "{{}}"),
    ] {
        std::fs::write(dir.path().join("test.md"), template).unwrap();
        let vars = HashMap::from([("name", "Alice"), ("a", "X")]);
        let out = load_prompt("test.md", dir.path(), &vars).unwrap();
        assert_eq!(out, expected, "template {template:?}");
    }
}

#[test]
fn load_prompt_resumes_after_invalid_placeholder() {
    // After emitting the literal "{{" for an invalid placeholder, scanning
    // resumes and a following valid placeholder is still substituted.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.md"), "{{ bad }}{{name}}").unwrap();
    let vars = HashMap::from([("name", "Alice")]);
    let out = load_prompt("test.md", dir.path(), &vars).unwrap();
    assert_eq!(out, "{{ bad }}Alice");
}

#[cfg(unix)]
fn with_fake_claude<T>(script: &str, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap();
    let bin_dir = tempfile::tempdir().unwrap();
    let claude_path = bin_dir.path().join("claude");
    write_executable(&claude_path, script);

    let old_path = std::env::var_os("PATH");
    let mut paths = vec![bin_dir.path().to_path_buf()];
    if let Some(path) = &old_path {
        paths.extend(std::env::split_paths(path));
    }
    let new_path = std::env::join_paths(paths).unwrap();

    // SAFETY: tests that mutate PATH are serialized by ENV_LOCK.
    unsafe {
        std::env::set_var("PATH", new_path);
    }
    let result = f();
    // SAFETY: tests that mutate PATH are serialized by ENV_LOCK.
    unsafe {
        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }

    result
}

#[cfg(unix)]
fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}
