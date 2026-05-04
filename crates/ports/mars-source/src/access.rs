//! Adapter from a decoded attribute row to `mars_expr::AttributeAccess`.

use mars_expr::{AttributeAccess, Literal};

use crate::AttrValue;

/// Borrowed view over a row's `(name, AttrValue)` pairs that exposes
/// `mars_expr::AttributeAccess` for in-memory filter evaluation.
pub struct RowAttrs<'a>(&'a [(String, AttrValue)]);

impl<'a> RowAttrs<'a> {
    /// Wrap a borrowed slice of pairs. O(n) lookups; rows are short.
    #[must_use]
    pub fn new(values: &'a [(String, AttrValue)]) -> Self {
        Self(values)
    }
}

impl<'a> AttributeAccess for RowAttrs<'a> {
    fn get(&self, name: &str) -> Option<Literal> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| to_literal(v))
    }
}

fn to_literal(v: &AttrValue) -> Literal {
    match v {
        AttrValue::Null => Literal::Null,
        AttrValue::Bool(b) => Literal::Bool(*b),
        AttrValue::Int(i) => Literal::Int(*i),
        AttrValue::Float(f) => Literal::Float(*f),
        AttrValue::String(s) => Literal::String(s.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample() -> Vec<(String, AttrValue)> {
        vec![
            ("n".into(), AttrValue::Null),
            ("b".into(), AttrValue::Bool(false)),
            ("i".into(), AttrValue::Int(7)),
            ("f".into(), AttrValue::Float(1.5)),
            ("s".into(), AttrValue::String("hi".into())),
        ]
    }

    #[test]
    fn mapping_is_total_and_lossless() {
        let row = sample();
        let a = RowAttrs::new(&row);
        assert_eq!(a.get("n"), Some(Literal::Null));
        assert_eq!(a.get("b"), Some(Literal::Bool(false)));
        assert_eq!(a.get("i"), Some(Literal::Int(7)));
        assert_eq!(a.get("f"), Some(Literal::Float(1.5)));
        assert_eq!(a.get("s"), Some(Literal::String("hi".into())));
    }

    #[test]
    fn unknown_ident_returns_none() {
        let row = sample();
        let a = RowAttrs::new(&row);
        assert!(a.get("missing").is_none());
    }
}
