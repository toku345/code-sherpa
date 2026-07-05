//! code-sherpa pipeline primitives and v0 deterministic manager.
//!
//! Drives a GitHub Issue through planning, implementation, testing, and
//! review stages. This module holds the deterministic primitives and the
//! walking-skeleton stage orchestration built on top of them.

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
const CLAUDE_AGENT_SETTINGS: &str = r#"{"sandbox":{"enabled":true,"allowUnsandboxedCommands":false,"network":{"allowedDomains":["api.anthropic.com","*.anthropic.com","github.com","*.githubusercontent.com","*.npmjs.org","pypi.org","files.pythonhosted.org"]},"filesystem":{"denyRead":["~/.aws/credentials","~/.ssh"]}},"permissions":{"deny":["Bash(rm -rf *)","Bash(chmod 777 *)","Read(./.env)","Read(./.env.*)","Read(**/*.pem)","Read(**/*.key)"]}}"#;

/// A stage in the pipeline, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    IssueFetch,
    PlanCreation,
    PlanReview,
    BranchCreation,
    Implementation,
    TestExecution,
    CodeReview,
    PrCreation,
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
        Stage::CodeReview,
        Stage::PrCreation,
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
            Stage::CodeReview => "code_review",
            Stage::PrCreation => "pr_creation",
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
    pub base_commit: String,
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
        let log_path = repo_root
            .parent()
            .map(|parent| parent.join(".sherpa-worktrees").join("observations.jsonl"))
            .unwrap_or_else(|| repo_root.join("sherpa-observations.jsonl"));
        Self {
            log_path,
            repo_root,
            prompts_dir: prompts_dir.into(),
            publish: false,
            max_retries: 3,
            command_timeout: DEFAULT_CMD_TIMEOUT,
            agent_timeout: DEFAULT_AGENT_TIMEOUT,
            base_ref: "origin/main".to_owned(),
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

/// Final state returned after the v0 pipeline passes CodeReview.
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
    duration_ms: u128,
}

struct CodeReviewResult {
    verdict: ReviewVerdict,
    reviewed_diff: String,
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

    let first_line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("review verdict is empty"))?;
    let Some(decision) = first_line.strip_prefix("VERDICT:").map(str::trim) else {
        bail!(
            "review verdict first non-empty line must be 'VERDICT: approve|reject|changes_requested'"
        );
    };
    let decision = ReviewDecision::parse(decision)
        .ok_or_else(|| anyhow!("unknown review verdict: {decision}"))?;
    let verdict_count = raw
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("VERDICT:"))
        .count();
    if verdict_count != 1 {
        bail!(
            "review verdict must contain exactly one 'VERDICT: approve|reject|changes_requested' line"
        );
    }

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

/// Run the v0 walking skeleton. It passes CodeReview and does not merge.
pub fn run_pipeline(
    mut ctx: PipelineContext,
    options: &PipelineOptions,
) -> Result<PipelineOutcome> {
    validate_options(options)?;
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
            Some(&review_reasons(&verdict)),
        )?;
    }

    run_branch_creation(&mut ctx, options)?;
    loop {
        run_implementation(&mut ctx, options)?;
        let test_started = Instant::now();
        match run_test_execution(&mut ctx, options) {
            Ok(()) => break,
            Err(err) => {
                ctx.last_error = format!("{err:#}");
                if let Err(log_err) = log_stage(
                    options,
                    StageLog {
                        stage: Stage::TestExecution,
                        attempt: 1,
                        outcome: StageOutcome::Failure,
                        input_summary: "test gate failed",
                        output_summary: None,
                        error: Some(&ctx.last_error),
                        verdict: None,
                        duration_ms: test_started.elapsed().as_millis(),
                    },
                ) {
                    bail!(
                        "TestExecution failed: {}; failed to write observation log: {log_err:#}",
                        ctx.last_error
                    );
                }
                record_retry(
                    options,
                    Stage::TestExecution,
                    "test_execution->implementation",
                    &mut retries,
                    Some(&ctx.last_error),
                )?;
            }
        }
    }

    let review = run_code_review(&ctx, options)?;
    if review.verdict.decision != ReviewDecision::Approve {
        let reasons = review_reasons(&review.verdict);
        bail!(
            "CodeReview did not approve: {}: {}",
            review.verdict.decision.as_str(),
            reasons
        );
    }
    let pr_url = run_pr_creation(&ctx, options, &review.reviewed_diff)?;
    Ok(PipelineOutcome {
        context: ctx,
        code_review: Some(review.verdict),
        pr_url,
        dry_run: !options.publish,
    })
}

