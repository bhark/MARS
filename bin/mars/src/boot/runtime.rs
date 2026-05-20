//! Runtime mode: serve WMS / WMTS / health / metrics, hot-swap state on
//! manifest changes.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use mars_bin_shared::{build_store_and_publisher, build_stylesheet, load_fonts};
use mars_config::{Config, PngCompression as ConfigPngCompression};
use mars_render::{PngCompression as RenderPngCompression, TinySkiaEncoder, TinySkiaRenderer};
use mars_runtime::{Deps as RuntimeDeps, Runtime, RuntimeState, run_manifest_reload_loop};
use mars_store::{LocalCache, ManifestStore};
use mars_store_fs::FsCache;
use mars_types::Manifest;
use tokio_util::sync::CancellationToken;

use crate::composition;

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8080";

pub(crate) async fn run(cfg: Arc<Config>, shutdown: CancellationToken) -> Result<()> {
    let (store, publisher) = build_store_and_publisher(&cfg)?;
    let cache = build_cache(&cfg)?;
    let stylesheet = build_stylesheet(&cfg);
    let fonts = load_fonts(&cfg)?;

    let listen = resolve_listen(&cfg)?;
    let wms_cfg = mars_wms::WmsConfig::from_config(&cfg);
    let wmts_cfg = mars_wmts::WmtsConfig::from_config(&cfg);
    let metrics = mars_observability::Metrics::new().context("init metrics")?;
    let pixel_budget = mars_runtime::resolve_pixel_budget(&cfg.render).context("resolve render.pixel_budget")?;
    let images = Arc::new(mars_runtime::images::MutableImageRegistry::new());
    let raster_sources = composition::build_raster_sources(&cfg).context("build raster source registry")?;
    let runtime = Arc::new(Runtime::with_pixel_budget(
        RuntimeDeps {
            store,
            cache,
            renderer: Arc::new(TinySkiaRenderer::with_images(fonts.clone(), images.clone())),
            encoder: Arc::new(TinySkiaEncoder::new(
                cfg.render.jpeg_quality,
                map_png_compression(cfg.render.png_compression),
            )),
            metrics: metrics.clone(),
            fonts,
            images,
            raster_sources,
        },
        pixel_budget,
        None,
    ));

    let manifest_opt = match publisher.current().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "initial manifest unavailable");
            None
        }
    };

    match &manifest_opt {
        Some(manifest) => {
            match mars_runtime::images::load_from_manifest(
                manifest.image_artifact.as_ref(),
                &runtime.deps().cache,
                &runtime.deps().store,
            )
            .await
            {
                Ok(map) => runtime.deps().images.set(map),
                Err(e) => tracing::warn!(error = %e, "initial image_artifact load failed"),
            }
            match RuntimeState::from_config_and_manifest(&cfg, stylesheet.clone(), manifest.clone()) {
                Ok(state) => runtime.swap_state(Arc::new(state)),
                Err(e) => tracing::warn!(error = %e, "initial manifest rejected"),
            }
        }
        None => {
            tracing::warn!("no manifest published yet; readyz will return 503");
        }
    }

    let manifests: Arc<dyn ManifestStore> = publisher.clone();
    let reload_task = tokio::spawn({
        let runtime = runtime.clone();
        let cfg = cfg.clone();
        let stylesheet = stylesheet.clone();
        let manifests = manifests.clone();
        let shutdown = shutdown.clone();
        async move {
            if let Err(e) = run_manifest_reload_loop(runtime, manifests, cfg, stylesheet, shutdown).await {
                tracing::error!(error = %e, "manifest reload loop stopped");
            }
        }
    });

    let initial_manifest_for_caps = manifest_opt.clone().unwrap_or_else(|| empty_manifest(&cfg));
    let initial_wms_caps_130 = mars_wms::capabilities_xml(&cfg, &initial_manifest_for_caps, mars_wms::WmsVersion::V130)
        .map_err(|e| anyhow!("wms 1.3.0 capabilities: {e}"))?;
    let initial_wms_caps_111 = mars_wms::capabilities_xml(&cfg, &initial_manifest_for_caps, mars_wms::WmsVersion::V111)
        .map_err(|e| anyhow!("wms 1.1.1 capabilities: {e}"))?;
    let initial_wmts_caps =
        mars_wmts::capabilities_xml(&cfg, &initial_manifest_for_caps).map_err(|e| anyhow!("wmts capabilities: {e}"))?;
    let caps_bundle = mars_http::CapabilitiesBundle {
        wms: mars_http::WmsCapabilitiesHandles {
            v111: mars_http::capabilities_handle(initial_wms_caps_111),
            v130: mars_http::capabilities_handle(initial_wms_caps_130),
        },
        wmts: mars_http::capabilities_handle(initial_wmts_caps),
    };

    let caps_task = tokio::spawn(rebuild_capabilities_loop(
        manifests.clone(),
        cfg.clone(),
        caps_bundle.clone(),
        metrics.clone(),
        shutdown.clone(),
    ));

    let serve_result = mars_http::serve(
        mars_http::ServerConfig { listen },
        runtime,
        caps_bundle,
        mars_http::InterfacesConfig {
            wms: wms_cfg,
            wmts: wmts_cfg,
            cors: cfg.interfaces.cors.clone(),
            gfi_templates: mars_wms::GfiTemplates::from_config(&cfg),
        },
        metrics,
        shutdown.clone(),
    )
    .await;

    // signal background loops to drain. capabilities/manifest watch streams
    // close when the underlying store is dropped, but cancelling now lets us
    // tear down promptly even when the watch is mid-poll.
    shutdown.cancel();
    let drain = Duration::from_secs(30);
    if tokio::time::timeout(drain, async {
        let _ = tokio::join!(reload_task, caps_task);
    })
    .await
    .is_err()
    {
        tracing::warn!("background tasks did not drain within {}s", drain.as_secs());
    }

    serve_result.map_err(Into::into)
}

