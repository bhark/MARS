#[cfg(test)]
use crate::SourceId;
#[cfg(test)]
use crate::model::*;
#[cfg(test)]
use mars_types::CrsCode;

#[cfg(test)]
pub(crate) const TEST_SOURCE_ID: &str = "pg";

#[cfg(test)]
pub(crate) fn minimal_config() -> Config {
    use crate::model::{ArtifactCache, ArtifactStore, Compiler, Interfaces, Observability, Render};
    Config {
        service: ServiceMeta {
            name: "test".into(),
            ..Default::default()
        },
        sources: vec![Source {
            id: SourceId::new(TEST_SOURCE_ID),
            native_crs: CrsCode::new("EPSG:25832"),
            backend: SourceBackend::Postgis(PostgisBackend {
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
                max_denom: 25000,
            }],
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: vec![],
        observability: Observability::default(),
        render: Render::default(),
        compiler: Compiler::default(),
    }
}

#[cfg(test)]
pub(crate) fn binding(from: &str) -> SourceBinding {
    SourceBinding {
        source: SourceId::new(TEST_SOURCE_ID),
        kind: BindingKind::PostgisTable {
            from: from.into(),
            geometry_column: "geom".into(),
            dsn: None,
        },
        scale: None,
        band: None,
        max_denom: None,
        filter: None,
        id_column: Some("id".into()),
        attributes: vec![],
        levels: None,
        page_size_target_bytes: None,
        reconcile_every_cycles: None,
        sidecar_size_warn_bytes: None,
        simplifier: None,
        on_missing_page: None,
    }
}

#[cfg(test)]
pub(crate) fn layer(name: &str) -> Layer {
    Layer {
        name: mars_types::LayerId::new(name),
        title: String::new(),
        abstract_: String::new(),
        kind: "line".into(),
        scale: None,
        group: None,
        bbox: None,
        sources: vec![],
        classes: vec![],
        label: None,
        label_survival: mars_style::LabelSurvival::Independent,
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    }
}

#[cfg(test)]
pub(crate) fn layer_with_binding(binding: SourceBinding) -> Layer {
    let mut l = layer("roads");
    l.sources = vec![binding];
    l
}

#[cfg(test)]
pub(crate) fn class_inline(name: &str, when: Option<&str>) -> Class {
    Class {
        name: name.into(),
        title: String::new(),
        when: when.map(Into::into),
        scale: None,
        style: ClassStyle::Inline(Box::default()),
        label: None,
    }
}

#[cfg(test)]
pub(crate) fn inline_label(text: &str, placement: Option<mars_style::Placement>) -> LayerLabel {
    LayerLabel {
        text: text.into(),
        style: LabelStyleAttach::Inline(mars_style::LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0.into(),
            fill: mars_style::Colour::rgb(0, 0, 0),
            halo: None,
            priority: 0,
            min_distance: 0.0,
            position: mars_style::AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle: None,
            partials: false,
            force: false,
        }),
        placement,
    }
}

#[cfg(test)]
pub(crate) fn two_band_config() -> Config {
    let mut cfg = minimal_config();
    cfg.scales.bands = vec![
        Band {
            name: "hi".into(),
            max_denom: 25_000,
        },
        Band {
            name: "mid".into(),
            max_denom: 250_000,
        },
    ];
    cfg
}

#[cfg(test)]
pub(crate) fn tiered_layer(sources: Vec<SourceBinding>) -> Layer {
    Layer {
        name: mars_types::LayerId::new("test"),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
        bbox: None,
        sources,
        classes: vec![],
        label: None,
        label_survival: mars_style::LabelSurvival::Independent,
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
    }
}