fn validate_options(options: &PipelineOptions) -> Result<()> {
    if options.max_retries == 0 {
        bail!("PipelineOptions max_retries must be greater than zero");
    }
    if options.test_commands.is_empty() {
        bail!("PipelineOptions test_commands must not be empty");
    }
    if options.test_commands.iter().any(Vec::is_empty) {
        bail!("PipelineOptions test_commands must not contain empty commands");
    }
    if options.publish {
        pr_base_branch(options)?;
    }
    Ok(())
}

fn record_retry(
    options: &PipelineOptions,
    stage: Stage,
    edge: &'static str,
    retries: &mut HashMap<&'static str, u8>,
    detail: Option<&str>,
) -> Result<()> {
    let started = Instant::now();
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
            error: detail,
            verdict: None,
            duration_ms: started.elapsed().as_millis(),
        },
    )?;
    if *attempt >= options.max_retries {
        if let Some(detail) = detail {
            bail!("pipeline escalated after {attempt} attempts on {edge}: {detail}");
        }
        bail!("pipeline escalated after {attempt} attempts on {edge}");
    }
    Ok(())
}

fn log_stage(options: &PipelineOptions, log: StageLog<'_>) -> Result<()> {
    let mut entry = json!({
        "timestamp": observation_timestamp(),
        "stage": log.stage.as_str(),
        "attempt": log.attempt,
        "input": log.input_summary,
        "output": log.output_summary.unwrap_or(serde_json::Value::Null),
        "outcome": log.outcome.as_str(),
        "error": log.error,
        "duration_ms": log.duration_ms,
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

    if let Some(parent) = options.log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create observation log dir {}", parent.display()))?;
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
    let started = Instant::now();
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
        .ok_or_else(|| anyhow!("IssueFetch JSON missing string field 'body'"))?
        .as_str()
        .ok_or_else(|| anyhow!("IssueFetch JSON missing string field 'body'"))?
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
            duration_ms: started.elapsed().as_millis(),
        },
    )?;
    Ok(())
}

fn run_plan_creation(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let started = Instant::now();
    let issue_number = ctx.issue_number.to_string();
    let vars = HashMap::from([
        ("issue_number", issue_number.as_str()),
        ("issue_title", ctx.issue_title.as_str()),
        ("issue_body", ctx.issue_body.as_str()),
    ]);
    let prompt = load_prompt("plan.md", &options.prompts_dir, &vars)?;
    ctx.plan = run_review_agent(&prompt, options).context("PlanCreation agent failed")?;
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
            duration_ms: started.elapsed().as_millis(),
        },
    )?;
    Ok(())
}

fn run_plan_review(ctx: &PipelineContext, options: &PipelineOptions) -> Result<ReviewVerdict> {
    let started = Instant::now();
    let issue_number = ctx.issue_number.to_string();
    let vars = HashMap::from([
        ("issue_number", issue_number.as_str()),
        ("issue_title", ctx.issue_title.as_str()),
        ("issue_body", ctx.issue_body.as_str()),
        ("plan", ctx.plan.as_str()),
    ]);
    let prompt = load_prompt("plan-review.md", &options.prompts_dir, &vars)?;
    let raw = run_review_agent(&prompt, options).context("PlanReview agent failed")?;
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
            duration_ms: started.elapsed().as_millis(),
        },
    )?;
    Ok(verdict)
}

fn run_branch_creation(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let started = Instant::now();
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
    let base_commit = resolve_base_commit(options)?;
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
            &base_commit,
        ],
        Some(&options.repo_root),
        options.command_timeout,
    )
    .context("BranchCreation git worktree add failed")?;

    ctx.branch_name = branch_name;
    ctx.base_commit = base_commit;
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
                "base_ref": options.base_ref,
                "base_commit": ctx.base_commit
            })),
            error: None,
            verdict: None,
            duration_ms: started.elapsed().as_millis(),
        },
    )?;
    Ok(())
}

