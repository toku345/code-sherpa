//! code-sherpa CLI entry point.
//!
//! Parses the target issue and prints the planned pipeline. Stage
//! orchestration (the deterministic state machine) is layered on top of the
//! primitives in `code_sherpa` and is not implemented yet.

use clap::Parser;
use code_sherpa::{PipelineContext, Stage};

/// Guide a GitHub Issue from detection to merge.
#[derive(Parser)]
#[command(name = "code-sherpa", version, about)]
struct Cli {
    /// Issue number to drive through the pipeline.
    issue_number: u64,
    /// Target repository in `owner/repo` form.
    #[arg(short, long)]
    repo: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let ctx = PipelineContext::new(cli.issue_number, cli.repo);

    eprintln!(
        "code-sherpa: issue #{} in {} (pipeline not yet implemented)",
        ctx.issue_number, ctx.repo
    );
    eprintln!("stages: {:?}", Stage::ALL.map(|s| s.as_str()));
    anyhow::bail!("pipeline orchestration is not implemented yet")
}
