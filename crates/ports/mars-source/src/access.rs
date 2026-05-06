//! Adapter from a decoded attribute row to `mars_expr::AttributeAccess`.

use std::collections::HashMap;

use mars_expr::{AttributeAccess, Literal};

use crate::AttrValue;

/// Borrowed view over a row's `(name, AttrValue)` pairs that exposes
/// `mars_expr::AttributeAccess` for in-memory filter evaluation.
///
/// A name→index `HashMap` is built once per row at construction so that
/// `get` is O(1) per lookup. For class-based filter evaluation each row
/// can absorb many lookups (one per ident referenced by every class
/// `when` expression), so the per-row build cost amortises quickly.
pub struct RowAttrs<'a> {
    values: &'a [(String, AttrValue)],
    index: HashMap<&'a str, usize>,
}

impl<'a> RowAttrs<'a> {
    /// Wrap a borrowed slice of pairs. Builds an O(n) lookup index; the
    /// borrowed strings live as long as `values`.
    #[must_use]
    pub fn new(values: &'a [(String, AttrValue)]) -> Self {
        let mut index = HashMap::with_capacity(values.len());
        for (i, (k, _)) in values.iter().enumerate() {
            index.insert(k.as_str(), i);
        }
        Self { values, index }
    }
}

impl<'a> AttributeAccess for RowAttrs<'a> {
    fn get(&self, name: &str) -> Option<Literal> {
        self.index.get(name).map(|i| to_literal(&self.values[*i].1))
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