/// Subscribe to the manifest watch stream and atomically swap the cached
/// capabilities body whenever the manifest changes. Errors on the watch are
/// logged; the task keeps running so transient adapter failures do not freeze
/// the capabilities document.
async fn rebuild_capabilities_loop(
    manifests: Arc<dyn ManifestStore>,
    cfg: Arc<Config>,
    handles: mars_http::CapabilitiesBundle,
    metrics: mars_observability::Metrics,
    shutdown: CancellationToken,
) {
    let mut stream = match manifests.watch().await {
        Ok(s) => s,
        Err(e) => {
            metrics.inc_capabilities_rebuild_failures();
            tracing::error!(error = %e, "capabilities: manifest watch unavailable");
            return;
        }
    };
    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            n = stream.next() => match n {
                Some(n) => n,
                None => return,
            },
        };
        let manifest = match next {
            Ok(m) => m,
            Err(e) => {
                metrics.inc_capabilities_rebuild_failures();
                tracing::warn!(error = %e, "capabilities: ignoring invalid snapshot");
                continue;
            }
        };
        match mars_wms::capabilities_xml(&cfg, &manifest, mars_wms::WmsVersion::V130) {
            Ok(body) => handles.wms.v130.store(Arc::new(mars_http::CapabilitiesDoc::new(body))),
            Err(e) => {
                metrics.inc_capabilities_rebuild_failures();
                tracing::error!(error = %e, "capabilities: wms 1.3.0 rebuild failed");
            }
        }
        match mars_wms::capabilities_xml(&cfg, &manifest, mars_wms::WmsVersion::V111) {
            Ok(body) => handles.wms.v111.store(Arc::new(mars_http::CapabilitiesDoc::new(body))),
            Err(e) => {
                metrics.inc_capabilities_rebuild_failures();
                tracing::error!(error = %e, "capabilities: wms 1.1.1 rebuild failed");
            }
        }
        match mars_wmts::capabilities_xml(&cfg, &manifest) {
            Ok(body) => handles.wmts.store(Arc::new(mars_http::CapabilitiesDoc::new(body))),
            Err(e) => {
                metrics.inc_capabilities_rebuild_failures();
                tracing::error!(error = %e, "capabilities: wmts rebuild failed");
            }
        }
    }
}

fn build_cache(cfg: &Config) -> Result<Arc<dyn LocalCache>> {
    let max = cfg
        .artifacts
        .cache
        .max_size_bytes()
        .map_err(|e| anyhow!("parse cache max_size: {e}"))?;
    Ok(Arc::new(
        FsCache::with_trust_path_hash(&cfg.artifacts.cache.path, max, cfg.artifacts.cache.trust_path_hash)
            .context("open fs cache")?,
    ))
}

fn empty_manifest(cfg: &Config) -> Manifest {
    Manifest::empty(0, cfg.service.name.clone())
}

fn map_png_compression(c: ConfigPngCompression) -> RenderPngCompression {
    match c {
        ConfigPngCompression::None => RenderPngCompression::None,
        ConfigPngCompression::Fastest => RenderPngCompression::Fastest,
        ConfigPngCompression::Fast => RenderPngCompression::Fast,
        ConfigPngCompression::Balanced => RenderPngCompression::Balanced,
        ConfigPngCompression::High => RenderPngCompression::High,
    }
}

fn resolve_listen(cfg: &Config) -> Result<SocketAddr> {
    let raw = cfg
        .interfaces
        .wms
        .as_ref()
        .and_then(|w| w.listen.clone())
        .or_else(|| std::env::var("MARS_HTTP_LISTEN").ok())
        .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_owned());
    SocketAddr::from_str(&raw).with_context(|| format!("parse listen addr {raw:?}"))
}
