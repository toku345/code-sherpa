"""Tests for pipeline core utilities."""

import subprocess
from pathlib import Path
from unittest.mock import patch

import pytest

import pipeline
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
                stdout='[{"type":"system"},{"type":"result","result":"done"}]',
                stderr="",
            )
            result = run_agent("do something")
            assert result == "done"
            mock_run.assert_called_once()
            call_kwargs = mock_run.call_args
            assert call_kwargs.kwargs["input"] == "do something"

    def test_picks_last_result_event(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=["claude", "-p", "--output-format", "json"],
                returncode=0,
                stdout=(
                    '[{"type":"system","subtype":"init"},'
                    '{"type":"assistant","message":{"content":"ignored"}},'
                    '{"type":"result","subtype":"success","result":"final answer"}]'
                ),
                stderr="",
            )
            assert run_agent("do something") == "final answer"

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

    def test_non_array_output(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=["claude", "-p", "--output-format", "json"],
                returncode=0,
                stdout='{"result": "done"}',
                stderr="",
            )
            with pytest.raises(RuntimeError, match=r"claude: expected JSON array"):
                run_agent("do something")

    def test_no_result_event(self) -> None:
        with patch("pipeline.subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=["claude", "-p", "--output-format", "json"],
                returncode=0,
                stdout='[{"type":"system"},{"type":"assistant"}]',
                stderr="",
            )
            with pytest.raises(RuntimeError, match=r"claude: no result event"):
                run_agent("do something")


