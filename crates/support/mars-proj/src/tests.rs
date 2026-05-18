#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn transformer_construction_succeeds() {
    let from = CrsCode::new("EPSG:25832");
    let to = CrsCode::new("EPSG:4326");
    Transformer::new(&from, &to).unwrap();
}

#[test]
fn transform_point_3857_to_4326_known_value() {
    let t = Transformer::new(&CrsCode::new("EPSG:3857"), &CrsCode::new("EPSG:4326")).unwrap();
    let (lon, lat) = t.transform_point(0.0, 0.0).unwrap();
    assert!(lon.abs() < 1e-9, "lon = {lon}");
    assert!(lat.abs() < 1e-9, "lat = {lat}");
}

#[test]
fn transform_point_25832_to_4326_known_value() {
    // utm 32n (725386, 6177286) -> wgs84 near copenhagen, ~ (12.586, 55.676).
    // tolerance is loose because the input easting/northing is rounded.
    let t = Transformer::new(&CrsCode::new("EPSG:25832"), &CrsCode::new("EPSG:4326")).unwrap();
    let (lon, lat) = t.transform_point(725_386.0, 6_177_286.0).unwrap();
    // round-trip back to verify, then check rough lat/lon range is plausible.
    let inv = Transformer::new(&CrsCode::new("EPSG:4326"), &CrsCode::new("EPSG:25832")).unwrap();
    let (e, n) = inv.transform_point(lon, lat).unwrap();
    assert!((e - 725_386.0).abs() < 1e-3, "round-trip easting = {e}");
    assert!((n - 6_177_286.0).abs() < 1e-3, "round-trip northing = {n}");
    // sanity: somewhere over denmark
    assert!((10.0..=15.0).contains(&lon), "lon = {lon}");
    assert!((54.0..=58.0).contains(&lat), "lat = {lat}");
}

#[test]
fn transform_bbox_densified_25832_to_4326_aabb_widens() {
    // wide utm bbox covering most of denmark; meridian convergence and
    // false-easting curvature mean densified edges bulge outward.
    let from = CrsCode::new("EPSG:25832");
    let to = CrsCode::new("EPSG:4326");
    let bbox = Bbox::new(440_000.0, 6_050_000.0, 900_000.0, 6_400_000.0);

    let dense = Transformer::with_options(&from, &to, TransformerOptions { densify_segments: 32 })
        .unwrap()
        .transform_bbox(bbox)
        .unwrap();

    // four-corner-only AABB
    let t = Transformer::with_options(&from, &to, TransformerOptions { densify_segments: 1 }).unwrap();
    let corners_only = t.transform_bbox(bbox).unwrap();

    // densified must contain the corners-only AABB...
    assert!(dense.min_x <= corners_only.min_x, "{dense:?} vs {corners_only:?}");
    assert!(dense.min_y <= corners_only.min_y, "{dense:?} vs {corners_only:?}");
    assert!(dense.max_x >= corners_only.max_x, "{dense:?} vs {corners_only:?}");
    assert!(dense.max_y >= corners_only.max_y, "{dense:?} vs {corners_only:?}");
    // ...and bulge strictly on at least one edge (otherwise densification
    // is a no-op and we haven't proven the codepath matters).
    let bulges = dense.min_x < corners_only.min_x
        || dense.min_y < corners_only.min_y
        || dense.max_x > corners_only.max_x
        || dense.max_y > corners_only.max_y;
    assert!(bulges, "densification produced no bulge: {dense:?} vs {corners_only:?}");
}

#[test]
fn transform_bbox_densified_4326_to_3857_finite() {
    let t = Transformer::new(&CrsCode::new("EPSG:4326"), &CrsCode::new("EPSG:3857")).unwrap();
    let out = t.transform_bbox(Bbox::new(-10.0, 40.0, 30.0, 60.0)).unwrap();
    for v in [out.min_x, out.min_y, out.max_x, out.max_y] {
        assert!(v.is_finite(), "non-finite component: {v}");
    }
    assert!(out.min_x < out.max_x);
    assert!(out.min_y < out.max_y);
}

#[test]
fn unknown_crs_returns_unknown_crs_error() {
    let err = Transformer::new(&CrsCode::new("EPSG:9999999"), &CrsCode::new("EPSG:4326")).unwrap_err();
    assert!(matches!(err, ProjError::UnknownCrs(_)), "got {err:?}");
}

#[test]
fn transform_points_matches_per_point_transform() {
    let t = Transformer::new(&CrsCode::new("EPSG:25832"), &CrsCode::new("EPSG:4326")).unwrap();
    let inputs: Vec<(f64, f64)> = vec![
        (725_386.0, 6_177_286.0),
        (440_000.0, 6_050_000.0),
        (900_000.0, 6_400_000.0),
        (600_000.0, 6_200_000.0),
    ];
    let mut batch: Vec<[f64; 2]> = inputs.iter().map(|&(x, y)| [x, y]).collect();
    t.transform_points(&mut batch).unwrap();
    for (i, &(x, y)) in inputs.iter().enumerate() {
        let (sx, sy) = t.transform_point(x, y).unwrap();
        assert!((batch[i][0] - sx).abs() < 1e-9, "x mismatch at {i}");
        assert!((batch[i][1] - sy).abs() < 1e-9, "y mismatch at {i}");
    }
}

#[test]
fn axis_order_geographic_crses_are_north_east() {
    for code in ["EPSG:4326", "EPSG:4258"] {
        let order = axis_order(&CrsCode::new(code)).unwrap();
        assert_eq!(order, AxisOrder::NorthEast, "{code}");
    }
}

#[test]
fn axis_order_projected_crses_are_east_north() {
    for code in ["EPSG:3857", "EPSG:25832"] {
        let order = axis_order(&CrsCode::new(code)).unwrap();
        assert_eq!(order, AxisOrder::EastNorth, "{code}");
    }
}

#[test]
fn axis_order_urn_form_resolves() {
    let order = axis_order(&CrsCode::new("urn:ogc:def:crs:EPSG::4326")).unwrap();
    assert_eq!(order, AxisOrder::NorthEast);
}

#[test]
fn axis_order_crs84_is_east_north() {
    for code in ["CRS:84", "crs:84"] {
        let order = axis_order(&CrsCode::new(code)).unwrap();
        assert_eq!(order, AxisOrder::EastNorth, "{code}");
    }
}

#[test]
fn axis_order_unknown_crs_errors() {
    let err = axis_order(&CrsCode::new("EPSG:9999999")).unwrap_err();
    assert!(matches!(err, ProjError::UnknownCrs(_)), "got {err:?}");
}

#[test]
fn transform_points_empty_is_ok() {
    let t = Transformer::new(&CrsCode::new("EPSG:25832"), &CrsCode::new("EPSG:4326")).unwrap();
    let mut empty: Vec<[f64; 2]> = vec![];
    t.transform_points(&mut empty).unwrap();
}
