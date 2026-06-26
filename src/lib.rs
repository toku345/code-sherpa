//! code-sherpa pipeline primitives.
//!
//! Drives a GitHub Issue through planning, implementation, testing, and
//! review stages. This module holds the deterministic primitives the
//! pipeline manager builds on; stage orchestration is layered on top.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Error, Result, anyhow, bail};
use serde_json::json;
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
    pub fn new(
        issue_number: u64,
        repo: impl Into<String>,
        worktree_path: impl Into<String>,
    ) -> Self {
        Self {
            issue_number,
            repo: repo.into(),
            worktree_path: worktree_path.into(),
            ..Self::default()
        }
    }
}

/// Machine-readable decision emitted by review prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve,
    Reject,
    ChangesRequested,
}

impl ReviewDecision {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "approve" => Some(Self::Approve),
            "reject" => Some(Self::Reject),
            "changes_requested" => Some(Self::ChangesRequested),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::ChangesRequested => "changes_requested",
        }
    }
}

/// Parsed review verdict. Missing or ambiguous verdicts fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewVerdict {
    pub decision: ReviewDecision,
    pub reasons: Vec<String>,
}

/// Runtime options for the deterministic pipeline manager.
#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub repo_root: PathBuf,
    pub prompts_dir: PathBuf,
    pub log_path: PathBuf,
    pub publish: bool,
    pub max_retries: u8,
    pub command_timeout: Duration,
    pub agent_timeout: Duration,
    pub base_ref: String,
    pub test_commands: Vec<Vec<String>>,
}

impl PipelineOptions {
    pub fn new(repo_root: impl Into<PathBuf>, prompts_dir: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        Self {
            log_path: repo_root.join(".sherpa-observations.jsonl"),
            repo_root,
            prompts_dir: prompts_dir.into(),
            publish: false,
            max_retries: 3,
            command_timeout: DEFAULT_CMD_TIMEOUT,
            agent_timeout: DEFAULT_AGENT_TIMEOUT,
            base_ref: "HEAD".to_owned(),
            test_commands: vec![
                vec![
                    "cargo".into(),
                    "fmt".into(),
                    "--all".into(),
                    "--check".into(),
                ],
                vec![
                    "cargo".into(),
                    "clippy".into(),
                    "--all-targets".into(),
                    "--".into(),
                    "-D".into(),
                    "warnings".into(),
                ],
                vec!["cargo".into(), "test".into(), "--all".into()],
            ],
        }
    }
}

/// Final state returned after the v0 pipeline stops at CodeReview.
#[derive(Debug, Clone, Default)]
pub struct PipelineOutcome {
    pub context: PipelineContext,
    pub code_review: Option<ReviewVerdict>,
    pub pr_url: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy)]
enum StageOutcome {
    #[allow(dead_code)]
    Success,
    Failure,
    Partial,
}

impl StageOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Partial => "partial",
        }
    }
}

struct StageLog<'a> {
    stage: Stage,
    attempt: u8,
    outcome: StageOutcome,
    input_summary: &'a str,
    output_summary: Option<serde_json::Value>,
    error: Option<&'a str>,
    verdict: Option<(&'a str, &'a ReviewVerdict)>,
}

/// Parse a review verdict from either a JSON object or exactly one leading
/// `VERDICT: ...` contract line.
pub fn parse_review_verdict(raw: &str) -> Result<ReviewVerdict> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("review verdict is empty");
    }

    if trimmed.starts_with('{') {
        return parse_json_verdict(trimmed);
    }

    let verdict_lines: Vec<_> = raw
        .lines()
        .filter_map(|line| line.trim().strip_prefix("VERDICT:").map(str::trim))
        .collect();

    let [decision] = verdict_lines.as_slice() else {
        bail!(
            "review verdict must contain exactly one 'VERDICT: approve|reject|changes_requested' line"
        );
    };
    let decision = ReviewDecision::parse(decision)
        .ok_or_else(|| anyhow!("unknown review verdict: {decision}"))?;

    let reasons = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("VERDICT:"))
        .map(ToOwned::to_owned)
        .collect();

    Ok(ReviewVerdict { decision, reasons })
}

