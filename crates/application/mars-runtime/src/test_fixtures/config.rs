//! minimal `Config` and `Stylesheet` builders used by both the single-layer
//! and multi-layer fixtures, plus the shared scaffolding helpers
//! (`base_config`, `default_source_binding`, `default_main_class`) that
//! factor out the parts both builders emit identically.

use std::sync::Arc;

use mars_config::model::{
    ArtifactCache, ArtifactStore, Artifacts, Band, Class, ClassStyle, Compiler, Config, Interfaces, Layer,
    Observability, Render, Scales, ServiceMeta, Source, SourceBinding,
};
use mars_style::{Colour, FillPaint, LabelStyle, LabelSurvival, Style, Stylesheet};
use mars_types::{BindingId, CrsCode, LayerId};

use super::REQUEST_CRS;

pub fn build_minimal_config(layer_id: &LayerId, binding_id: &BindingId, label_survival: LabelSurvival) -> Config {
    let mut config = base_config("test");
    config.layers = vec![Layer {
        name: layer_id.clone(),
        title: "Buildings".into(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: vec![default_source_binding(binding_id)],
        classes: vec![default_main_class()],
        label: None,
        label_survival,
        raster: None,
        wms: mars_config::LayerWms {
            enable_get_feature_info: true,
            ..Default::default()
        },
        ows: Default::default(),
        template: None,
    }];
    config
}

pub fn build_minimal_stylesheet() -> Stylesheet {
    let mut ss = Stylesheet::default();
    ss.geometry
        .insert("buildings__main".into(), Arc::from(vec![default_style()]));
    ss.labels.insert(
        "buildings__label".into(),
        Arc::new(LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0.into(),
            fill: Colour {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            halo: None,
            priority: 100,
            min_distance: 0.0,
            position: mars_style::AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle: None,
            partials: true,
            force: false,
        }),
    );
    ss
}

pub fn default_style() -> Style {
    Style {
        fill: Some(FillPaint::Solid(Colour {
            r: 200,
            g: 200,
            b: 200,
            a: 255,
        })),
        stroke: Some(Colour {
            r: 64,
            g: 64,
            b: 64,
            a: 255,
        }),
        stroke_width: Some(1.0.into()),
        ..Default::default()
    }
}

/// scaffolding shared by `build_minimal_config` and `build_multi_layer_config`:
/// service / sources / artifacts / scales / defaults filled in,
/// `layers` left empty for the caller to populate.
pub(super) fn base_config(service_name: &str) -> Config {
    Config {
        service: ServiceMeta {
            name: service_name.into(),
            ..Default::default()
        },
        sources: vec![Source {
            id: mars_config::SourceId::new("default"),
            native_crs: CrsCode::new(REQUEST_CRS),
            backend: mars_config::SourceBackend::Postgis(mars_config::PostgisBackend {
                dsn: "memory://".into(),
                change_feed: None,
                pool: Default::default(),
                bootstrap: None,
            }),
        }],
        artifacts: Artifacts {
            store: ArtifactStore {
                kind: "fs".into(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: Some("/tmp".into()),
                allow_http: false,
                ..Default::default()
            },
            cache: ArtifactCache {
                path: "/tmp".into(),
                max_size: "1GiB".into(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".into(),
                max_denom: 25_000,
            }],
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: Vec::new(),
        observability: Observability::default(),
        render: Render::default(),
        compiler: Compiler::default(),
    }
}

/// the `SourceBinding` shape used by every fixture layer. only `from`
/// (binding id) varies between call sites.
pub(super) fn default_source_binding(binding_id: &BindingId) -> SourceBinding {
    SourceBinding {
        source: mars_config::SourceId::new("default"),
        kind: mars_config::BindingKind::PostgisTable {
            from: binding_id.as_str().into(),
            geometry_column: "geom".into(),
            dsn: None,
        },
        scale: None,
        band: None,
        max_denom: None,
        filter: None,
        id_column: None,
        attributes: vec!["name".into()],
        levels: None,
        page_size_target_bytes: None,
        reconcile_every_cycles: None,
        sidecar_size_warn_bytes: None,
        simplifier: None,
        on_missing_page: None,
    }
}

/// the single inline `Class` every fixture layer carries.
pub(super) fn default_main_class() -> Class {
    Class {
        name: "main".into(),
        title: String::new(),
        when: None,
        scale: None,
        style: ClassStyle::Inline(Box::new(default_style())),
        label: None,
    }
}
