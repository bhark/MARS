//! mars-operator: Kubernetes operator for MarsService custom resources.
//!
//! Reconciles a MarsService CR into ConfigMap + PVCs + compiler/runtime
//! Deployments + Service. Single-binary operator, leader-elected, owns its
//! children via owner references for cascade GC. See plan in CLAUDE.md /
//! design docs - this crate is a composition root and may freely reach into
//! infrastructure crates; the hexagonal layering rules apply to library
//! crates only.

#![forbid(unsafe_code)]

mod bootstrap;
mod children;
mod cli;
mod config;
mod controller;
mod crd;
mod dsn;
mod error;
mod metrics;
mod reconcile;
mod status;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // print-crd is pure and synchronous; run before touching tokio so the
    // CI drift check works in minimal environments.
    if let Some(Command::PrintCrd) = cli.command {
        return crd::print_crd();
    }

    mars_observability::init_tracing(cli.log_format.is_json(), Some(&cli.log_level)).context("init tracing")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(controller::run(cli))
}