fn run_implementation(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let started = Instant::now();
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
            duration_ms: started.elapsed().as_millis(),
        },
    )?;
    Ok(())
}

fn run_test_execution(ctx: &mut PipelineContext, options: &PipelineOptions) -> Result<()> {
    let started = Instant::now();
    let worktree = Path::new(&ctx.worktree_path);
    for cmd in &options.test_commands {
        run_isolated_test_cmd(cmd, worktree, options)
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
            duration_ms: started.elapsed().as_millis(),
        },
    )?;
    Ok(())
}

fn run_pr_creation(
    ctx: &PipelineContext,
    options: &PipelineOptions,
    reviewed_diff: &str,
) -> Result<Option<String>> {
    let started = Instant::now();
    let title = format!("Fix #{}: {}", ctx.issue_number, ctx.issue_title);
    let body = format!(
        "Closes #{}\n\nGenerated by code-sherpa walking skeleton.",
        ctx.issue_number
    );
    if options.publish {
        pr_base_branch(options)?;
        verify_reviewed_diff_unchanged(ctx, options, reviewed_diff)?;
    }
    let existing_pr = find_existing_pr(ctx, options)?;
    if options.publish {
        commit_reviewed_worktree_changes(ctx, options, reviewed_diff)?;
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
            log_pr_creation(
                options,
                ctx,
                &title,
                &body,
                false,
                Some(&url),
                started.elapsed().as_millis(),
            )
                .with_context(|| {
                    format!("PrCreation pushed branch for existing PR {url} but failed to write observation log")
                })?;
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
            "--base".to_owned(),
            pr_base_branch(options)?.to_owned(),
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
        log_pr_creation(
            options,
            ctx,
            &title,
            &body,
            false,
            Some(&url),
            started.elapsed().as_millis(),
        )
        .with_context(|| {
            format!("PrCreation created PR {url} but failed to write observation log")
        })?;
        return Ok(Some(url));
    }

    log_pr_creation(
        options,
        ctx,
        &title,
        &body,
        true,
        existing_pr.as_deref(),
        started.elapsed().as_millis(),
    )?;
    Ok(existing_pr)
}

fn verify_reviewed_diff_unchanged(
    ctx: &PipelineContext,
    options: &PipelineOptions,
    reviewed_diff: &str,
) -> Result<()> {
    let current_diff = collect_code_review_diff(ctx, options)?;
    if current_diff != reviewed_diff {
        bail!("PrCreation worktree diff changed after CodeReview approval");
    }
    Ok(())
}

fn commit_reviewed_worktree_changes(
    ctx: &PipelineContext,
    options: &PipelineOptions,
    reviewed_diff: &str,
) -> Result<()> {
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
    let staged_diff = run_cmd(
        &["git", "diff", "--cached", "--no-ext-diff", &ctx.base_commit],
        Some(worktree),
        options.command_timeout,
    )
    .context("PrCreation failed to collect staged diff")?;
    if staged_diff != reviewed_diff {
        bail!("PrCreation staged diff changed after CodeReview approval");
    }
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
    let Some(item) = items.first() else {
        return Ok(None);
    };
    let url = item
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow!(
                "PrCreation existing PR item missing string url: {}",
                truncate(&item.to_string(), 200)
            )
        })?;
    Ok(Some(url.to_owned()))
}

fn log_pr_creation(
    options: &PipelineOptions,
    ctx: &PipelineContext,
    title: &str,
    body: &str,
    dry_run: bool,
    existing_or_created_url: Option<&str>,
    duration_ms: u128,
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
        "--base".to_owned(),
        pr_base_branch(options)?.to_owned(),
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
            duration_ms,
        },
    )?;
    Ok(())
}

