//! code-sherpa CLI entry point.
//!
//! Parses the target issue and prints the planned pipeline. Stage
//! orchestration (the deterministic state machine) is layered on top of the
//! primitives in `code_sherpa` and is not implemented yet.

use anyhow::{Context, bail};
use clap::Parser;
use code_sherpa::{PipelineContext, Stage};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Guide a GitHub Issue from detection to merge.
#[derive(Parser)]
#[command(name = "sherpa", version, about)]
struct Cli {
    /// Issue number to drive through the pipeline.
    issue_number: u64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let ctx = resolve_context(cli.issue_number)?;

    eprintln!(
        "sherpa: issue #{} in {} at {} (pipeline not yet implemented)",
        ctx.issue_number, ctx.repo, ctx.worktree_path
    );
    eprintln!("stages: {:?}", Stage::ALL.map(|s| s.as_str()));
    anyhow::bail!("pipeline orchestration is not implemented yet")
}

fn resolve_context(issue_number: u64) -> anyhow::Result<PipelineContext> {
    resolve_context_from(issue_number, std::env::current_dir()?.as_path())
}

fn resolve_context_from(issue_number: u64, cwd: &Path) -> anyhow::Result<PipelineContext> {
    let repo_root = resolve_repo_root(cwd)?;
    let remote_url = read_origin_remote(&repo_root)?;
    let repo = parse_repo_slug(&remote_url).with_context(|| {
        format!(
            "git origin remote is not a supported owner/repo URL: {}",
            remote_url.trim()
        )
    })?;

    Ok(PipelineContext::new(
        issue_number,
        repo,
        repo_root.display().to_string(),
    ))
}

fn resolve_repo_root(cwd: &Path) -> anyhow::Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("failed to resolve git repository root")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "current directory must be inside a git repository: {}",
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout).context("git repository root was not UTF-8")?;
    let repo_root = stdout.trim();
    if repo_root.is_empty() {
        bail!("git rev-parse returned an empty repository root");
    }
    Ok(PathBuf::from(repo_root))
}

fn read_origin_remote(repo_root: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(repo_root)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("failed to read git origin remote")?;

    if output.status.success() {
        return String::from_utf8(output.stdout).context("git origin remote was not valid UTF-8");
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "git repository must have an origin remote: {}",
        stderr.trim()
    )
}

fn parse_repo_slug(remote_url: &str) -> Option<String> {
    let remote_url = remote_url.trim();
    if remote_url.is_empty() {
        return None;
    }

    let path = if is_scp_like_remote(remote_url) {
        remote_url.split_once(':')?.1
    } else if let Some(scheme_end) = remote_url.find("://") {
        let without_scheme = &remote_url[scheme_end + 3..];
        let path_start = without_scheme.find('/')?;
        &without_scheme[path_start + 1..]
    } else {
        remote_url
    };

    if path.contains(['?', '#', '\\']) {
        return None;
    }

    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 2 {
        return None;
    }
    let [owner, repo] = parts.as_slice() else {
        return None;
    };
    if !is_valid_repo_segment(owner) || !is_valid_repo_segment(repo) {
        return None;
    }

    Some(format!("{owner}/{repo}"))
}

fn is_scp_like_remote(remote_url: &str) -> bool {
    let Some((authority, _path)) = remote_url.split_once(':') else {
        return false;
    };
    authority.contains('@') && !authority.contains('/')
}

fn is_valid_repo_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(':')
}

#[cfg(test)]
mod tests {
    use super::{parse_repo_slug, resolve_context_from};
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn parses_common_origin_remote_forms() {
        assert_eq!(
            parse_repo_slug("git@github.com:owner/repo.git"),
            Some("owner/repo".to_owned())
        );
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo.git"),
            Some("owner/repo".to_owned())
        );
        assert_eq!(
            parse_repo_slug("ssh://git@github.com/owner/repo.git"),
            Some("owner/repo".to_owned())
        );
        assert_eq!(
            parse_repo_slug("git@github.example.com:owner/repo"),
            Some("owner/repo".to_owned())
        );
    }

    #[test]
    fn rejects_unsupported_remote_forms() {
        assert_eq!(parse_repo_slug(""), None);
        assert_eq!(parse_repo_slug("https://github.com/owner"), None);
        assert_eq!(parse_repo_slug("/tmp/owner/repo"), None);
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo/issues"),
            None
        );
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo.git?x=1"),
            None
        );
        assert_eq!(
            parse_repo_slug("https://github.com/org/team/repo.git"),
            None
        );
    }

    #[test]
    fn resolves_context_from_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "git@github.com:owner/repo.git");

        let ctx = resolve_context_from(123, dir.path()).unwrap();

        assert_eq!(ctx.issue_number, 123);
        assert_eq!(ctx.repo, "owner/repo");
        assert_eq!(
            std::fs::canonicalize(&ctx.worktree_path).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn resolves_context_from_repo_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/owner/repo.git");
        let subdir = dir.path().join("nested");
        std::fs::create_dir(&subdir).unwrap();

        let ctx = resolve_context_from(123, &subdir).unwrap();

        assert_eq!(ctx.repo, "owner/repo");
        assert_eq!(
            std::fs::canonicalize(&ctx.worktree_path).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn resolve_context_rejects_non_git_directory() {
        let dir = tempfile::tempdir().unwrap();

        let err = resolve_context_from(123, dir.path()).unwrap_err();

        assert!(
            err.to_string()
                .contains("current directory must be inside a git repository"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_context_rejects_missing_origin() {
        let dir = tempfile::tempdir().unwrap();
        run_git(&["init"], dir.path());

        let err = resolve_context_from(123, dir.path()).unwrap_err();

        assert!(
            err.to_string()
                .contains("git repository must have an origin remote"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_context_rejects_deep_origin_path() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/org/team/repo.git");

        let err = resolve_context_from(123, dir.path()).unwrap_err();

        assert!(
            err.to_string()
                .contains("git origin remote is not a supported owner/repo URL"),
            "{err:#}"
        );
    }

    fn init_repo(cwd: &Path, remote: &str) {
        run_git(&["init"], cwd);
        run_git(&["remote", "add", "origin", remote], cwd);
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
}
