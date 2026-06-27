//! Tests for the pipeline primitives.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
#[cfg(unix)]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use code_sherpa::{
    PipelineContext, PipelineOptions, ReviewDecision, Stage, load_prompt, parse_agent_output,
    parse_review_verdict, run_agent, run_cmd, run_pipeline,
};

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
    assert_eq!(ctx.base_commit, "");
    assert_eq!(ctx.last_error, "");
}

#[test]
fn pipeline_options_default_log_path_stays_outside_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let options = PipelineOptions::new(&repo, repo.join("docs/prompts"));

    assert_eq!(
        options.log_path,
        dir.path().join(".sherpa-worktrees/observations.jsonl")
    );
    assert!(!options.log_path.starts_with(&repo));
    assert_eq!(options.base_ref, "origin/main");
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
        "-p\n--output-format\njson\n--no-session-persistence\n"
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
fn run_cmd_failure_includes_stdout_and_stderr() {
    let err = run_cmd(
        &[
            "/bin/sh",
            "-c",
            "printf 'assertion failed\\n'; printf 'warning\\n' >&2; exit 1",
        ],
        None,
        Duration::from_secs(10),
    )
    .unwrap_err();
    let message = err.to_string();

    assert!(message.contains("stderr:\nwarning"), "{message}");
    assert!(message.contains("stdout:\nassertion failed"), "{message}");
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
fn parse_agent_output_success_from_transcript_result_event() {
    let stdout = r#"[{"type":"system"},{"type":"assistant","message":{"content":[{"type":"text","text":"ignored"}]}},{"type":"result","result":"done"}]"#;

    assert_eq!(parse_agent_output(stdout).unwrap(), "done");
}

#[test]
fn parse_agent_output_rejects_transcript_without_result_event() {
    let err = parse_agent_output(r#"[{"type":"system"},{"type":"assistant"}]"#).unwrap_err();

    assert!(err.to_string().contains("missing result event"), "{err}");
}

#[test]
fn parse_agent_output_rejects_transcript_result_event_without_result() {
    let err = parse_agent_output(r#"[{"type":"result","usage":{}}]"#).unwrap_err();

    assert!(
        err.to_string()
            .contains("result event missing 'result' key"),
        "{err}"
    );
}

#[test]
fn parse_agent_output_rejects_transcript_non_string_result() {
    let err = parse_agent_output(r#"[{"type":"result","result":42}]"#).unwrap_err();

    assert!(
        err.to_string()
            .contains("result event 'result' must be a string"),
        "{err}"
    );
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
fn parse_review_verdict_accepts_contract_line() {
    let verdict = parse_review_verdict("VERDICT: reject\nMissing tests").unwrap();

    assert_eq!(verdict.decision, ReviewDecision::Reject);
    assert_eq!(verdict.reasons, ["Missing tests"]);
}

#[test]
fn parse_review_verdict_accepts_json_contract() {
    let verdict =
        parse_review_verdict(r#"{"verdict":"changes_requested","reasons":["fix lint"]}"#).unwrap();

    assert_eq!(verdict.decision, ReviewDecision::ChangesRequested);
    assert_eq!(verdict.reasons, ["fix lint"]);
}

#[test]
fn parse_review_verdict_rejects_json_missing_verdict() {
    let err = parse_review_verdict(r#"{"reasons":["fix lint"]}"#).unwrap_err();

    assert!(
        err.to_string()
            .contains("must contain string field 'verdict'"),
        "{err}"
    );
}

#[test]
fn parse_review_verdict_rejects_json_unknown_verdict() {
    let err = parse_review_verdict(r#"{"verdict":"maybe"}"#).unwrap_err();

    assert!(err.to_string().contains("unknown review verdict"), "{err}");
}

#[test]
fn parse_review_verdict_rejects_json_non_array_reasons() {
    let err = parse_review_verdict(r#"{"verdict":"approve","reasons":"ok"}"#).unwrap_err();

    assert!(
        err.to_string()
            .contains("JSON field 'reasons' must be an array"),
        "{err}"
    );
}

#[test]
fn parse_review_verdict_rejects_json_non_string_reason() {
    let err = parse_review_verdict(r#"{"verdict":"approve","reasons":[42]}"#).unwrap_err();

    assert!(err.to_string().contains("reasons must be strings"), "{err}");
}

#[test]
fn parse_review_verdict_fails_closed_on_ambiguous_text() {
    let err = parse_review_verdict("I cannot approve; reject this plan").unwrap_err();

    assert!(
        err.to_string()
            .contains("first non-empty line must be 'VERDICT: approve|reject|changes_requested'"),
        "{err}"
    );
}

#[test]
fn parse_review_verdict_fails_closed_on_multiple_verdicts() {
    let err = parse_review_verdict("VERDICT: approve\nVERDICT: reject").unwrap_err();

    assert!(err.to_string().contains("exactly one 'VERDICT"), "{err}");
}

#[test]
fn parse_review_verdict_fails_closed_when_verdict_is_not_first_line() {
    let err = parse_review_verdict("Looks good\nVERDICT: approve").unwrap_err();

    assert!(
        err.to_string()
            .contains("first non-empty line must be 'VERDICT: approve|reject|changes_requested'"),
        "{err}"
    );
}

#[test]
fn run_pipeline_rejects_empty_test_commands() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let mut options = PipelineOptions::new(&repo, repo.join("docs/prompts"));
    options.test_commands = Vec::new();
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let err = run_pipeline(ctx, &options).unwrap_err();

    assert!(
        err.to_string()
            .contains("PipelineOptions test_commands must not be empty"),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn pipeline_happy_path_with_fake_tools_dry_runs_pr_and_reviews_code() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let log_path = dir.path().join("observations.jsonl");
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = log_path.clone();
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let result = with_fake_pipeline_tools(dir.path(), FakeScenario::Happy, || {
        run_pipeline(ctx, &options)
    })
    .unwrap();

    assert!(result.dry_run);
    assert_eq!(
        result.code_review.unwrap().decision,
        ReviewDecision::Approve
    );
    assert_eq!(result.context.issue_title, "Pipeline issue");
    assert_eq!(result.context.branch_name, "sherpa/issue-10");
    assert!(
        Path::new(&result.context.worktree_path)
            .join("implemented.txt")
            .exists()
    );

    let log = std::fs::read_to_string(log_path).unwrap();
    assert!(log.contains("\"stage\":\"issue_fetch\""), "{log}");
    assert!(log.contains("\"stage\":\"code_review\""), "{log}");
    assert!(
        log.contains("\"raw_verdict_text\":\"VERDICT: approve"),
        "{log}"
    );

    let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    assert!(calls.contains("gh pr list"), "{calls}");
    assert!(
        calls.contains("git fetch --quiet origin refs/heads/main"),
        "{calls}"
    );
    assert!(
        calls.contains("git worktree add ")
            && calls.contains(" -b sherpa/issue-10 0123456789abcdef0123456789abcdef01234567"),
        "{calls}"
    );
    assert!(calls.contains("git add --intent-to-add ."), "{calls}");
    assert!(calls.contains("git diff --no-ext-diff 0123456789abcdef0123456789abcdef01234567"));
    assert!(!calls.contains("git add -A"), "{calls}");
    assert!(!calls.contains("git commit"), "{calls}");
    assert!(!calls.contains("git push"), "{calls}");
    assert!(!calls.contains("gh pr create"), "{calls}");

    let review_prompt = std::fs::read_to_string(dir.path().join("code-review-prompt.txt")).unwrap();
    assert!(
        review_prompt.contains("diff --git a/implemented.txt b/implemented.txt"),
        "{review_prompt}"
    );
}

#[cfg(unix)]
#[test]
fn pipeline_escalates_after_plan_review_reject_retries() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let err = with_fake_pipeline_tools(dir.path(), FakeScenario::RejectPlan, || {
        run_pipeline(ctx, &options)
    })
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("pipeline escalated after 3 attempts on plan_review->plan_creation"),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn pipeline_recovers_after_plan_review_retry() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let log_path = dir.path().join("observations.jsonl");
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = log_path.clone();
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let result = with_fake_pipeline_tools(dir.path(), FakeScenario::RejectThenApprovePlan, || {
        run_pipeline(ctx, &options)
    })
    .unwrap();

    assert_eq!(
        result.code_review.unwrap().decision,
        ReviewDecision::Approve
    );
    let log = std::fs::read_to_string(log_path).unwrap();
    assert!(log.contains("plan_review->plan_creation"), "{log}");
}

#[cfg(unix)]
#[test]
fn pipeline_escalates_after_test_fail_retries() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-fail".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let err = with_fake_pipeline_tools(dir.path(), FakeScenario::Happy, || {
        run_pipeline(ctx, &options)
    })
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("pipeline escalated after 3 attempts on test_execution->implementation"),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn pipeline_recovers_after_test_retry_and_passes_last_error_back_to_implementation() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-flaky".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let result = with_fake_pipeline_tools(dir.path(), FakeScenario::Happy, || {
        run_pipeline(ctx, &options)
    })
    .unwrap();

    assert_eq!(
        result.code_review.unwrap().decision,
        ReviewDecision::Approve
    );
    let second_prompt = std::fs::read_to_string(dir.path().join("implement-prompt-2.txt")).unwrap();
    assert!(second_prompt.contains("gate failed"), "{second_prompt}");
}

#[cfg(unix)]
#[test]
fn pipeline_publish_mode_pushes_and_reuses_existing_pr() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    options.publish = true;
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let result = with_fake_pipeline_tools(dir.path(), FakeScenario::ExistingPr, || {
        run_pipeline(ctx, &options)
    })
    .unwrap();

    assert!(!result.dry_run);
    assert_eq!(
        result.pr_url.as_deref(),
        Some("https://github.com/owner/repo/pull/7")
    );

    let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    assert!(calls.contains("git add -A"), "{calls}");
    assert!(calls.contains("git commit -m Fix issue #10"), "{calls}");
    assert!(
        calls.contains("git push -u origin sherpa/issue-10"),
        "{calls}"
    );
    assert!(!calls.contains("gh pr create"), "{calls}");
}

#[cfg(unix)]
#[test]
fn pipeline_publish_mode_creates_pr_when_none_exists() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    options.publish = true;
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let result = with_fake_pipeline_tools(dir.path(), FakeScenario::Happy, || {
        run_pipeline(ctx, &options)
    })
    .unwrap();

    assert!(!result.dry_run);
    assert_eq!(
        result.pr_url.as_deref(),
        Some("https://github.com/owner/repo/pull/8")
    );

    let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    assert!(calls.contains("git add -A"), "{calls}");
    assert!(calls.contains("git commit -m Fix issue #10"), "{calls}");
    assert!(
        calls.contains("git push -u origin sherpa/issue-10"),
        "{calls}"
    );
    assert!(calls.contains("gh pr create"), "{calls}");
    assert!(calls.contains("--base main"), "{calls}");
}

#[cfg(unix)]
#[test]
fn pipeline_fails_loud_when_issue_body_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let err = with_fake_pipeline_tools(dir.path(), FakeScenario::MissingIssueBody, || {
        run_pipeline(ctx, &options)
    })
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("IssueFetch JSON missing string field 'body'"),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn pipeline_fails_loud_when_existing_pr_json_lacks_url() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let err = with_fake_pipeline_tools(dir.path(), FakeScenario::MalformedExistingPr, || {
        run_pipeline(ctx, &options)
    })
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("PrCreation existing PR item missing string url"),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn pipeline_fails_after_code_review_changes_requested() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut options = PipelineOptions::new(
        &repo,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/prompts"),
    );
    options.log_path = dir.path().join("observations.jsonl");
    options.test_commands = vec![vec!["gate-ok".into()]];
    options.command_timeout = Duration::from_secs(10);
    options.agent_timeout = Duration::from_secs(10);
    let ctx = PipelineContext::new(10, "owner/repo", repo.display().to_string());

    let err = with_fake_pipeline_tools(dir.path(), FakeScenario::CodeReviewChanges, || {
        run_pipeline(ctx, &options)
    })
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("CodeReview did not approve: changes_requested: fix code"),
        "{err:#}"
    );
    let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    assert!(!calls.contains("gh pr create"), "{calls}");
}

