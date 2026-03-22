from __future__ import annotations

import json
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from pipeline import (
    PipelineContext,
    Stage,
    StageResult,
    _parse_verdict,
    _run_stage,
    _summarize,
    emit_log,
    load_prompt,
    run_cmd,
    run_pipeline,
    timed_stage,
)

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def ctx() -> PipelineContext:
    return PipelineContext(issue_number=42, repo="owner/repo")


@pytest.fixture
def tmp_prompts(tmp_path: Path) -> Path:
    tpl = tmp_path / "test.md"
    tpl.write_text("Hello {name}, issue #{num}")
    return tmp_path


@pytest.fixture
def tmp_logs(tmp_path: Path) -> Path:
    return tmp_path / "logs"


# ---------------------------------------------------------------------------
# Unit tests: data structures
# ---------------------------------------------------------------------------


class TestStage:
    def test_all_stages_have_string_values(self) -> None:
        for s in Stage:
            assert isinstance(s.value, str)

    def test_stage_count(self) -> None:
        assert len(Stage) == 11


class TestStageResult:
    def test_frozen(self) -> None:
        r = StageResult(
            stage=Stage.ISSUE_DETECTION,
            status="success",
            input_summary="in",
            output_summary="out",
            error=None,
            duration_seconds=1.0,
            timestamp="2026-01-01T00:00:00+00:00",
        )
        with pytest.raises(AttributeError):
            r.status = "failure"  # type: ignore[misc]


class TestPipelineContext:
    def test_defaults(self, ctx: PipelineContext) -> None:
        assert ctx.issue_title == ""
        assert ctx.results == []


# ---------------------------------------------------------------------------
# Unit tests: utilities
# ---------------------------------------------------------------------------


class TestRunCmd:
    def test_echo(self) -> None:
        assert run_cmd(["echo", "hello"]) == "hello"

    def test_failure_raises(self) -> None:
        with pytest.raises(RuntimeError):
            run_cmd(["false"])


class TestLoadPrompt:
    def test_substitution(self, tmp_prompts: Path) -> None:
        result = load_prompt(
            "test.md",
            _prompts_dir=tmp_prompts,
            name="Alice",
            num="7",
        )
        assert result == "Hello Alice, issue #7"

    def test_missing_var_left_as_placeholder(self, tmp_prompts: Path) -> None:
        result = load_prompt("test.md", _prompts_dir=tmp_prompts, name="Alice")
        assert result == "Hello Alice, issue #{num}"


class TestEmitLog:
    def test_writes_jsonl(self, tmp_logs: Path) -> None:
        results = [
            StageResult(
                stage=Stage.ISSUE_DETECTION,
                status="success",
                input_summary="in",
                output_summary="out",
                error=None,
                duration_seconds=0.5,
                timestamp="2026-01-01T00:00:00+00:00",
            ),
        ]
        emit_log(results, 42, _logs_dir=tmp_logs)
        log_file = tmp_logs / "issue-42.jsonl"
        assert log_file.exists()
        line = json.loads(log_file.read_text().strip())
        assert line["stage"] == "issue_detection"
        assert line["status"] == "success"

    def test_appends(self, tmp_logs: Path) -> None:
        r = StageResult(
            stage=Stage.PLAN_CREATION,
            status="success",
            input_summary="in",
            output_summary="out",
            error=None,
            duration_seconds=1.0,
            timestamp="2026-01-01T00:00:00+00:00",
        )
        emit_log([r], 42, _logs_dir=tmp_logs)
        emit_log([r], 42, _logs_dir=tmp_logs)
        lines = (tmp_logs / "issue-42.jsonl").read_text().strip().split("\n")
        assert len(lines) == 2


class TestTimedStage:
    def test_success(self, ctx: PipelineContext) -> None:
        def noop(c: PipelineContext) -> None:
            pass

        result = timed_stage(Stage.SMOKE_TEST, noop, ctx, "test")
        assert result.status == "success"
        assert result.duration_seconds >= 0

    def test_failure(self, ctx: PipelineContext) -> None:
        def fail(c: PipelineContext) -> None:
            raise ValueError("boom")

        result = timed_stage(Stage.SMOKE_TEST, fail, ctx, "test")
        assert result.status == "failure"
        assert result.error == "boom"
        assert ctx.last_error == "boom"

    def test_permanent_error_reraises(self, ctx: PipelineContext) -> None:
        def fail_permanent(c: PipelineContext) -> None:
            raise FileNotFoundError("missing template")

        with pytest.raises(FileNotFoundError, match="missing template"):
            timed_stage(Stage.SMOKE_TEST, fail_permanent, ctx, "test")