fn parse_json_verdict(raw: &str) -> Result<ReviewVerdict> {
    let data: serde_json::Value = serde_json::from_str(raw)
        .map_err(|_| anyhow!("review verdict JSON is invalid: {}", truncate(raw, 200)))?;
    let decision = data
        .get("verdict")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("review verdict JSON must contain string field 'verdict'"))?;
    let decision = ReviewDecision::parse(decision)
        .ok_or_else(|| anyhow!("unknown review verdict: {decision}"))?;
    let reasons = match data.get("reasons") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow!("review verdict reasons must be strings"))
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => bail!("review verdict JSON field 'reasons' must be an array"),
        None => Vec::new(),
    };
    Ok(ReviewVerdict { decision, reasons })
}

/// Run the v0 walking skeleton. It stops after CodeReview and does not merge.
pub fn run_pipeline(
    mut ctx: PipelineContext,
    options: &PipelineOptions,
) -> Result<PipelineOutcome> {
    let mut retries: HashMap<&'static str, u8> = HashMap::new();

    run_issue_fetch(&mut ctx, options)?;
    loop {
        run_plan_creation(&mut ctx, options)?;
        let verdict = run_plan_review(&ctx, options)?;
        if verdict.decision == ReviewDecision::Approve {
            break;
        }
        record_retry(
            options,
            Stage::PlanReview,
            "plan_review->plan_creation",
            &mut retries,
        )?;
    }

    run_branch_creation(&mut ctx, options)?;
    loop {
        run_implementation(&mut ctx, options)?;
        match run_test_execution(&mut ctx, options) {
            Ok(()) => break,
            Err(err) => {
                ctx.last_error = format!("{err:#}");
                log_stage(
                    options,
                    StageLog {
                        stage: Stage::TestExecution,
                        attempt: 1,
                        outcome: StageOutcome::Failure,
                        input_summary: "test gate failed",
                        output_summary: None,
                        error: Some(&ctx.last_error),
                        verdict: None,
                    },
                )?;
                record_retry(
                    options,
                    Stage::TestExecution,
                    "test_execution->implementation",
                    &mut retries,
                )?;
            }
        }
    }

    let pr_url = run_pr_creation(&ctx, options)?;
    let review = run_code_review(&ctx, options)?;
    Ok(PipelineOutcome {
        context: ctx,
        code_review: Some(review),
        pr_url,
        dry_run: !options.publish,
    })
}

fn record_retry(
    options: &PipelineOptions,
    stage: Stage,
    edge: &'static str,
    retries: &mut HashMap<&'static str, u8>,
) -> Result<()> {
    let attempt = retries.entry(edge).or_default();
    *attempt += 1;
    log_stage(
        options,
        StageLog {
            stage,
            attempt: *attempt,
            outcome: StageOutcome::Partial,
            input_summary: "retry edge",
            output_summary: Some(json!({ "edge": edge, "attempt": *attempt })),
            error: None,
            verdict: None,
        },
    )?;
    if *attempt >= options.max_retries {
        bail!("pipeline escalated after {attempt} attempts on {edge}");
    }
    Ok(())
}

