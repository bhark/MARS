use super::*;

#[test]
fn bbox_dimensions() {
    let b = Bbox::new(0.0, 0.0, 10.0, 5.0);
    assert!((b.width() - 10.0).abs() < f64::EPSILON);
    assert!((b.height() - 5.0).abs() < f64::EPSILON);
}
