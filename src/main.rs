//! code-sherpa CLI entry point.
//!
//! Parses the target issue, resolves the GitHub repository, and starts the
//! deterministic pipeline walking skeleton.

use anyhow::{Context, bail};
use clap::Parser;
use code_sherpa::{PipelineContext, PipelineOptions, run_pipeline};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Guide a GitHub Issue from detection to merge.
#[derive(Parser)]
#[command(name = "sherpa", version, about)]
struct Cli {
    /// Issue number to drive through the pipeline.
    issue_number: u64,

    /// Push the generated branch and create/reuse a GitHub pull request.
    #[arg(long)]
    publish: bool,

    /// Alias for --publish.
    #[arg(long)]
    yes: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let ctx = resolve_context(cli.issue_number)?;
    let repo_root = PathBuf::from(&ctx.worktree_path);
    let mut options = PipelineOptions::new(&repo_root, repo_root.join("docs/prompts"));
    options.publish = cli.publish || cli.yes;

    let outcome = run_pipeline(ctx, &options)?;
    eprintln!(
        "sherpa: issue #{} in {} completed through code_review (dry_run={})",
        outcome.context.issue_number, outcome.context.repo, outcome.dry_run
    );
    if let Some(url) = outcome.pr_url {
        eprintln!("pr: {url}");
    }
    if let Some(review) = outcome.code_review {
        eprintln!("code_review: {:?}", review.decision);
    }
    Ok(())
}

fn resolve_context(issue_number: u64) -> anyhow::Result<PipelineContext> {
    resolve_context_from(issue_number, std::env::current_dir()?.as_path())
}

fn resolve_context_from(issue_number: u64, cwd: &Path) -> anyhow::Result<PipelineContext> {
    let repo_root = resolve_repo_root(cwd)?;
    let remote_url = read_origin_remote(&repo_root)?;
    let repo = parse_repo_slug(&remote_url)
        .context("git origin remote must be a github.com owner/repo URL")?;

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

    let path = if let Some((authority, path)) = scp_like_remote_parts(remote_url) {
        if !is_github_host(scp_like_host(authority)?) {
            return None;
        }
        path
    } else if let Some(scheme_end) = remote_url.find("://") {
        let without_scheme = &remote_url[scheme_end + 3..];
        let path_start = without_scheme.find('/')?;
        let authority = &without_scheme[..path_start];
        if !is_github_host(url_authority_host(authority)?) {
            return None;
        }
        &without_scheme[path_start + 1..]
    } else {
        return None;
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

fn scp_like_remote_parts(remote_url: &str) -> Option<(&str, &str)> {
    let (authority, path) = remote_url.split_once(':')?;
    if authority.contains('@') && !authority.contains('/') {
        Some((authority, path))
    } else {
        None
    }
}

fn scp_like_host(authority: &str) -> Option<&str> {
    authority.rsplit_once('@').map(|(_user, host)| host)
}

fn url_authority_host(authority: &str) -> Option<&str> {
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_user, host)| host);
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some(host)
}

fn is_github_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com")
}

fn is_valid_repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
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
            parse_repo_slug("https://token@github.com/owner/repo.git"),
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
        assert_eq!(parse_repo_slug("owner/repo"), None);
        assert_eq!(parse_repo_slug("git@github.example.com:owner/repo"), None);
        assert_eq!(
            parse_repo_slug("https://github.example.com/owner/repo.git"),
            None
        );
        assert_eq!(
            parse_repo_slug("ssh://git@github.example.com/owner/repo.git"),
            None
        );
        assert_eq!(parse_repo_slug("git@github.com:owner@evil/repo.git"), None);
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo@evil.git"),
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
                .contains("git origin remote must be a github.com owner/repo URL"),
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