fn log_stage(options: &PipelineOptions, log: StageLog<'_>) -> Result<()> {
    let started = Instant::now();
    let mut entry = json!({
        "timestamp": observation_timestamp(),
        "stage": log.stage.as_str(),
        "attempt": log.attempt,
        "input": log.input_summary,
        "output": log.output_summary.unwrap_or(serde_json::Value::Null),
        "outcome": log.outcome.as_str(),
        "error": log.error,
        "duration_ms": started.elapsed().as_millis(),
        "argv": null,
        "artifact": null,
        "retry_edge": null
    });
    if let Some((raw, parsed)) = log.verdict {
        entry["raw_verdict_text"] = json!(raw);
        entry["parsed_verdict"] = review_verdict_json(parsed);
    } else {
        entry["raw_verdict_text"] = json!(null);
        entry["parsed_verdict"] = json!(null);
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.log_path)
        .with_context(|| format!("cannot open observation log {}", options.log_path.display()))?;
    writeln!(file, "{entry}")?;
    Ok(())
}

fn review_verdict_json(verdict: &ReviewVerdict) -> serde_json::Value {
    json!({
        "decision": verdict.decision.as_str(),
        "reasons": verdict.reasons,
    })
}

fn observation_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix_ms:{}", duration.as_millis()),
        Err(_) => "unix_ms:0".to_owned(),
    }
}

fn run_issue_fetch(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let issue_number = ctx.issue_number.to_string();
    let output = run_cmd(
        &[
            "gh",
            "issue",
            "view",
            &issue_number,
            "--repo",
            &ctx.repo,
            "--json",
            "title,body",
        ],
        Some(&options.repo_root),
        options.command_timeout,
    )
    .context("IssueFetch failed")?;
    let data: serde_json::Value =
        serde_json::from_str(&output).context("IssueFetch returned invalid JSON")?;
    ctx.issue_title = data
        .get("title")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("IssueFetch JSON missing string field 'title'"))?
        .to_owned();
    ctx.issue_body = data
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    log_stage(
        options,
        StageLog {
            stage: Stage::IssueFetch,
            attempt: 1,
            outcome: StageOutcome::Success,
            input_summary: "fetch issue title and body",
            output_summary: Some(json!({
                "title": ctx.issue_title,
                "body_len": ctx.issue_body.len()
            })),
            error: None,
            verdict: None,
        },
    )?;
    Ok(())
}

fn run_plan_creation(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let issue_number = ctx.issue_number.to_string();
    let vars = HashMap::from([
        ("issue_number", issue_number.as_str()),
        ("issue_title", ctx.issue_title.as_str()),
        ("issue_body", ctx.issue_body.as_str()),
    ]);
    let prompt = load_prompt("plan.md", &options.prompts_dir, &vars)?;
    ctx.plan = run_agent(&prompt, Some(&options.repo_root), options.agent_timeout)
        .context("PlanCreation agent failed")?;
    log_stage(
        options,
        StageLog {
            stage: Stage::PlanCreation,
            attempt: 1,
            outcome: StageOutcome::Success,
            input_summary: "create implementation plan",
            output_summary: Some(json!({ "plan_len": ctx.plan.len() })),
            error: None,
            verdict: None,
        },
    )?;
    Ok(())
}

fn run_plan_review(ctx: &PipelineContext, options: &PipelineOptions) -> Result<ReviewVerdict> {
    let issue_number = ctx.issue_number.to_string();
    let vars = HashMap::from([
        ("issue_number", issue_number.as_str()),
        ("issue_title", ctx.issue_title.as_str()),
        ("issue_body", ctx.issue_body.as_str()),
        ("plan", ctx.plan.as_str()),
    ]);
    let prompt = load_prompt("plan-review.md", &options.prompts_dir, &vars)?;
    let raw = run_agent(&prompt, Some(&options.repo_root), options.agent_timeout)
        .context("PlanReview agent failed")?;
    let verdict = parse_review_verdict(&raw).context("PlanReview verdict parse failed")?;
    log_stage(
        options,
        StageLog {
            stage: Stage::PlanReview,
            attempt: 1,
            outcome: StageOutcome::Success,
            input_summary: "review implementation plan",
            output_summary: Some(json!({ "decision": verdict.decision.as_str() })),
            error: None,
            verdict: Some((&raw, &verdict)),
        },
    )?;
    Ok(verdict)
}

