"""Pipeline manager.

Drives a GitHub Issue through planning, implementation, testing,
and review stages.
"""

import json
import re
import subprocess
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
