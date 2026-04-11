"""Pipeline manager.

Drives a GitHub Issue through planning, implementation, testing,
and review stages.
"""

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path


class Stage(StrEnum):
    ISSUE_FETCH = "issue_fetch"
    PLAN_CREATION = "plan_creation"
    PLAN_REVIEW = "plan_review"
    BRANCH_CREATION = "branch_creation"
    IMPLEMENTATION = "implementation"
    TEST_EXECUTION = "test_execution"
    PR_CREATION = "pr_creation"
    CODE_REVIEW = "code_review"


@dataclass
class PipelineContext:
    issue_number: int
    repo: str
    issue_title: str = ""
    issue_body: str = ""
    plan: str = ""
    worktree_path: str = ""
    branch_name: str = ""
    last_error: str = ""


def run_cmd(
    cmd: list[str],
    *,
    cwd: str | None = None,
    timeout: int = 120,
) -> str:
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            check=True,
            encoding="utf-8",
            timeout=timeout,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"{cmd[0]}: timed out after {timeout}s") from exc
    except subprocess.CalledProcessError as exc:
        detail = exc.stderr or exc.stdout
        raise RuntimeError(f"{cmd[0]}: {detail}") from exc
    return result.stdout


def run_agent(
    prompt: str,
    *,
    cwd: str | None = None,
    timeout: int = 300,
) -> str:
    cmd = ["claude", "-p", "--output-format", "json"]
    try:
        result = subprocess.run(
            cmd,
            input=prompt,
            capture_output=True,
            text=True,
            check=True,
            encoding="utf-8",
            timeout=timeout,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"{cmd[0]}: timed out after {timeout}s") from exc
    except subprocess.CalledProcessError as exc:
        detail = exc.stderr or exc.stdout or str(exc)
        raise RuntimeError(f"{cmd[0]}: {detail}") from exc

    stdout = result.stdout
    try:
        data: object = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"{cmd[0]}: invalid JSON: {stdout[:200]}") from exc

    if not isinstance(data, dict) or "result" not in data:
        raise RuntimeError(f"{cmd[0]}: missing 'result' key: {stdout[:200]}")

    return str(data["result"])


def load_prompt(
    template_name: str,
    *,
    _prompts_dir: Path | None = None,
    **variables: str,
) -> str:
    prompts_dir = _prompts_dir or Path("docs/prompts")
    template = (prompts_dir / template_name).read_text(encoding="utf-8")

    def replacer(match: re.Match[str]) -> str:
        key = match.group(1)
        return variables.get(key, match.group(0))

    return re.sub(r"\{\{(\w+)\}\}", replacer, template)


def _detect_repo() -> str:
    repo = os.environ.get("CODE_SHERPA_REPO")
    if repo:
        return repo
    return run_cmd(
        ["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"]
    ).strip()


def fetch_issue(ctx: PipelineContext) -> None:
    raw = run_cmd(
        [
            "gh",
            "issue",
            "view",
            str(ctx.issue_number),
            "--repo",
            ctx.repo,
            "--json",
            "title,body",
        ]
    )
    data = json.loads(raw)
    ctx.issue_title = data["title"]
    ctx.issue_body = data["body"]


def create_branch(ctx: PipelineContext) -> None:
    worktree_path = f"../code-sherpa-worktrees/issue-{ctx.issue_number}"
    branch_name = f"feat/issue-{ctx.issue_number}"
    run_cmd(["git", "fetch", "origin", "main"])
    run_cmd(["git", "worktree", "add", "-b", branch_name, worktree_path, "origin/main"])
    ctx.worktree_path = worktree_path
    ctx.branch_name = branch_name


def implement(ctx: PipelineContext) -> None:
    plan = f"{ctx.issue_title}\n\n{ctx.issue_body}"
    prompt = load_prompt("implement.md", plan=plan, last_error=ctx.last_error)
    run_agent(prompt, cwd=ctx.worktree_path)


def run_tests(ctx: PipelineContext) -> None:
    cwd = ctx.worktree_path
    run_cmd(["uv", "run", "ruff", "check", "."], cwd=cwd)
    run_cmd(["uv", "run", "ruff", "format", "--check", "."], cwd=cwd)
    run_cmd(["uv", "run", "pytest"], cwd=cwd)


def create_pr(ctx: PipelineContext) -> None:
    cwd = ctx.worktree_path
    run_cmd(["git", "add", "-A"], cwd=cwd)
    run_cmd(
        ["git", "commit", "-m", f"feat: implement issue #{ctx.issue_number}"],
        cwd=cwd,
    )
    run_cmd(["git", "push", "-u", "origin", ctx.branch_name], cwd=cwd)
    run_cmd(
        [
            "gh",
            "pr",
            "create",
            "--repo",
            ctx.repo,
            "--title",
            f"feat: {ctx.issue_title}",
            "--body",
            f"Closes #{ctx.issue_number}\n\nAutomated by code-sherpa pipeline.",
        ],
        cwd=cwd,
    )


def run_pipeline(issue_number: int, repo: str) -> None:
    ctx = PipelineContext(issue_number=issue_number, repo=repo)
    fetch_issue(ctx)
    create_branch(ctx)
    try:
        implement(ctx)
        run_tests(ctx)
        create_pr(ctx)
    finally:
        if ctx.worktree_path:
            run_cmd(["git", "worktree", "remove", "--force", ctx.worktree_path])


def main() -> None:
    if len(sys.argv) != 2:
        print("Usage: pipeline.py <issue-number>", file=sys.stderr)
        raise SystemExit(1)
    try:
        issue_number = int(sys.argv[1])
    except ValueError:
        print(f"Invalid issue number: {sys.argv[1]}", file=sys.stderr)
        raise SystemExit(1) from None
    repo = _detect_repo()
    run_pipeline(issue_number, repo)


if __name__ == "__main__":
    main()