fn run_branch_creation(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let status = run_cmd(
        &["git", "status", "--porcelain"],
        Some(&options.repo_root),
        options.command_timeout,
    )
    .context("BranchCreation clean check failed")?;
    if !status.trim().is_empty() {
        bail!("BranchCreation requires a clean working tree");
    }

    let branch_name = format!("sherpa/issue-{}", ctx.issue_number);
    let worktree_parent = options
        .repo_root
        .parent()
        .ok_or_else(|| anyhow!("repo root has no parent directory"))?
        .join(".sherpa-worktrees");
    std::fs::create_dir_all(&worktree_parent)
        .with_context(|| format!("cannot create {}", worktree_parent.display()))?;
    let worktree_path = worktree_parent.join(format!("issue-{}", ctx.issue_number));
    if worktree_path.exists() {
        bail!(
            "BranchCreation target worktree already exists: {}",
            worktree_path.display()
        );
    }

    let worktree_arg = worktree_path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not UTF-8"))?;
    run_cmd(
        &[
            "git",
            "worktree",
            "add",
            worktree_arg,
            "-b",
            &branch_name,
            &options.base_ref,
        ],
        Some(&options.repo_root),
        options.command_timeout,
    )
    .context("BranchCreation git worktree add failed")?;

    ctx.branch_name = branch_name;
    ctx.worktree_path = worktree_path.display().to_string();
    log_stage(
        options,
        StageLog {
            stage: Stage::BranchCreation,
            attempt: 1,
            outcome: StageOutcome::Success,
            input_summary: "create isolated git worktree",
            output_summary: Some(json!({
                "branch": ctx.branch_name,
                "worktree_path": ctx.worktree_path,
                "base_ref": options.base_ref
            })),
            error: None,
            verdict: None,
        },
    )?;
    Ok(())
}

fn run_implementation(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let worktree = Path::new(&ctx.worktree_path);
    verify_agent_runtime(worktree, options)?;
    let vars = HashMap::from([
        ("plan", ctx.plan.as_str()),
        ("last_error", ctx.last_error.as_str()),
    ]);
    let prompt = load_prompt("implement.md", &options.prompts_dir, &vars)?;
    let result = run_agent(&prompt, Some(worktree), options.agent_timeout)
        .context("Implementation agent failed")?;
    log_stage(
        options,
        StageLog {
            stage: Stage::Implementation,
            attempt: 1,
            outcome: StageOutcome::Success,
            input_summary: "apply plan in isolated worktree",
            output_summary: Some(json!({ "result_len": result.len() })),
            error: None,
            verdict: None,
        },
    )?;
    Ok(())
}

fn run_test_execution(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let worktree = Path::new(&ctx.worktree_path);
    for cmd in &options.test_commands {
        run_cmd_owned(cmd, Some(worktree), options.command_timeout)
            .with_context(|| format!("TestExecution failed: {}", redacted_argv(cmd)))?;
    }
    log_stage(
        options,
        StageLog {
            stage: Stage::TestExecution,
            attempt: 1,
            outcome: StageOutcome::Success,
            input_summary: "run deterministic test gate",
            output_summary: Some(json!({
                "commands": options
                    .test_commands
                    .iter()
                    .map(|cmd| redacted_argv(cmd))
                    .collect::<Vec<_>>()
            })),
            error: None,
            verdict: None,
        },
    )?;
    Ok(())
}

