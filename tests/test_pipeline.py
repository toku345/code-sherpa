"""Tests for pipeline core utilities."""

import subprocess
from pathlib import Path
from unittest.mock import patch

import pytest

from pipeline import PipelineContext, Stage, load_prompt, run_agent, run_cmd


class TestStage:
    def test_values(self) -> None:
        expected = [
            "issue_fetch",
            "plan_creation",
            "plan_review",
            "branch_creation",
            "implementation",
            "test_execution",
            "pr_creation",
            "code_review",
        ]
        assert [s.value for s in Stage] == expected
        for s in Stage:
            assert isinstance(s, str)


class TestPipelineContext:
    def test_defaults(self) -> None:
        ctx = PipelineContext(issue_number=1, repo="owner/repo")
        assert ctx.issue_number == 1
        assert ctx.repo == "owner/repo"
        assert ctx.issue_title == ""
        assert ctx.issue_body == ""
        assert ctx.plan == ""
        assert ctx.worktree_path == ""
        assert ctx.branch_name == ""
        assert ctx.last_error == ""


class TestRunCmd:
    def test_success(self) -> None:
        result = run_cmd(["echo", "hello"])
        assert result == "hello\n"

    def test_timeout(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.side_effect = subprocess.TimeoutExpired(
                cmd=["git", "clone"], timeout=10
            )
            with pytest.raises(RuntimeError, match=r"git: timed out after 10s"):
                run_cmd(["git", "clone"], timeout=10)

    def test_failure(self) -> None:
        with pytest.raises(RuntimeError, match=r"^ls: ") as exc_info:
            run_cmd(["ls", "nonexistent_path_that_does_not_exist"])
        assert "ls:" in str(exc_info.value)


class TestRunAgent:
    def test_success(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=["claude", "-p", "--output-format", "json"],
                returncode=0,
                stdout='{"result": "done"}',
                stderr="",
            )
            result = run_agent("do something")
            assert result == "done"
            mock_run.assert_called_once()
            call_kwargs = mock_run.call_args
            assert call_kwargs.kwargs["input"] == "do something"

    def test_failure(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.side_effect = subprocess.CalledProcessError(
                returncode=1,
                cmd=["claude", "-p", "--output-format", "json"],
                stderr="agent error",
            )
            with pytest.raises(RuntimeError, match=r"claude: agent error"):
                run_agent("do something")

    def test_timeout(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.side_effect = subprocess.TimeoutExpired(
                cmd=["claude", "-p", "--output-format", "json"], timeout=300
            )
            with pytest.raises(RuntimeError, match=r"claude: timed out after 300s"):
                run_agent("do something")

    def test_invalid_json(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=["claude", "-p", "--output-format", "json"],
                returncode=0,
                stdout="not json",
                stderr="",
            )
            with pytest.raises(RuntimeError, match=r"claude: invalid JSON"):
                run_agent("do something")

    def test_missing_result_key(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=["claude", "-p", "--output-format", "json"],
                returncode=0,
                stdout='{"other": "value"}',
                stderr="",
            )
            with pytest.raises(RuntimeError, match=r"claude: missing 'result' key"):
                run_agent("do something")


class TestLoadPrompt:
    def test_substitution(self, tmp_path: Path) -> None:
        tpl = tmp_path / "test.md"
        tpl.write_text("Hello {{name}}, issue #{{num}}", encoding="utf-8")
        result = load_prompt("test.md", _prompts_dir=tmp_path, name="Alice", num="7")
        assert result == "Hello Alice, issue #7"

    def test_missing_variable(self, tmp_path: Path) -> None:
        tpl = tmp_path / "test.md"
        tpl.write_text("Hello {{name}}, issue #{{num}}", encoding="utf-8")
        result = load_prompt("test.md", _prompts_dir=tmp_path, name="Alice")
        assert result == "Hello Alice, issue #{{num}}"

    def test_no_reexpansion(self, tmp_path: Path) -> None:
        tpl = tmp_path / "test.md"
        tpl.write_text("{{first}} and {{second}}", encoding="utf-8")
        result = load_prompt(
            "test.md", _prompts_dir=tmp_path, first="{{second}}", second="BOOM"
        )
        assert result == "{{second}} and BOOM"