fn run_code_review(ctx: &PipelineContext, options: &PipelineOptions) -> Result<CodeReviewResult> {
    let started = Instant::now();
    let diff = collect_code_review_diff(ctx, options)?;
    let issue_number = ctx.issue_number.to_string();
    let vars = HashMap::from([
        ("issue_number", issue_number.as_str()),
        ("issue_title", ctx.issue_title.as_str()),
        ("issue_body", ctx.issue_body.as_str()),
        ("plan", ctx.plan.as_str()),
        ("diff", diff.as_str()),
    ]);
    let prompt = load_prompt("code-review.md", &options.prompts_dir, &vars)?;
    let raw = run_review_agent(&prompt, options).context("CodeReview agent failed")?;
    let verdict = parse_review_verdict(&raw).context("CodeReview verdict parse failed")?;
    if let Err(log_err) = log_stage(
        options,
        StageLog {
            stage: Stage::CodeReview,
            attempt: 1,
            outcome: if verdict.decision == ReviewDecision::Approve {
                StageOutcome::Success
            } else {
                StageOutcome::Failure
            },
            input_summary: "review final worktree diff and stop",
            output_summary: Some(json!({ "decision": verdict.decision.as_str() })),
            error: None,
            verdict: Some((&raw, &verdict)),
            duration_ms: started.elapsed().as_millis(),
        },
    ) {
        let reasons = review_reasons(&verdict);
        bail!(
            "CodeReview produced verdict {}: {}; failed to write observation log: {log_err:#}",
            verdict.decision.as_str(),
            reasons
        );
    }
    Ok(CodeReviewResult {
        verdict,
        reviewed_diff: diff,
    })
}

fn collect_code_review_diff(ctx: &PipelineContext, options: &PipelineOptions) -> Result<String> {
    let worktree = Path::new(&ctx.worktree_path);
    run_cmd(
        &["git", "add", "--intent-to-add", "."],
        Some(worktree),
        options.command_timeout,
    )
    .context("CodeReview failed to mark untracked files for diff")?;
    let diff = run_cmd(
        &["git", "diff", "--no-ext-diff", &ctx.base_commit],
        Some(worktree),
        options.command_timeout,
    )
    .context("CodeReview failed to collect worktree diff")?;
    if diff.trim().is_empty() {
        bail!("CodeReview collected an empty diff");
    }
    Ok(diff)
}

fn resolve_base_commit(options: &PipelineOptions) -> Result<String> {
    if let Some(branch) = origin_base_branch(options)? {
        run_cmd(
            &[
                "git",
                "fetch",
                "--quiet",
                "origin",
                &format!("refs/heads/{branch}"),
            ],
            Some(&options.repo_root),
            options.command_timeout,
        )
        .with_context(|| format!("BranchCreation failed to fetch origin/{branch}"))?;
        return run_cmd(
            &["git", "rev-parse", "--verify", "FETCH_HEAD^{commit}"],
            Some(&options.repo_root),
            options.command_timeout,
        )
        .context("BranchCreation failed to resolve fetched base commit")
        .map(|output| output.trim().to_owned());
    }

    run_cmd(
        &[
            "git",
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", options.base_ref),
        ],
        Some(&options.repo_root),
        options.command_timeout,
    )
    .context("BranchCreation failed to resolve base commit")
    .map(|output| output.trim().to_owned())
}

fn pr_base_branch(options: &PipelineOptions) -> Result<&str> {
    origin_base_branch(options)?.ok_or_else(|| {
        anyhow!("PipelineOptions base_ref must be an origin/<branch> ref for PR creation")
    })
}