#[test]
fn cli_runs_pipeline_in_dry_run_mode() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let prompts = repo.join("docs/prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("plan.md"), "planning agent {{issue_title}}").unwrap();
    std::fs::write(
        prompts.join("plan-review.md"),
        "Review the proposed implementation plan {{plan}}",
    )
    .unwrap();
    std::fs::write(
        prompts.join("implement.md"),
        "Implement the following plan {{plan}} {{last_error}}",
    )
    .unwrap();
    std::fs::write(prompts.join("code-review.md"), "code review agent {{diff}}").unwrap();
    run_git(&["init"], &repo);
    run_git(
        &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        &repo,
    );

    let output = with_fake_pipeline_tools(dir.path(), FakeScenario::Happy, || {
        Command::new(env!("CARGO_BIN_EXE_sherpa"))
            .arg("123")
            .current_dir(&repo)
            .output()
            .unwrap()
    });

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("sherpa: issue #123 in owner/repo completed through code_review"),
        "{stderr}"
    );
    assert!(stderr.contains("dry_run=true"), "{stderr}");
    assert!(stderr.contains("code_review: Approve"), "{stderr}");
}

#[test]
fn cli_publish_flags_trigger_publish_mode() {
    for flag in ["--publish", "--yes"] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let prompts = repo.join("docs/prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("plan.md"), "planning agent {{issue_title}}").unwrap();
        std::fs::write(
            prompts.join("plan-review.md"),
            "Review the proposed implementation plan {{plan}}",
        )
        .unwrap();
        std::fs::write(
            prompts.join("implement.md"),
            "Implement the following plan {{plan}} {{last_error}}",
        )
        .unwrap();
        std::fs::write(prompts.join("code-review.md"), "code review agent {{diff}}").unwrap();
        run_git(&["init"], &repo);
        run_git(
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
            &repo,
        );

        let output = with_fake_pipeline_tools(dir.path(), FakeScenario::Happy, || {
            Command::new(env!("CARGO_BIN_EXE_sherpa"))
                .arg("123")
                .arg(flag)
                .current_dir(&repo)
                .output()
                .unwrap()
        });

        assert!(
            output.status.success(),
            "{flag} stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("dry_run=false"), "{stderr}");
        assert!(
            stderr.contains("pr: https://github.com/owner/repo/pull/8"),
            "{stderr}"
        );
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert!(
            calls.contains("git push -u origin sherpa/issue-123"),
            "{calls}"
        );
        assert!(calls.contains("gh pr create"), "{calls}");
        assert!(calls.contains("--base main"), "{calls}");
    }
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
fn cli_does_not_echo_unsupported_origin_url() {
    let dir = tempfile::tempdir().unwrap();
    run_git(&["init"], dir.path());
    run_git(
        &[
            "remote",
            "add",
            "origin",
            "https://token@example.com/owner/repo.git",
        ],
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
        stderr.contains("git origin remote must be a github.com owner/repo URL"),
        "{stderr}"
    );
    assert!(!stderr.contains("token"), "{stderr}");
    assert!(!stderr.contains("example.com"), "{stderr}");
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
#[derive(Clone, Copy)]
enum FakeScenario {
    Happy,
    RejectPlan,
    RejectThenApprovePlan,
    ExistingPr,
    MalformedExistingPr,
    MissingIssueBody,
    CodeReviewChanges,
}

#[cfg(unix)]
fn with_fake_pipeline_tools<T>(root: &Path, scenario: FakeScenario, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap();
    let bin_dir = root.join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    write_executable(
        &bin_dir.join("gh"),
        r#"#!/bin/sh
if [ -n "${SHERPA_CALL_LOG:-}" ]; then
  printf 'gh %s\n' "$*" >> "$SHERPA_CALL_LOG"
fi
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  case "${SHERPA_SCENARIO:-happy}" in
    missing_issue_body)
      printf '{"title":"Pipeline issue"}'
      ;;
    *)
      printf '{"title":"Pipeline issue","body":"Implement the skeleton"}'
      ;;
  esac
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  case "${SHERPA_SCENARIO:-happy}" in
    existing_pr)
      printf '[{"number":7,"url":"https://github.com/owner/repo/pull/7"}]'
      ;;
    malformed_existing_pr)
      printf '[{"number":7}]'
      ;;
    *)
      printf '[]'
      ;;
  esac
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  printf 'https://github.com/owner/repo/pull/8\n'
  exit 0