fn run_pr_creation(ctx: &PipelineContext, options: &PipelineOptions) -> Result<Option<String>> {
    let title = format!("Fix #{}: {}", ctx.issue_number, ctx.issue_title);
    let body = format!(
        "Closes #{}\n\nGenerated by code-sherpa walking skeleton.",
        ctx.issue_number
    );
    let existing_pr = find_existing_pr(ctx, options)?;
    if options.publish {
        commit_worktree_changes(ctx, options)?;
        let push_cmd = vec![
            "git".to_owned(),
            "push".to_owned(),
            "-u".to_owned(),
            "origin".to_owned(),
            ctx.branch_name.clone(),
        ];
        run_cmd_owned(
            &push_cmd,
            Some(Path::new(&ctx.worktree_path)),
            options.command_timeout,
        )
        .context("PrCreation git push failed")?;

        if let Some(url) = existing_pr {
            log_pr_creation(options, ctx, &title, &body, false, Some(&url))?;
            return Ok(Some(url));
        }

        let pr_cmd = vec![
            "gh".to_owned(),
            "pr".to_owned(),
            "create".to_owned(),
            "--repo".to_owned(),
            ctx.repo.clone(),
            "--head".to_owned(),
            ctx.branch_name.clone(),
            "--title".to_owned(),
            title.clone(),
            "--body".to_owned(),
            body.clone(),
        ];
        let url = run_cmd_owned(
            &pr_cmd,
            Some(Path::new(&ctx.worktree_path)),
            options.command_timeout,
        )
        .context("PrCreation gh pr create failed")?
        .trim()
        .to_owned();
        if url.is_empty() {
            bail!("PrCreation gh pr create returned empty URL");
        }
        log_pr_creation(options, ctx, &title, &body, false, Some(&url))?;
        return Ok(Some(url));
    }

    log_pr_creation(options, ctx, &title, &body, true, existing_pr.as_deref())?;
    Ok(existing_pr)
}

fn commit_worktree_changes(ctx: &PipelineContext, options: &PipelineOptions) -> Result<()> {
    let worktree = Path::new(&ctx.worktree_path);
    let status = run_cmd(
        &["git", "status", "--porcelain"],
        Some(worktree),
        options.command_timeout,
    )
    .context("PrCreation git status failed")?;
    if status.trim().is_empty() {
        bail!("PrCreation found no worktree changes to publish");
    }

    run_cmd(
        &["git", "add", "-A"],
        Some(worktree),
        options.command_timeout,
    )
    .context("PrCreation git add failed")?;
    let subject = format!("Fix issue #{}", ctx.issue_number);
    let body =
        "Generated by code-sherpa walking skeleton.\n\nCo-authored-by: Codex <noreply@openai.com>";
    run_cmd(
        &["git", "commit", "-m", &subject, "-m", body],
        Some(worktree),
        options.command_timeout,
    )
    .context("PrCreation git commit failed")?;
    Ok(())
}

fn find_existing_pr(ctx: &PipelineContext, options: &PipelineOptions) -> Result<Option<String>> {
    let output = run_cmd(
        &[
            "gh",
            "pr",
            "list",
            "--repo",
            &ctx.repo,
            "--head",
            &ctx.branch_name,
            "--json",
            "number,url",
            "--limit",
            "1",
        ],
        Some(Path::new(&ctx.worktree_path)),
        options.command_timeout,
    )
    .context("PrCreation existing PR check failed")?;
    let data: serde_json::Value =
        serde_json::from_str(&output).context("PrCreation existing PR JSON invalid")?;
    let Some(items) = data.as_array() else {
        bail!("PrCreation existing PR JSON must be an array");
    };
    Ok(items.first().and_then(|item| {
        item.get("url")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    }))
}