fn origin_base_branch(options: &PipelineOptions) -> Result<Option<&str>> {
    let Some(branch) = options.base_ref.strip_prefix("origin/") else {
        return Ok(None);
    };
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('+')
        || branch.contains(':')
        || branch.contains("..")
        || branch.contains('\\')
    {
        bail!("PipelineOptions base_ref must be a safe origin branch ref");
    }
    Ok(Some(branch))
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
                bail!("{label}: {}", command_failure_detail(&stdout, &stderr))
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

fn command_failure_detail(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "command failed without output".to_owned(),
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (false, false) => format!("command failed\nstderr:\n{stderr}\nstdout:\n{stdout}"),
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

fn run_isolated_test_cmd(cmd: &[String], worktree: &Path, options: &PipelineOptions) -> Result<()> {
    if cmd.is_empty() {
        bail!("TestExecution command must not be empty");
    }

    let sandbox_cmd = sandboxed_test_command(cmd, worktree, options)?;
    let label = sandbox_cmd
        .first()
        .map(String::as_str)
        .unwrap_or("test sandbox");
    run_cmd_owned_with_env(
        &sandbox_cmd,
        Some(worktree),
        options.command_timeout,
        sandbox_env(worktree)?,
        label,
    )?;
    Ok(())
}

fn sandboxed_test_command(
    cmd: &[String],
    worktree: &Path,
    options: &PipelineOptions,
) -> Result<Vec<String>> {
    if cfg!(target_os = "macos") {
        let mut sandbox_cmd = vec![
            "sandbox-exec".to_owned(),
            "-p".to_owned(),
            test_sandbox_profile(worktree, options),
            "--".to_owned(),
        ];
        sandbox_cmd.extend(cmd.iter().cloned());
        return Ok(sandbox_cmd);
    }
    if cfg!(target_os = "linux") {
        return Ok(linux_sandboxed_test_command(cmd, worktree));
    }
    bail!(
        "TestExecution requires a supported sandbox backend; macOS sandbox-exec and Linux bwrap are currently supported"
    );
}

fn sandbox_env(worktree: &Path) -> Result<Vec<(String, String)>> {
    let home = worktree.join(".sherpa-sandbox-home");
    let tmp = home.join("tmp");
    std::fs::create_dir_all(&tmp)
        .with_context(|| format!("cannot create sandbox home {}", home.display()))?;

    let mut envs = vec![
        ("HOME".to_owned(), home.display().to_string()),
        ("TMPDIR".to_owned(), tmp.display().to_string()),
        (
            "CARGO_TARGET_DIR".to_owned(),
            worktree.join("target").display().to_string(),
        ),
        ("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()),
    ];
    for key in [
        "PATH",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "SHERPA_CALL_LOG",
        "SHERPA_FAKE_ROOT",
        "SHERPA_LOG_PATH",
        "SHERPA_SCENARIO",
    ] {
        if let Some(value) = std::env::var_os(key).and_then(|value| value.into_string().ok()) {
            envs.push((key.to_owned(), value));
        }
    }
    if !envs.iter().any(|(key, _)| key == "CARGO_HOME")
        && let Some(home_dir) = std::env::var_os("HOME").and_then(|value| value.into_string().ok())
    {
        envs.push(("CARGO_HOME".to_owned(), format!("{home_dir}/.cargo")));
    }
    if !envs.iter().any(|(key, _)| key == "RUSTUP_HOME")
        && let Some(home_dir) = std::env::var_os("HOME").and_then(|value| value.into_string().ok())
    {
        envs.push(("RUSTUP_HOME".to_owned(), format!("{home_dir}/.rustup")));
    }
    Ok(envs)
}

fn run_cmd_owned_with_env(
    cmd: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    envs: Vec<(String, String)>,
    label: &str,
) -> Result<String> {
    let Some((program, args)) = cmd.split_first() else {
        bail!("{label}: empty command");
    };
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.env_clear();
    for (key, value) in envs {
        command.env(key, value);
    }
    capture(command, None, timeout, label)
}

fn test_sandbox_profile(worktree: &Path, _options: &PipelineOptions) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut read_subpaths = vec![
        "/bin".to_owned(),
        "/dev".to_owned(),
        "/Library".to_owned(),
        "/System".to_owned(),
        "/usr".to_owned(),
        "/private/tmp".to_owned(),
        "/private/var/folders".to_owned(),
        worktree.display().to_string(),
    ];
    if !home.is_empty() {
        read_subpaths.push(format!("{home}/.cargo/bin"));
        read_subpaths.push(format!("{home}/.cargo/git"));
        read_subpaths.push(format!("{home}/.cargo/registry"));
        read_subpaths.push(format!("{home}/.rustup/toolchains"));
    }

    let read_rules = read_subpaths
        .iter()
        .map(|path| format!("  (subpath \"{}\")", sandbox_escape(path)))
        .collect::<Vec<_>>()
        .join("\n");
    let worktree = sandbox_escape(&worktree.display().to_string());
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow mach-lookup)
(allow file-read-metadata)
(allow file-read*
{read_rules})
(allow file-write*
  (subpath "{worktree}"))
(deny network*)
"#
    )
}

