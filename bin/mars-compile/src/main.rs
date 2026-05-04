//! mars-compile: standalone snapshot compile CLI. Reuses `mars-compiler`
//! over a static source snapshot - useful for local dev, CI fixtures and
//! offline rebuilds. SPEC §18.2.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "mars-compile", version, about = "Standalone snapshot compile.")]
struct Cli {
    /// Path to the service configuration.
    #[arg(long, default_value = "/etc/mars/mars.yaml")]
    config: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    mars_observability::init_tracing(false).ok();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let _cfg = mars_config::load(&cli.config);
        tracing::info!(?cli.config, "snapshot compile (Phase 0 stub)");
        anyhow::bail!("mars-compile: not implemented (SPEC §18.2) - lands in Phase 1")
    })
}
