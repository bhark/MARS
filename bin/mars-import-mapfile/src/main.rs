//! mars-import-mapfile: translate a MapServer mapfile into a MARS YAML config.
//! Phase 0 stub. Intentionally synchronous; no tokio dependency.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "mars-import-mapfile",
    version,
    about = "Translate a MapServer mapfile to a MARS YAML config."
)]
struct Cli {
    /// Path to the input mapfile.
    input: PathBuf,
    /// Path to write the output YAML to (defaults to stdout).
    #[arg(long)]
    out: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    eprintln!("mars-import-mapfile: Phase 0 stub. input={}", cli.input.display());
    if let Some(out) = &cli.out {
        eprintln!("(would write to {})", out.display());
    }
    anyhow::bail!("mars-import-mapfile: not implemented (SPEC §18.1) - lands in Phase 0/1")
}
