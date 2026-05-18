//! Compiler mode: subscribe to the source change feed, build artifacts,
//! publish manifests. Also wires the all-in-one mode that runs compiler +
//! runtime in one process.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use mars_bin_shared::{build_pg_source, build_sources, build_store_and_publisher};
use mars_compiler::{Compiler, Deps as CompilerDeps};
use mars_config::Config;
use tokio_util::sync::CancellationToken;

use crate::boot::runtime;
use crate::composition;

pub(crate) async fn run(cfg: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    composition::validate_change_feed_config(&cfg)?;
    let topology = composition::build_replication_topology(&cfg)?;
    let pg_source = build_pg_source(&cfg, Some(topology)).await?;
    let registry = build_sources(&cfg, Some(pg_source.clone())).await?;
    let (store, publisher) = build_store_and_publisher(&cfg)?;
    let metrics = mars_observability::Metrics::new().context("init metrics")?;

    // Compiler::new takes Config by value; clone out of the Arc once at handoff.
    let compiler = Compiler::new(
        CompilerDeps {
            sources: Arc::new(registry),
            change_feed: pg_source.clone(),
            leader_lock: pg_source,
            store,
            manifest: publisher,
            metrics,
        },
        (*cfg).clone(),
    );
    match compiler.run(shutdown).await {
        Ok(()) => Ok(()),
        Err(mars_compiler::CompilerError::NotLeader) => {
            tracing::info!("compiler: another instance is leader; exiting cleanly");
            Ok(())
        }
        Err(e) => Err(anyhow!(e)),
    }
}

pub(crate) async fn run_all_in_one(cfg: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    // spawn both halves so we can observe the first to finish and cancel the
    // shared shutdown *before* awaiting the survivor's drain. try_join! would
    // drop the survivor's future mid-await on a sibling failure, so its HTTP
    // graceful drain never runs. we want the survivor to see the cancellation
    // and shut down cleanly.
    let mut compiler_handle = tokio::spawn(run(cfg.clone(), shutdown.clone()));
    let mut runtime_handle = tokio::spawn(runtime::run(cfg, shutdown.clone()));

    let first = tokio::select! {
        res = &mut compiler_handle => ("compiler", res),
        res = &mut runtime_handle => ("runtime", res),
    };
    shutdown.cancel();

    let (first_name, first_res) = first;
    let (compiler_res, runtime_res) = if first_name == "compiler" {
        (first_res, runtime_handle.await)
    } else {
        (compiler_handle.await, first_res)
    };

    flatten_join(compiler_res, "compiler")?;
    flatten_join(runtime_res, "runtime")?;
    Ok(())
}

fn flatten_join(res: Result<Result<()>, tokio::task::JoinError>, what: &str) -> Result<()> {
    match res {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.context(format!("{what} task"))),
        Err(e) => Err(anyhow!(e).context(format!("{what} task panicked"))),
    }
}
