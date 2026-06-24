//! Tests for the pipeline primitives.

use std::collections::HashMap;
use std::time::Duration;

use code_sherpa::{PipelineContext, Stage, load_prompt, parse_agent_output, run_cmd};

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
    let ctx = PipelineContext::new(1, "owner/repo");
    assert_eq!(ctx.issue_number, 1);
    assert_eq!(ctx.repo, "owner/repo");
    assert_eq!(ctx.issue_title, "");
    assert_eq!(ctx.issue_body, "");
    assert_eq!(ctx.plan, "");
    assert_eq!(ctx.worktree_path, "");
    assert_eq!(ctx.branch_name, "");
    assert_eq!(ctx.last_error, "");
}

#[test]
fn run_cmd_success() {
    let out = run_cmd(&["echo", "hello"], None, Duration::from_secs(10)).unwrap();
    assert_eq!(out, "hello\n");
}

#[test]
fn run_cmd_timeout() {
    let err = run_cmd(&["sleep", "5"], None, Duration::from_secs(1)).unwrap_err();
    assert!(err.to_string().contains("timed out after 1s"), "{err}");
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