class TestSummarize:
    def test_issue_detection(self, ctx: PipelineContext) -> None:
        ctx.issue_title = "Fix bug"
        assert _summarize(ctx, Stage.ISSUE_DETECTION) == "#42: Fix bug"

    def test_branch_creation(self, ctx: PipelineContext) -> None:
        ctx.branch_name = "issue-42"
        assert _summarize(ctx, Stage.BRANCH_CREATION) == "issue-42"

    def test_default(self, ctx: PipelineContext) -> None:
        assert _summarize(ctx, Stage.SMOKE_TEST) == "done"


class TestParseVerdict:
    def test_approved(self) -> None:
        assert _parse_verdict("Looks good.\nVERDICT:APPROVED") is True

    def test_rejected(self) -> None:
        assert _parse_verdict("Issues found.\nVERDICT:REJECTED") is False

    def test_no_verdict_raises(self) -> None:
        with pytest.raises(RuntimeError, match="No VERDICT line"):
            _parse_verdict("Some output without verdict")

    def test_disapprove_not_matched_as_approved(self) -> None:
        assert _parse_verdict("VERDICT:DISAPPROVED") is False


# ---------------------------------------------------------------------------
# Integration tests: stage functions (subprocess mocked)
# ---------------------------------------------------------------------------


class TestFetchIssue:
    @patch("pipeline.run_cmd")
    def test_populates_context(self, mock_run: Any, ctx: PipelineContext) -> None:
        from pipeline import fetch_issue

        mock_run.return_value = json.dumps(
            {"title": "Fix login", "body": "Login is broken"}
        )
        fetch_issue(ctx)
        assert ctx.issue_title == "Fix login"
        assert ctx.issue_body == "Login is broken"

    @patch("pipeline.run_cmd")
    def test_null_body(self, mock_run: Any, ctx: PipelineContext) -> None:
        from pipeline import fetch_issue

        mock_run.return_value = json.dumps({"title": "No body", "body": None})
        fetch_issue(ctx)
        assert ctx.issue_body == ""


class TestCreatePlan:
    @patch("pipeline.run_agent")
    @patch("pipeline.run_cmd")
    def test_sets_plan(
        self,
        mock_cmd: Any,
        mock_agent: Any,
        ctx: PipelineContext,
    ) -> None:
        from pipeline import create_plan

        ctx.issue_title = "Add feature"
        ctx.issue_body = "Details"
        mock_cmd.return_value = "main.py\nREADME.md"
        mock_agent.return_value = "Step 1: do thing"
        create_plan(ctx)
        assert ctx.plan == "Step 1: do thing"
        mock_agent.assert_called_once()


class TestReviewPlan:
    @patch("pipeline.run_agent")
    def test_approve(self, mock_agent: Any, ctx: PipelineContext) -> None:
        from pipeline import review_plan

        ctx.plan = "my plan"
        mock_agent.return_value = "Looks good.\nVERDICT:APPROVED"
        review_plan(ctx)  # should not raise

    @patch("pipeline.run_agent")
    def test_reject(self, mock_agent: Any, ctx: PipelineContext) -> None:
        from pipeline import review_plan

        ctx.plan = "bad plan"
        mock_agent.return_value = "Missing tests.\nVERDICT:REJECTED"
        with pytest.raises(RuntimeError, match="Plan rejected"):
            review_plan(ctx)


class TestCreateBranch:
    @patch("pipeline.run_cmd")
    def test_sets_branch(self, mock_cmd: Any, ctx: PipelineContext) -> None:
        from pipeline import create_branch

        mock_cmd.return_value = ""
        create_branch(ctx)
        assert ctx.branch_name == "issue-42"
        assert "issue-42" in ctx.worktree_path


class TestImplement:
    @patch("pipeline.run_agent")
    def test_calls_agent(self, mock_agent: Any, ctx: PipelineContext) -> None:
        from pipeline import implement

        ctx.plan = "the plan"
        ctx.worktree_path = "/tmp/wt"
        mock_agent.return_value = "done"
        implement(ctx)
        mock_agent.assert_called_once()
        call_kwargs = mock_agent.call_args
        assert call_kwargs[1]["cwd"] == "/tmp/wt"