fn log_pr_creation(
    options: &PipelineOptions,
    ctx: &PipelineContext,
    title: &str,
    body: &str,
    dry_run: bool,
    existing_or_created_url: Option<&str>,
) -> Result<()> {
    let push_cmd = vec![
        "git".to_owned(),
        "push".to_owned(),
        "-u".to_owned(),
        "origin".to_owned(),
        ctx.branch_name.clone(),
    ];
    let add_cmd = vec!["git".to_owned(), "add".to_owned(), "-A".to_owned()];
    let commit_cmd = vec![
        "git".to_owned(),
        "commit".to_owned(),
        "-m".to_owned(),
        format!("Fix issue #{}", ctx.issue_number),
        "-m".to_owned(),
        "Generated by code-sherpa walking skeleton.\n\nCo-authored-by: Codex <noreply@openai.com>"
            .to_owned(),
    ];
    let pr_cmd = vec![
        "gh".to_owned(),
        "pr".to_owned(),
        "create".to_owned(),
        "--repo".to_owned(),
        ctx.repo.clone(),
        "--head".to_owned(),
        ctx.branch_name.clone(),
        "--title".to_owned(),
        title.to_owned(),
        "--body".to_owned(),
        body.to_owned(),
    ];
    log_stage(
        options,
        StageLog {
            stage: Stage::PrCreation,
            attempt: 1,
            outcome: if dry_run {
                StageOutcome::Partial
            } else {
                StageOutcome::Success
            },
            input_summary: "PR creation gate",
            output_summary: Some(json!({
                "dry_run": dry_run,
                "branch": ctx.branch_name,
                "repo": ctx.repo,
                "title": title,
                "body": body,
                "url": existing_or_created_url,
                "argv": [
                    redacted_argv(&add_cmd),
                    redacted_argv(&commit_cmd),
                    redacted_argv(&push_cmd),
                    redacted_argv(&pr_cmd)
                ],
            })),
            error: None,
            verdict: None,
        },
    )?;
    Ok(())
}

fn run_code_review(ctx: &PipelineContext, options: &PipelineOptions) -> Result<ReviewVerdict> {
    let worktree = Path::new(&ctx.worktree_path);
    let diff = run_cmd(
        &["git", "diff", "--stat", &options.base_ref],
        Some(worktree),
        options.command_timeout,
    )
    .unwrap_or_else(|err| format!("diff unavailable: {err:#}"));
    let issue_number = ctx.issue_number.to_string();
    let vars = HashMap::from([
        ("issue_number", issue_number.as_str()),
        ("issue_title", ctx.issue_title.as_str()),
        ("issue_body", ctx.issue_body.as_str()),
        ("plan", ctx.plan.as_str()),
        ("diff", diff.as_str()),
    ]);
    let prompt = load_prompt("code-review.md", &options.prompts_dir, &vars)?;
    let raw = run_agent(&prompt, Some(worktree), options.agent_timeout)
        .context("CodeReview agent failed")?;
    let verdict = parse_review_verdict(&raw).context("CodeReview verdict parse failed")?;
    log_stage(
        options,
        StageLog {
            stage: Stage::CodeReview,
            attempt: 1,
            outcome: StageOutcome::Success,
            input_summary: "review final worktree diff and stop",
            output_summary: Some(json!({ "decision": verdict.decision.as_str() })),
            error: None,
            verdict: Some((&raw, &verdict)),
        },
    )?;
    Ok(verdict)
}

fn verify_agent_runtime(cwd: &Path, options: &PipelineOptions) -> Result<()> {
    run_cmd(&["claude", "--version"], Some(cwd), options.command_timeout)
        .context("Implementation requires claude on PATH")?;

    let check_path = cwd.join(".sherpa-write-check");
    std::fs::write(&check_path, "ok")
        .with_context(|| format!("Implementation cwd is not writable: {}", cwd.display()))?;
    std::fs::remove_file(&check_path)
        .with_context(|| format!("cannot remove {}", check_path.display()))?;
    Ok(())
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

fn run_cmd_owned(cmd: &[String], cwd: Option<&Path>, timeout: Duration) -> Result<String> {
    let refs: Vec<_> = cmd.iter().map(String::as_str).collect();
    run_cmd(&refs, cwd, timeout)
}

fn redacted_argv(cmd: &[String]) -> String {
    cmd.iter()
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            if lower.contains("token") || lower.contains("password") || lower.contains("secret") {
                "<redacted>".to_owned()
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