fn linux_sandboxed_test_command(cmd: &[String], worktree: &Path) -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut sandbox_cmd = vec![
        "bwrap".to_owned(),
        "--die-with-parent".to_owned(),
        "--unshare-all".to_owned(),
        "--unshare-net".to_owned(),
        "--new-session".to_owned(),
        "--dev".to_owned(),
        "/dev".to_owned(),
        "--proc".to_owned(),
        "/proc".to_owned(),
        "--tmpfs".to_owned(),
        "/tmp".to_owned(),
    ];
    let home = home.strip_suffix('/').unwrap_or(&home);
    for path in [
        "/bin".to_owned(),
        "/etc".to_owned(),
        "/lib".to_owned(),
        "/lib64".to_owned(),
        "/nix".to_owned(),
        "/opt".to_owned(),
        "/usr".to_owned(),
        home.to_owned() + "/.cargo/bin",
        home.to_owned() + "/.cargo/git",
        home.to_owned() + "/.cargo/registry",
        home.to_owned() + "/.rustup/toolchains",
    ] {
        sandbox_cmd.extend(["--ro-bind-try".to_owned(), path.clone(), path]);
    }
    let worktree = worktree.display().to_string();
    sandbox_cmd.extend([
        "--bind".to_owned(),
        worktree.clone(),
        worktree.clone(),
        "--chdir".to_owned(),
        worktree,
        "--".to_owned(),
    ]);
    sandbox_cmd.extend(cmd.iter().cloned());
    sandbox_cmd
}

fn sandbox_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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

fn review_reasons(verdict: &ReviewVerdict) -> String {
    if verdict.reasons.is_empty() {
        "no reasons provided".to_owned()
    } else {
        verdict.reasons.join("; ")
    }
}

/// Invoke the Claude Code agent headlessly, feeding `prompt` on stdin and
/// returning the agent's `result` field.
pub fn run_agent(prompt: &str, cwd: Option<&Path>, timeout: Duration) -> Result<String> {
    let mut command = Command::new("claude");
    add_claude_base_args(&mut command);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let stdout = capture(command, Some(prompt), timeout, "claude")?;
    parse_agent_output(&stdout)
}

fn run_review_agent(prompt: &str, options: &PipelineOptions) -> Result<String> {
    let mut command = Command::new("claude");
    add_claude_base_args(&mut command);
    command.args([
        "--safe-mode",
        "--setting-sources",
        "user",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--tools",
        "",
    ]);
    command.current_dir(&options.repo_root);
    let stdout = capture(command, Some(prompt), options.agent_timeout, "claude")?;
    parse_agent_output(&stdout)
}

fn add_claude_base_args(command: &mut Command) {
    command.args([
        "-p",
        "--output-format",
        "json",
        "--no-session-persistence",
        "--permission-mode",
        "default",
        "--settings",
        CLAUDE_AGENT_SETTINGS,
    ]);
}

/// Parse the JSON envelope emitted by `claude -p --output-format json` and
/// extract its `result` field.
pub fn parse_agent_output(stdout: &str) -> Result<String> {
    let data: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|_| anyhow!("claude: invalid JSON: {}", truncate(stdout, 200)))?;
    if let Some(result) = data.get("result") {
        return match result {
            serde_json::Value::String(s) => Ok(s.clone()),
            other => bail!(
                "claude: 'result' must be a string, got {}: {}",
                json_value_kind(other),
                truncate(stdout, 200)
            ),
        };
    }

    if let Some(items) = data.as_array() {
        let results = items
            .iter()
            .filter(|item| item.get("type").and_then(serde_json::Value::as_str) == Some("result"))
            .collect::<Vec<_>>();
        let result = match results.as_slice() {
            [] => bail!("claude: missing result event: {}", truncate(stdout, 200)),
            [result] => *result,
            _ => bail!(
                "claude: expected exactly one result event, got {}: {}",
                results.len(),
                truncate(stdout, 200)
            ),
        };
        return match result.get("result") {
            Some(serde_json::Value::String(s)) => Ok(s.clone()),
            Some(other) => bail!(
                "claude: result event 'result' must be a string, got {}: {}",
                json_value_kind(other),
                truncate(stdout, 200)
            ),
            None => bail!(
                "claude: result event missing 'result' key: {}",
                truncate(stdout, 200)
            ),
        };
    }

    bail!("claude: missing 'result' key: {}", truncate(stdout, 200))
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