fi
printf 'unexpected gh args: %s\n' "$*" >&2
exit 1
"#,
    );
    write_executable(
        &bin_dir.join("git"),
        r#"#!/bin/sh
if [ -n "${SHERPA_CALL_LOG:-}" ]; then
  printf 'git %s\n' "$*" >> "$SHERPA_CALL_LOG"
fi
if [ "$1" = "status" ] && [ "$2" = "--porcelain" ]; then
  case "$PWD" in
    */.sherpa-worktrees/*)
      printf '?? implemented.txt\n'
      ;;
  esac
  exit 0
fi
if [ "$1" = "fetch" ] && [ "$2" = "--quiet" ] && [ "$3" = "origin" ]; then
  if [ "$4" != "refs/heads/main" ]; then
    printf 'unexpected fetch args: %s\n' "$*" >&2
    exit 1
  fi
  exit 0
fi
if [ "$1" = "rev-parse" ] && [ "$2" = "--verify" ]; then
  printf '0123456789abcdef0123456789abcdef01234567\n'
  exit 0
fi
if [ "$1" = "worktree" ] && [ "$2" = "add" ]; then
  case "$5" in
    sherpa/issue-*) ;;
    *)
      printf 'unexpected worktree branch: %s\n' "$*" >&2
      exit 1
      ;;
  esac
  if [ "$4" != "-b" ] || [ "$6" != "0123456789abcdef0123456789abcdef01234567" ]; then
    printf 'unexpected worktree add args: %s\n' "$*" >&2
    exit 1
  fi
  mkdir -p "$3"
  exit 0
fi
if [ "$1" = "add" ] && [ "$2" = "--intent-to-add" ] && [ "$3" = "." ]; then
  touch "$SHERPA_FAKE_ROOT/intent-to-add"
  exit 0
fi
if [ "$1" = "add" ]; then
  exit 0
fi
if [ "$1" = "commit" ]; then
  exit 0
fi
if [ "$1" = "push" ]; then
  exit 0
fi
if [ "$1" = "diff" ]; then
  if [ "$2" != "--no-ext-diff" ] || [ "$3" != "0123456789abcdef0123456789abcdef01234567" ]; then
    printf 'unexpected diff args: %s\n' "$*" >&2
    exit 1
  fi
  if [ ! -e "$SHERPA_FAKE_ROOT/intent-to-add" ]; then
    exit 0
  fi
  printf '%s\n' \
    'diff --git a/implemented.txt b/implemented.txt' \
    'new file mode 100644' \
    'index 0000000..8ab686e' \
    '--- /dev/null' \
    '+++ b/implemented.txt' \
    '@@ -0,0 +1 @@' \
    '+implemented'
  exit 0
fi
exec /usr/bin/git "$@"
"#,
    );
    let plan_review = match scenario {
        FakeScenario::Happy
        | FakeScenario::ExistingPr
        | FakeScenario::MalformedExistingPr
        | FakeScenario::MissingIssueBody
        | FakeScenario::CodeReviewChanges => r#"VERDICT: approve\nplan ok"#,
        FakeScenario::RejectPlan => r#"VERDICT: reject\nplan too broad"#,
        FakeScenario::RejectThenApprovePlan => r#"dynamic"#,
    };
    write_executable(
        &bin_dir.join("claude"),
        &format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'claude-test\n'
  exit 0
fi
prompt=$(cat)
case "$prompt" in
  *"planning agent"*)
    printf '%s' '{{"result":"plan v1"}}'
    ;;
  *"Review the proposed implementation plan"*)
    if [ "${{SHERPA_SCENARIO:-happy}}" = "reject_then_approve_plan" ]; then
      count_file="$SHERPA_FAKE_ROOT/plan-review-count"
      count=$(cat "$count_file" 2>/dev/null || printf '0')
      count=$((count + 1))
      printf '%s' "$count" > "$count_file"
      if [ "$count" -eq 1 ]; then
        printf '%s' '{{"result":"VERDICT: reject\nplan too broad"}}'
      else
        printf '%s' '{{"result":"VERDICT: approve\nplan ok"}}'
      fi
    else
      printf '%s' '{{"result":"{plan_review}"}}'
    fi
    ;;
  *"Implement the following plan"*)
    count_file="$SHERPA_FAKE_ROOT/implement-count"
    count=$(cat "$count_file" 2>/dev/null || printf '0')
    count=$((count + 1))
    printf '%s' "$count" > "$count_file"
    printf '%s' "$prompt" > "$SHERPA_FAKE_ROOT/implement-prompt-$count.txt"
    printf 'implemented\n' > implemented.txt
    printf '%s' '{{"result":"implemented"}}'
    ;;
  *"code review agent"*)
    printf '%s' "$prompt" > "$SHERPA_FAKE_ROOT/code-review-prompt.txt"
    if [ "${{SHERPA_SCENARIO:-happy}}" = "code_review_changes" ]; then
      printf '%s' '{{"result":"VERDICT: changes_requested\nfix code"}}'
    else
      printf '%s' '{{"result":"VERDICT: approve\ncode ok"}}'
    fi
    ;;
  *)
    printf 'unexpected prompt\n' >&2
    exit 1
    ;;
esac
"#
        ),
    );
    write_executable(
        &bin_dir.join("gate-ok"),
        r#"#!/bin/sh
exit 0
"#,
    );
    write_executable(
        &bin_dir.join("gate-fail"),
        r#"#!/bin/sh
printf 'gate failed\n' >&2
exit 1
"#,
    );
    write_executable(
        &bin_dir.join("gate-flaky"),
        r#"#!/bin/sh
count_file="$SHERPA_FAKE_ROOT/gate-count"
count=$(cat "$count_file" 2>/dev/null || printf '0')
count=$((count + 1))
printf '%s' "$count" > "$count_file"
if [ "$count" -eq 1 ]; then
  printf 'gate failed\n' >&2
  exit 1
fi
exit 0
"#,
    );
    write_executable(
        &bin_dir.join("cargo"),
        r#"#!/bin/sh
exit 0
"#,
    );

    let old_path = std::env::var_os("PATH");
    let old_call_log = std::env::var_os("SHERPA_CALL_LOG");
    let old_fake_root = std::env::var_os("SHERPA_FAKE_ROOT");
    let old_scenario = std::env::var_os("SHERPA_SCENARIO");
    let mut paths = vec![bin_dir];
    if let Some(path) = &old_path {
        paths.extend(std::env::split_paths(path));
    }
    let new_path = std::env::join_paths(paths).unwrap();
    let call_log = root.join("calls.log");
    let scenario_name = match scenario {
        FakeScenario::Happy => "happy",
        FakeScenario::RejectPlan => "reject_plan",
        FakeScenario::RejectThenApprovePlan => "reject_then_approve_plan",
        FakeScenario::ExistingPr => "existing_pr",
        FakeScenario::MalformedExistingPr => "malformed_existing_pr",
        FakeScenario::MissingIssueBody => "missing_issue_body",
        FakeScenario::CodeReviewChanges => "code_review_changes",
    };

    // SAFETY: tests that mutate PATH are serialized by ENV_LOCK.
    unsafe {
        std::env::set_var("PATH", new_path);
        std::env::set_var("SHERPA_CALL_LOG", call_log);
        std::env::set_var("SHERPA_FAKE_ROOT", root);
        std::env::set_var("SHERPA_SCENARIO", scenario_name);
    }
    let result = f();
    // SAFETY: tests that mutate PATH are serialized by ENV_LOCK.
    unsafe {
        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        match old_call_log {
            Some(path) => std::env::set_var("SHERPA_CALL_LOG", path),
            None => std::env::remove_var("SHERPA_CALL_LOG"),
        }
        match old_fake_root {
            Some(path) => std::env::set_var("SHERPA_FAKE_ROOT", path),
            None => std::env::remove_var("SHERPA_FAKE_ROOT"),
        }
        match old_scenario {
            Some(value) => std::env::set_var("SHERPA_SCENARIO", value),
            None => std::env::remove_var("SHERPA_SCENARIO"),
        }
    }

    result
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
