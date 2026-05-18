//! Snapshot orchestrator built on the unified compile pipeline:
//! per binding, open a `CompileSession`, run [`crate::page_plan::compute_page_plan`]
//! for pass 1, hand the resulting `PagePlan` to
//! [`crate::render::rebuild_binding_from_plan`] for pass 2, fold the
//! emitted artifacts into a fresh `Manifest`. Bindings compile concurrently
//! up to `binding_parallelism` (each holds one pooled connection in
//! `REPEATABLE READ`); the operator must size `source.pool.max_size`
//! accordingly. Returns the manifest for the caller to publish.
//!
//! Re-exported at the crate root as `mars_compiler::run_snapshot_from_plan`
//! because external tests and benches pin that path.

use std::path::Path;
use std::time::{Instant, SystemTime};

use futures_util::StreamExt;
use mars_source::{SourceBinding as PortBinding, SourceCollectionId};
use mars_types::{BindingMetadata, LayerSidecarEntry, MANIFEST_FORMAT_VERSION, Manifest, PageEntry};

use crate::disk_governor::DiskGovernor;
use crate::memory_governor::MemoryGovernor;
use crate::plan::{BindingPlan, BootstrapPlan};
use crate::render::{self, BindingOutput};
use crate::{CompilerError, Deps, page_plan};

#[allow(clippy::too_many_arguments)]
pub async fn run_snapshot_from_plan(
    deps: &Deps,
    bootstrap: &BootstrapPlan,
    service_name: String,
    manifest_version: u64,
    working_set_bytes: u64,
    plan_budget_bytes: u64,
    in_flight_budget_bytes: u64,
    binding_parallelism: usize,
    spill_dir: &Path,
    spill_open_file_limit: usize,
    governor: &MemoryGovernor,
    disk_governor: &DiskGovernor,
) -> Result<Manifest, CompilerError> {
    let parallelism = binding_parallelism.max(1);

    let mut pending = futures_util::stream::FuturesUnordered::new();
    let mut iter = bootstrap.bindings.iter();
    let mut outputs: Vec<BindingOutput> = Vec::with_capacity(bootstrap.bindings.len());
    loop {
        while pending.len() < parallelism
            && let Some(binding_plan) = iter.next()
        {
            pending.push(compile_one(
                deps,
                bootstrap,
                binding_plan,
                working_set_bytes,
                plan_budget_bytes,
                in_flight_budget_bytes,
                spill_dir,
                spill_open_file_limit,
                governor,
                disk_governor,
            ));
        }
        match pending.next().await {
            Some(Ok(out)) => outputs.push(out),
            Some(Err(err)) => return Err(err),
            None => break,
        }
    }

    Ok(assemble_manifest(outputs, bootstrap, service_name, manifest_version))
}

#[allow(clippy::too_many_arguments)]
async fn compile_one(
    deps: &Deps,
    bootstrap: &BootstrapPlan,
    binding_plan: &BindingPlan,
    working_set_bytes: u64,
    plan_budget_bytes: u64,
    in_flight_budget_bytes: u64,
    spill_dir: &Path,
    spill_open_file_limit: usize,
    governor: &MemoryGovernor,
    disk_governor: &DiskGovernor,
) -> Result<BindingOutput, CompilerError> {
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding_plan.binding_id.as_str()),
        binding_plan.source_table.clone(),
        binding_plan.geometry_field.clone(),
        binding_plan.id_field.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?
    .with_filter(binding_plan.filter.clone())
    .with_dsn(binding_plan.dsn.clone());
    let started = Instant::now();
    tracing::info!(
        target: "mars_compiler::compile",
        binding = %binding_plan.binding_id,
        "compile.binding.start",
    );
    let source = deps.source_for(binding_plan)?;
    let mut session = source.open_compile_session(&port_binding).await?;
    let work = async {
        let page_plan =
            page_plan::compute_page_plan(session.as_mut(), binding_plan, plan_budget_bytes, spill_dir).await?;
        render::rebuild_binding_from_plan(
            deps,
            bootstrap,
            binding_plan,
            &page_plan,
            session.as_mut(),
            working_set_bytes,
            in_flight_budget_bytes,
            spill_dir,
            spill_open_file_limit,
            governor,
            disk_governor,
        )
        .await
    }
    .await;
    match work {
        Ok(out) => {
            session.commit().await?;
            tracing::info!(
                target: "mars_compiler::compile",
                binding = %binding_plan.binding_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                pages = out.pages.len(),
                levels = out.meta.levels.len(),
                feature_count_total = out.meta.feature_count_total,
                "compile.binding.end",
            );
            Ok(out)
        }
        Err(err) => {
            if let Err(rb) = session.rollback().await {
                tracing::warn!(error = %rb, "compile session rollback failed");
            }
            tracing::info!(
                target: "mars_compiler::compile",
                binding = %binding_plan.binding_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %err,
                "compile.binding.end",
            );
            Err(err)
        }
    }
}

/// Fold per-binding outputs into one manifest with stable ordering. Sort
/// keys mirror manifest read paths so two compiles of the same source
/// produce byte-identical manifests under any binding-completion order.
fn assemble_manifest(
    outputs: Vec<BindingOutput>,
    bootstrap: &BootstrapPlan,
    service_name: String,
    version: u64,
) -> Manifest {
    let mut bindings_meta: Vec<BindingMetadata> = Vec::with_capacity(outputs.len());
    let mut pages_meta: Vec<PageEntry> = Vec::new();
    let mut class_sidecars: Vec<LayerSidecarEntry> = Vec::new();
    let mut label_sidecars: Vec<LayerSidecarEntry> = Vec::new();

    for mut out in outputs {
        bindings_meta.push(out.meta);
        pages_meta.append(&mut out.pages);
        class_sidecars.append(&mut out.class_sidecars);
        label_sidecars.append(&mut out.label_sidecars);
    }

    bindings_meta.sort_by(|a, b| a.binding_id.as_str().cmp(b.binding_id.as_str()));
    pages_meta.sort_by(|a, b| {
        a.key
            .binding_id
            .as_str()
            .cmp(b.key.binding_id.as_str())
            .then_with(|| a.key.level.cmp(&b.key.level))
            .then_with(|| a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });
    let sidecar_cmp = |a: &LayerSidecarEntry, b: &LayerSidecarEntry| {
        a.layer_id
            .as_str()
            .cmp(b.layer_id.as_str())
            .then_with(|| a.page_key.binding_id.as_str().cmp(b.page_key.binding_id.as_str()))
            .then_with(|| a.page_key.level.cmp(&b.page_key.level))
            .then_with(|| a.page_key.page_id.cmp(&b.page_key.page_id))
    };
    class_sidecars.sort_by(sidecar_cmp);
    label_sidecars.sort_by(sidecar_cmp);

    Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version,
        service: service_name,
        created_at: SystemTime::now(),
        bindings: bindings_meta,
        pages: pages_meta,
        class_sidecars,
        label_sidecars,
        style_artifact: None,
        image_artifact: None,
        raster_layers: bootstrap.raster_layers.clone(),
        source_version: None,
        epoch: version,
    }
}