class TestRunTests:
    @patch("pipeline.run_cmd")
    def test_runs_three_commands(self, mock_cmd: Any, ctx: PipelineContext) -> None:
        from pipeline import run_tests

        ctx.worktree_path = "/tmp/wt"
        mock_cmd.return_value = ""
        run_tests(ctx)
        assert mock_cmd.call_count == 3


class TestCommitChanges:
    @patch("pipeline.run_cmd")
    def test_runs_git_commands(self, mock_cmd: Any, ctx: PipelineContext) -> None:
        from pipeline import commit_changes

        ctx.issue_number = 42
        ctx.worktree_path = "/tmp/wt"
        mock_cmd.side_effect = [
            "",  # git add -A
            "M pipeline.py",  # git status --porcelain
            "",  # git commit
        ]
        commit_changes(ctx)
        assert mock_cmd.call_count == 3

    @patch("pipeline.run_cmd")
    def test_no_changes_raises(self, mock_cmd: Any, ctx: PipelineContext) -> None:
        from pipeline import commit_changes

        ctx.worktree_path = "/tmp/wt"
        mock_cmd.side_effect = [
            "",  # git add -A
            "",  # git status --porcelain (empty = no changes)
        ]
        with pytest.raises(RuntimeError, match="No changes to commit"):
            commit_changes(ctx)


class TestSmokeTest:
    def test_noop(self, ctx: PipelineContext) -> None:
        from pipeline import smoke_test

        smoke_test(ctx)  # should not raise


class TestCreatePr:
    @patch("pipeline.run_cmd")
    def test_sets_pr_url(self, mock_cmd: Any, ctx: PipelineContext) -> None:
        from pipeline import create_pr

        ctx.branch_name = "issue-42"
        ctx.issue_title = "Fix bug"
        ctx.plan = "the plan"
        mock_cmd.side_effect = [
            "",  # git push
            "https://github.com/owner/repo/pull/1",  # gh pr create
        ]
        create_pr(ctx)
        assert ctx.pr_url == "https://github.com/owner/repo/pull/1"


class TestCodeReview:
    @patch("pipeline.run_agent")
    @patch("pipeline.run_cmd")
    def test_approve(
        self,
        mock_cmd: Any,
        mock_agent: Any,
        ctx: PipelineContext,
    ) -> None:
        from pipeline import code_review

        ctx.worktree_path = "/tmp/wt"
        ctx.issue_title = "Fix"
        mock_cmd.return_value = "diff content"
        mock_agent.return_value = "LGTM.\nVERDICT:APPROVED"
        code_review(ctx)

    @patch("pipeline.run_agent")
    @patch("pipeline.run_cmd")
    def test_reject(
        self,
        mock_cmd: Any,
        mock_agent: Any,
        ctx: PipelineContext,
    ) -> None:
        from pipeline import code_review

        ctx.worktree_path = "/tmp/wt"
        ctx.issue_title = "Fix"
        mock_cmd.return_value = "diff content"
        mock_agent.return_value = "Bug found.\nVERDICT:REJECTED"
        with pytest.raises(RuntimeError, match="Code review rejected"):
            code_review(ctx)


# ---------------------------------------------------------------------------
# Integration tests: pipeline orchestration
# ---------------------------------------------------------------------------


class TestRunPipeline:
    @patch("pipeline.emit_log")
    @patch("pipeline.wait_for_merge")
    @patch("pipeline.code_review")
    @patch("pipeline.create_pr")
    @patch("pipeline.smoke_test")
    @patch("pipeline.commit_changes")
    @patch("pipeline.run_tests")
    @patch("pipeline.implement")
    @patch("pipeline.create_branch")
    @patch("pipeline.review_plan")
    @patch("pipeline.create_plan")
    @patch("pipeline.fetch_issue")
    def test_happy_path(
        self,
        mock_fetch: Any,
        mock_plan: Any,
        mock_review: Any,
        mock_branch: Any,
        mock_impl: Any,
        mock_test: Any,
        mock_commit: Any,
        mock_smoke: Any,
        mock_pr: Any,
        mock_code_review: Any,
        mock_merge: Any,
        mock_emit: Any,
    ) -> None:
        def set_title(c: PipelineContext) -> None:
            c.issue_title = "Test"

        def set_plan(c: PipelineContext) -> None:
            c.plan = "plan"

        def set_branch(c: PipelineContext) -> None:
            c.branch_name = "issue-1"
            c.worktree_path = "/tmp/wt"

        def set_pr(c: PipelineContext) -> None:
            c.pr_url = "https://example.com/pr/1"

        mock_fetch.side_effect = set_title
        mock_plan.side_effect = set_plan
        mock_branch.side_effect = set_branch
        mock_pr.side_effect = set_pr

        results = run_pipeline(1, "owner/repo")
        assert len(results) > 0
        assert all(r.status == "success" for r in results)

    @patch("pipeline.emit_log")
    @patch("pipeline.fetch_issue")
    def test_fetch_failure_exits(
        self,
        mock_fetch: Any,
        mock_emit: Any,
    ) -> None:
        mock_fetch.side_effect = RuntimeError("gh not found")
        with pytest.raises(SystemExit):
            run_pipeline(1, "owner/repo")

    @patch("pipeline.emit_log")
    @patch("pipeline.create_plan")
    @patch("pipeline.fetch_issue")
    def test_plan_retry_escalation(
        self,
        mock_fetch: Any,
        mock_plan: Any,
        mock_emit: Any,
    ) -> None:
        def set_title(c: PipelineContext) -> None:
            c.issue_title = "Test"

        mock_fetch.side_effect = set_title
        mock_plan.side_effect = RuntimeError("agent failed")
        with pytest.raises(SystemExit, match="failed"):
            run_pipeline(1, "owner/repo")


