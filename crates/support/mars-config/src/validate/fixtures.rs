#[cfg(test)]
use crate::model::*;
#[cfg(test)]
use mars_types::{Bbox, CrsCode};

#[cfg(test)]
pub(crate) fn minimal_config() -> Config {
    use crate::model::{ArtifactCache, ArtifactStore, Compiler, Interfaces, Observability, Render};
    let mut size_per_band = std::collections::BTreeMap::new();
    size_per_band.insert("hi".into(), "1024m".into());
    Config {
        service: ServiceMeta {
            name: "test".into(),
            ..Default::default()
        },
        source: Source {
            kind: "memory".into(),
            dsn: "memory://".into(),
            native_crs: CrsCode::new("EPSG:25832"),
            change_feed: None,
            pool: Default::default(),
        },
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
                eviction: "lru".into(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".into(),
                max_denom: 25000,
            }],
        },
        cells: Cells {
            grid: "regular".into(),
            origin: [0.0, 0.0],
            size_per_band,
            extent: Some(Bbox::new(0.0, 0.0, 1_000.0, 1_000.0)),
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
        scale: None,
        band: None,
        max_denom: None,
        filter: None,
        from: Some(from.into()),
        sql: None,
        geometry_column: "geom".into(),
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
        enable_get_feature_info: false,
        bbox: None,
        sources: vec![],
        classes: vec![],
        label: None,
        label_survival: mars_style::LabelSurvival::Independent,
        raster: None,
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
        style: ClassStyle::Inline(Default::default()),
    }
}

#[cfg(test)]
pub(crate) fn inline_label(text: &str, placement: Option<mars_style::Placement>) -> LayerLabel {
    LayerLabel {
        text: text.into(),
        style: LabelStyleAttach::Inline(mars_style::LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0,
            fill: mars_style::Colour::rgb(0, 0, 0),
            halo: None,
            priority: 0,
            min_distance: 0.0,
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
    cfg.cells.size_per_band.insert("mid".into(), "4096m".into());
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
        enable_get_feature_info: false,
        bbox: None,
        sources,
        classes: vec![],
        label: None,
        label_survival: mars_style::LabelSurvival::Independent,
        raster: None,
    }
}