class TestMain:
    def test_no_args_raises_system_exit(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setattr("sys.argv", ["pipeline.py"])
        with pytest.raises(SystemExit) as exc_info:
            pipeline.main()
        assert exc_info.value.code == 1

    def test_non_numeric_arg_raises_system_exit(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr("sys.argv", ["pipeline.py", "abc"])
        with pytest.raises(SystemExit) as exc_info:
            pipeline.main()
        assert exc_info.value.code == 1

    def test_valid_args_invokes_run_pipeline(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr("sys.argv", ["pipeline.py", "42"])
        with (
            patch("pipeline._detect_repo", return_value="owner/repo"),
            patch("pipeline.run_pipeline") as mock_run_pipeline,
        ):
            pipeline.main()
            mock_run_pipeline.assert_called_once_with(42, "owner/repo")


class TestRunPipeline:
    def test_success_runs_all_stages_and_cleans_up(self) -> None:
        def set_worktree(ctx: PipelineContext) -> None:
            ctx.worktree_path = "/tmp/wt"

        with (
            patch("pipeline.fetch_issue") as mock_fetch,
            patch(
                "pipeline.create_branch", side_effect=set_worktree
            ) as mock_create_branch,
            patch("pipeline.implement") as mock_implement,
            patch("pipeline.run_tests") as mock_run_tests,
            patch("pipeline.create_pr") as mock_create_pr,
            patch("pipeline.run_cmd") as mock_run_cmd,
        ):
            pipeline.run_pipeline(issue_number=1, repo="owner/repo")
            mock_fetch.assert_called_once()
            mock_create_branch.assert_called_once()
            mock_implement.assert_called_once()
            mock_run_tests.assert_called_once()
            mock_create_pr.assert_called_once()
            mock_run_cmd.assert_called_once_with(
                ["git", "worktree", "remove", "--force", "/tmp/wt"]
            )

    def test_cleanup_on_failure(self) -> None:
        def set_worktree(ctx: PipelineContext) -> None:
            ctx.worktree_path = "/tmp/wt"

        with (
            patch("pipeline.fetch_issue"),
            patch("pipeline.create_branch", side_effect=set_worktree),
            patch("pipeline.implement", side_effect=RuntimeError("boom")),
            patch("pipeline.run_tests") as mock_run_tests,
            patch("pipeline.create_pr") as mock_create_pr,
            patch("pipeline.run_cmd") as mock_run_cmd,
        ):
            with pytest.raises(RuntimeError, match="boom"):
                pipeline.run_pipeline(issue_number=1, repo="owner/repo")
            mock_run_tests.assert_not_called()
            mock_create_pr.assert_not_called()
            mock_run_cmd.assert_called_once_with(
                ["git", "worktree", "remove", "--force", "/tmp/wt"]
            )


class TestCreatePr:
    def test_runs_git_and_gh_in_worktree(self) -> None:
        with patch("pipeline.run_cmd") as mock_run_cmd:
            ctx = PipelineContext(issue_number=42, repo="owner/repo")
            ctx.issue_title = "Fix bug"
            ctx.worktree_path = "/tmp/wt"
            ctx.branch_name = "feat/issue-42"
            pipeline.create_pr(ctx)
            calls = mock_run_cmd.call_args_list
            assert len(calls) == 4
            assert calls[0].args[0] == ["git", "add", "-A"]
            assert calls[1].args[0] == [
                "git",
                "commit",
                "-m",
                "feat: implement issue #42",
            ]
            assert calls[2].args[0] == [
                "git",
                "push",
                "-u",
                "origin",
                "feat/issue-42",
            ]
            assert calls[3].args[0] == [
                "gh",
                "pr",
                "create",
                "--repo",
                "owner/repo",
                "--title",
                "feat: Fix bug",
                "--body",
                "Closes #42\n\nAutomated by code-sherpa pipeline.",
            ]
            for call in calls:
                assert call.kwargs["cwd"] == "/tmp/wt"


class TestRunTestsStage:
    def test_runs_ruff_and_pytest_in_worktree(self) -> None:
        with patch("pipeline.run_cmd") as mock_run_cmd:
            ctx = PipelineContext(issue_number=1, repo="owner/repo")
            ctx.worktree_path = "/tmp/wt"
            pipeline.run_tests(ctx)
            calls = mock_run_cmd.call_args_list
            assert len(calls) == 3
            assert calls[0].args[0] == ["uv", "run", "ruff", "check", "."]
            assert calls[1].args[0] == ["uv", "run", "ruff", "format", "--check", "."]
            assert calls[2].args[0] == ["uv", "run", "pytest"]
            for call in calls:
                assert call.kwargs["cwd"] == "/tmp/wt"


class TestImplement:
    def test_passes_issue_as_plan_and_runs_in_worktree(self) -> None:
        with (
            patch("pipeline.load_prompt") as mock_load_prompt,
            patch("pipeline.run_agent") as mock_run_agent,
        ):
            mock_load_prompt.return_value = "rendered prompt"
            ctx = PipelineContext(issue_number=1, repo="owner/repo")
            ctx.issue_title = "T"
            ctx.issue_body = "B"
            ctx.worktree_path = "/tmp/wt"
            pipeline.implement(ctx)
            mock_load_prompt.assert_called_once_with(
                "implement.md", plan="T\n\nB", last_error=""
            )
            mock_run_agent.assert_called_once_with("rendered prompt", cwd="/tmp/wt")


class TestCreateBranch:
    def test_uses_origin_main_as_base(self) -> None:
        with patch("pipeline.run_cmd") as mock_run_cmd:
            ctx = PipelineContext(issue_number=42, repo="owner/repo")
            pipeline.create_branch(ctx)
            assert mock_run_cmd.call_args_list[0].args[0] == [
                "git",
                "fetch",
                "origin",
                "main",
            ]
            assert mock_run_cmd.call_args_list[1].args[0] == [
                "git",
                "worktree",
                "add",
                "-b",
                "feat/issue-42",
                "../code-sherpa-worktrees/issue-42",
                "origin/main",
            ]
            assert ctx.worktree_path == "../code-sherpa-worktrees/issue-42"
            assert ctx.branch_name == "feat/issue-42"


class TestFetchIssue:
    def test_parses_and_sets_ctx(self) -> None:
        with patch("pipeline.run_cmd") as mock_run_cmd:
            mock_run_cmd.return_value = '{"title": "Fix bug", "body": "Details"}'
            ctx = PipelineContext(issue_number=7, repo="owner/repo")
            pipeline.fetch_issue(ctx)
            assert ctx.issue_title == "Fix bug"
            assert ctx.issue_body == "Details"
            mock_run_cmd.assert_called_once_with(
                [
                    "gh",
                    "issue",
                    "view",
                    "7",
                    "--repo",
                    "owner/repo",
                    "--json",
                    "title,body",
                ]
            )


class TestDetectRepo:
    def test_env_var(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("CODE_SHERPA_REPO", "owner/repo")
        assert pipeline._detect_repo() == "owner/repo"

    def test_gh_command(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("CODE_SHERPA_REPO", raising=False)
        with patch("pipeline.run_cmd") as mock_run_cmd:
            mock_run_cmd.return_value = "owner/repo\n"
            assert pipeline._detect_repo() == "owner/repo"
            mock_run_cmd.assert_called_once_with(
                [
                    "gh",
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner",
                    "-q",
                    ".nameWithOwner",
                ]
            )


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
