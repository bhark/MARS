//! MARS service binary. Composition root; see `cli`, `boot`, `tooling`,
//! `composition` modules for the actual logic.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use clap::Parser;
use mars_config::Config;

mod boot;
mod cli;
mod composition;
mod tooling;

use cli::{Cli, Mode, Tool};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // healthcheck is a one-shot blocking HTTP probe; building a multi-thread
    // tokio runtime to host a synchronous reqwest::blocking call would just
    // block a worker. short-circuit before the runtime is built.
    if let Some(Tool::Healthcheck { url }) = &cli.tool {
        return tooling::healthcheck(url);
    }

    // service modes need a validated Config; tool subcommands don't. load
    // once here so observability prefs and the chosen mode share one parse.
    let cfg = if cli.mode.is_some() {
        Some(Arc::new(composition::load_and_validate(&cli.config)?))
    } else {
        None
    };

    let (json, log_level) = cfg.as_ref().map_or((false, None), |c| {
        (
            matches!(c.observability.log_format.as_deref(), Some("json")),
            c.observability.log_level.clone(),
        )
    });
    if let Err(e) = mars_observability::init_tracing(json, log_level.as_deref()) {
        eprintln!("warning: tracing init failed: {e}");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async_main(cli, cfg))
}

async fn async_main(cli: Cli, cfg: Option<Arc<Config>>) -> Result<()> {
    // clap's `conflicts_with` on `mode` rules out the (Some, Some) case at
    // parse time; only one branch can populate. Healthcheck is handled
    // before the runtime is built, so it never reaches this match.
    match (cli.mode, cli.tool) {
        (None, None) => Err(anyhow!(
            "mars: provide --mode <runtime|compiler|all-in-one> or one of: validate, inspect, healthcheck, setup, teardown"
        )),
        (Some(Mode::Runtime), None) => {
            let cfg = cfg.ok_or_else(|| anyhow!("internal: service mode without loaded config"))?;
            let shutdown = boot::shutdown::install_signal_handler();
            boot::runtime::run(cfg, shutdown).await
        }
        (Some(Mode::Compiler), None) => {
            let cfg = cfg.ok_or_else(|| anyhow!("internal: service mode without loaded config"))?;
            let shutdown = boot::shutdown::install_signal_handler();
            boot::compiler::run(cfg, shutdown).await
        }
        (Some(Mode::AllInOne), None) => {
            let cfg = cfg.ok_or_else(|| anyhow!("internal: service mode without loaded config"))?;
            let shutdown = boot::shutdown::install_signal_handler();
            boot::compiler::run_all_in_one(cfg, shutdown).await
        }
        (None, Some(Tool::Validate { path })) => tooling::validate(&path),
        (None, Some(Tool::Inspect { path })) => tooling::inspect(&path),
        (None, Some(Tool::Healthcheck { .. })) => {
            unreachable!("healthcheck is handled in main() before the tokio runtime is built")
        }
        (
            None,
            Some(Tool::Setup {
                config,
                admin_dsn,
                runtime_password,
                dry_run,
            }),
        ) => tooling::setup(&config, &admin_dsn, runtime_password, dry_run).await,
        (
            None,
            Some(Tool::Teardown {
                config,
                admin_dsn,
                drop_slot,
                drop_publication,
                drop_role,
                dry_run,
            }),
        ) => tooling::teardown(&config, &admin_dsn, drop_slot, drop_publication, drop_role, dry_run).await,
        (Some(_), Some(_)) => unreachable!("clap conflicts_with rules this out at parse time"),
    }
}
