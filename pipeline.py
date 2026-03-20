"""Pipeline manager: GitHub Issue → plan → review → implement → test → PR."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import Enum
from pathlib import Path

MAX_RETRIES = 3
READONLY_TOOLS = "Read,Glob,Grep"
WRITE_TOOLS = "Read,Write,Edit,Glob,Grep"


class Stage(Enum):
    ISSUE_DETECTION = "issue_detection"
    PLAN_CREATION = "plan_creation"
    PLAN_REVIEW = "plan_review"
    BRANCH_CREATION = "branch_creation"
    IMPLEMENTATION = "implementation"
    TEST_EXECUTION = "test_execution"
    COMMIT_CHANGES = "commit_changes"
    SMOKE_TEST = "smoke_test"
    PR_CREATION = "pr_creation"
    CODE_REVIEW = "code_review"
    MERGE_DECISION = "merge_decision"


@dataclass(frozen=True)
class StageResult:
    stage: Stage
    status: str  # "success" | "failure"
    input_summary: str
    output_summary: str
    error: str | None
    duration_seconds: float
    timestamp: str  # ISO 8601


@dataclass
class PipelineContext:
    issue_number: int
    repo: str  # "owner/repo"
    issue_title: str = ""
    issue_body: str = ""
    plan: str = ""
    branch_name: str = ""
    worktree_path: str = ""
    pr_url: str = ""
    last_error: str = ""
    results: list[StageResult] = field(default_factory=list)


def run_cmd(
    cmd: list[str], cwd: str | None = None, timeout: int = 120,
) -> str:
    return subprocess.run(
        cmd, capture_output=True, text=True, check=True,
        timeout=timeout, cwd=cwd).stdout.strip()


def run_agent(
    prompt: str, allowed_tools: str,
    timeout: int = 300, cwd: str | None = None,
) -> str:
    cmd = [
        "claude", "-p", "--output-format", "json", "--max-turns", "50",
        "--dangerously-skip-permissions", "--allowedTools", allowed_tools,
    ]
    r = subprocess.run(cmd, input=prompt, capture_output=True, text=True,
                       check=True, timeout=timeout, cwd=cwd)
    return str(json.loads(r.stdout)["result"])


def load_prompt(
    template: str, _prompts_dir: Path | None = None, **variables: str,
) -> str:
    prompts_dir = _prompts_dir or Path("docs/prompts")
    return (prompts_dir / template).read_text().format_map(variables)


def emit_log(
    results: list[StageResult], issue_number: int,
    _logs_dir: Path | None = None,
) -> None:
    logs_dir = _logs_dir or Path("logs")
    logs_dir.mkdir(parents=True, exist_ok=True)
    with (logs_dir / f"issue-{issue_number}.jsonl").open("a") as f:
        for r in results:
            rec = {"stage": r.stage.value, "status": r.status,
                   "input_summary": r.input_summary,
                   "output_summary": r.output_summary, "error": r.error,
                   "duration_seconds": r.duration_seconds,
                   "timestamp": r.timestamp}
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")


def _summarize(ctx: PipelineContext, stage: Stage) -> str:
    table: dict[Stage, str] = {
        Stage.ISSUE_DETECTION: f"#{ctx.issue_number}: {ctx.issue_title}",
        Stage.PLAN_CREATION: ctx.plan[:200],
        Stage.PLAN_REVIEW: "approved",
        Stage.BRANCH_CREATION: ctx.branch_name,
        Stage.PR_CREATION: ctx.pr_url,
    }
    return table.get(stage, "done")


def timed_stage(
    stage: Stage, func: object, ctx: PipelineContext, input_summary: str,
) -> StageResult:
    start = time.monotonic()
    ts = datetime.now(UTC).isoformat()
    status, output, err = "success", "", None
    try:
        func(ctx)  # type: ignore[operator]
        output = _summarize(ctx, stage)
    except Exception as exc:
        status, err = "failure", str(exc)
        ctx.last_error = err
    return StageResult(
        stage=stage, status=status, input_summary=input_summary,
        output_summary=output, error=err,
        duration_seconds=round(time.monotonic() - start, 2), timestamp=ts,
    )


def fetch_issue(ctx: PipelineContext) -> None:
    data = json.loads(run_cmd([
        "gh", "issue", "view", str(ctx.issue_number),
        "--repo", ctx.repo, "--json", "title,body"]))
    ctx.issue_title = data["title"]
    ctx.issue_body = data["body"] or ""


def create_plan(ctx: PipelineContext) -> None:
    ctx.plan = run_agent(load_prompt(
        "plan.md", issue_number=str(ctx.issue_number),
        issue_title=ctx.issue_title, issue_body=ctx.issue_body,
        repo_structure=run_cmd(["git", "ls-files"]),
    ), allowed_tools=READONLY_TOOLS, timeout=600)


def review_plan(ctx: PipelineContext) -> None:
    output = run_agent(load_prompt(
        "plan-review.md", plan=ctx.plan,
        issue_number=str(ctx.issue_number), issue_title=ctx.issue_title,
    ), allowed_tools=READONLY_TOOLS)
    if "APPROVE" not in output:
        raise RuntimeError(f"Plan rejected: {output[:500]}")


def create_branch(ctx: PipelineContext) -> None:
    branch = f"issue-{ctx.issue_number}"
    wt = str(Path(f".worktrees/{branch}").resolve())
    run_cmd(["git", "worktree", "add", "-b", branch, wt, "main"])
    ctx.branch_name = branch
    ctx.worktree_path = wt


def implement(ctx: PipelineContext) -> None:
    run_agent(
        load_prompt("implement.md", plan=ctx.plan, last_error=ctx.last_error),
        allowed_tools=WRITE_TOOLS, timeout=600, cwd=ctx.worktree_path,
    )


def run_tests(ctx: PipelineContext) -> None:
    wt = ctx.worktree_path
    run_cmd(["uv", "run", "pytest"], cwd=wt, timeout=120)
    run_cmd(["uv", "run", "ruff", "check", "."], cwd=wt, timeout=60)
    run_cmd(["uv", "run", "mypy", "--strict", "."], cwd=wt, timeout=120)


def commit_changes(ctx: PipelineContext) -> None:
    wt = ctx.worktree_path
    run_cmd(["git", "add", "-A"], cwd=wt)
    msg = (f"feat: implement issue #{ctx.issue_number}\n\n"
           f"Resolves #{ctx.issue_number}\n\n"
           "Co-Authored-By: Claude Code <noreply@anthropic.com>")
    run_cmd(["git", "commit", "-m", msg], cwd=wt)


def smoke_test(ctx: PipelineContext) -> None:
    pass  # Phase 0: stub


def create_pr(ctx: PipelineContext) -> None:
    run_cmd(["git", "push", "-u", "origin", ctx.branch_name],
            cwd=ctx.worktree_path)
    ctx.pr_url = run_cmd([
        "gh", "pr", "create", "--repo", ctx.repo,
        "--head", ctx.branch_name, "--title", f"feat: {ctx.issue_title}",
        "--body", f"Resolves #{ctx.issue_number}\n\n"
        f"## Summary\n\n{ctx.plan[:500]}"])


def code_review(ctx: PipelineContext) -> None:
    diff = run_cmd(["git", "diff", "main...HEAD"], cwd=ctx.worktree_path)
    output = run_agent(load_prompt(
        "code-review.md", diff=diff,
        issue_number=str(ctx.issue_number), issue_title=ctx.issue_title,
    ), allowed_tools=READONLY_TOOLS)
    if "APPROVE" not in output:
        raise RuntimeError(f"Code review rejected: {output[:500]}")


def wait_for_merge(ctx: PipelineContext) -> None:
    print(f"PR ready for review: {ctx.pr_url}")


def _run_stage(
    stage: Stage, func: object, ctx: PipelineContext,
    input_summary: str, max_attempts: int = 1,
) -> StageResult:
    for attempt in range(1, max_attempts + 1):
        result = timed_stage(stage, func, ctx, input_summary)
        ctx.results.append(result)
        emit_log([result], ctx.issue_number)
        if result.status == "success":
            return result
        print(f"[{attempt}/{max_attempts}] {stage.value}: {result.error}")
    raise SystemExit(  # noqa: F821
        f"Stage {stage.value} failed. Escalating to human.")


def run_pipeline(issue_number: int, repo: str) -> list[StageResult]:
    ctx = PipelineContext(issue_number=issue_number, repo=repo)
    n = issue_number
    _run_stage(Stage.ISSUE_DETECTION, fetch_issue, ctx, f"issue #{n}")
    for _ in range(MAX_RETRIES):
        _run_stage(Stage.PLAN_CREATION, create_plan, ctx,
                   f"#{n}: {ctx.issue_title}", MAX_RETRIES)
        r = timed_stage(Stage.PLAN_REVIEW, review_plan, ctx,
                        f"plan for #{n}")
        ctx.results.append(r)
        emit_log([r], n)
        if r.status == "success":
            break
        print(f"Plan rejected, regenerating... ({r.error})")
    else:
        raise SystemExit(
            f"Plan review failed after {MAX_RETRIES} cycles. "
            "Escalating to human.")
    _run_stage(Stage.BRANCH_CREATION, create_branch, ctx, f"branch #{n}")
    _run_stage(Stage.IMPLEMENTATION, implement, ctx,
               f"impl #{n}", MAX_RETRIES)
    _run_stage(Stage.TEST_EXECUTION, run_tests, ctx,
               "pytest+ruff+mypy", MAX_RETRIES)
    _run_stage(Stage.COMMIT_CHANGES, commit_changes, ctx, "git commit")
    _run_stage(Stage.SMOKE_TEST, smoke_test, ctx, "smoke test")
    _run_stage(Stage.PR_CREATION, create_pr, ctx, f"PR for #{n}")
    for _ in range(MAX_RETRIES):
        r = timed_stage(Stage.CODE_REVIEW, code_review, ctx,
                        f"review PR #{n}")
        ctx.results.append(r)
        emit_log([r], n)
        if r.status == "success":
            break
        print(f"Code review rejected, fixing... ({r.error})")
        _run_stage(Stage.IMPLEMENTATION, implement, ctx,
                   f"fix #{n}", MAX_RETRIES)
        _run_stage(Stage.TEST_EXECUTION, run_tests, ctx,
                   "pytest+ruff+mypy", MAX_RETRIES)
        _run_stage(Stage.COMMIT_CHANGES, commit_changes, ctx,
                   "git commit")
    else:
        raise SystemExit(
            f"Code review failed after {MAX_RETRIES} cycles. "
            "Escalating to human.")
    _run_stage(Stage.MERGE_DECISION, wait_for_merge, ctx,
               f"PR: {ctx.pr_url}")
    return ctx.results


def _detect_repo() -> str:
    return os.environ.get("CODE_SHERPA_REPO") or run_cmd([
        "gh", "repo", "view", "--json", "nameWithOwner",
        "-q", ".nameWithOwner",
    ])


def main() -> None:
    if len(sys.argv) < 2:
        print("Usage: pipeline.py <issue-number>", file=sys.stderr)
        raise SystemExit(1)
    issue_number = int(sys.argv[1])
    repo = _detect_repo()
    print(f"Running pipeline for {repo}#{issue_number}")
    run_pipeline(issue_number, repo)


if __name__ == "__main__":
    main()