# ---------------------------------------------------------------------------
# Unit tests: _run_stage retry logic (T1)
# ---------------------------------------------------------------------------


class TestRunStage:
    @patch("pipeline.emit_log")
    def test_success_first_attempt(self, mock_emit: Any, ctx: PipelineContext) -> None:
        def noop(c: PipelineContext) -> None:
            pass

        result = _run_stage(Stage.SMOKE_TEST, noop, ctx, "test")
        assert result.status == "success"
        assert len(ctx.results) == 1

    @patch("pipeline.emit_log")
    def test_retry_then_success(self, mock_emit: Any, ctx: PipelineContext) -> None:
        call_count = 0

        def fail_once(c: PipelineContext) -> None:
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                raise RuntimeError("transient")

        result = _run_stage(Stage.SMOKE_TEST, fail_once, ctx, "test", max_attempts=2)
        assert result.status == "success"
        assert len(ctx.results) == 2

    @patch("pipeline.emit_log")
    def test_max_attempts_exits(self, mock_emit: Any, ctx: PipelineContext) -> None:
        def always_fail(c: PipelineContext) -> None:
            raise RuntimeError("persistent")

        with pytest.raises(SystemExit, match="failed"):
            _run_stage(Stage.SMOKE_TEST, always_fail, ctx, "test", max_attempts=2)
        assert len(ctx.results) == 2


# ---------------------------------------------------------------------------
# Unit tests: run_agent error handling (T4)
# ---------------------------------------------------------------------------


class TestRunAgentParsing:
    @patch("subprocess.run")
    def test_invalid_json_raises(self, mock_run: Any) -> None:
        from pipeline import run_agent

        mock_run.return_value = type("R", (), {"stdout": "not json", "returncode": 0})()
        with pytest.raises(RuntimeError, match="invalid JSON"):
            run_agent("prompt", allowed_tools="Read")

    @patch("subprocess.run")
    def test_missing_result_key_raises(self, mock_run: Any) -> None:
        from pipeline import run_agent

        mock_run.return_value = type(
            "R", (), {"stdout": '{"output": "no result key"}', "returncode": 0}
        )()
        with pytest.raises(RuntimeError, match="missing 'result' key"):
            run_agent("prompt", allowed_tools="Read")

    @patch("subprocess.run")
    def test_timeout_raises(self, mock_run: Any) -> None:
        import subprocess as sp

        from pipeline import run_agent

        mock_run.side_effect = sp.TimeoutExpired(cmd=["claude"], timeout=300)
        with pytest.raises(RuntimeError, match="timed out"):
            run_agent("prompt", allowed_tools="Read")


# ---------------------------------------------------------------------------
# Unit tests: load_prompt with braces in content (C1 verification)
# ---------------------------------------------------------------------------


class TestLoadPromptBraces:
    def test_diff_with_braces_does_not_crash(self, tmp_path: Path) -> None:
        tpl = tmp_path / "review.md"
        tpl.write_text("Review: {diff}\n")
        result = load_prompt(
            "review.md",
            _prompts_dir=tmp_path,
            diff="function foo() { return {bar: 1}; }",
        )
        assert "function foo()" in result
        assert "{bar: 1}" in result
