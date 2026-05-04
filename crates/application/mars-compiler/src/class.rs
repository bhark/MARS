//! first-match-wins class evaluation.

use mars_expr::{AttributeAccess, Expr, Literal};

/// compiled class entry: pre-parsed `when:` AST plus the style id and the
/// stable index used in the layer artifact's class_assignment section.
#[derive(Debug, Clone)]
pub struct CompiledClass {
    pub name: String,
    pub when: Option<Expr>,
    pub style_id: String,
    pub class_index: u16,
}

/// returns the first matching class index. `None` means no class matched
/// (the row is unrendered and must be dropped from class_assignment).
pub fn first_match(
    classes: &[CompiledClass],
    attrs: &dyn AttributeAccess,
) -> Result<Option<u16>, mars_expr::ExprError> {
    for c in classes {
        let matched = match &c.when {
            None => true,
            Some(e) => matches!(mars_expr::eval(e, attrs)?, Literal::Bool(true)),
        };
        if matched {
            return Ok(Some(c.class_index));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;

    use mars_expr::{AttributeAccess, Expr, Literal};

    use super::{CompiledClass, first_match};

    struct Map(HashMap<String, Literal>);
    impl AttributeAccess for Map {
        fn get(&self, name: &str) -> Option<Literal> {
            self.0.get(name).cloned()
        }
    }
    fn attrs(pairs: &[(&str, Literal)]) -> Map {
        Map(pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect())
    }

    fn compile_expr(s: &str) -> Expr {
        mars_expr::parse(s).unwrap()
    }

    #[test]
    fn no_classes_returns_none() {
        assert_eq!(first_match(&[], &attrs(&[])).unwrap(), None);
    }

    #[test]
    fn class_without_when_always_matches() {
        let classes = vec![CompiledClass {
            name: "default".into(),
            when: None,
            style_id: "s1".into(),
            class_index: 0,
        }];
        assert_eq!(first_match(&classes, &attrs(&[])).unwrap(), Some(0));
    }

    #[test]
    fn first_match_wins_ordering() {
        let classes = vec![
            CompiledClass {
                name: "a".into(),
                when: Some(compile_expr("x = 1")),
                style_id: "s_a".into(),
                class_index: 0,
            },
            CompiledClass {
                name: "b".into(),
                when: Some(compile_expr("x = 1")),
                style_id: "s_b".into(),
                class_index: 1,
            },
        ];
        assert_eq!(
            first_match(&classes, &attrs(&[("x", Literal::Int(1))])).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn no_match_returns_none() {
        let classes = vec![CompiledClass {
            name: "a".into(),
            when: Some(compile_expr("x = 1")),
            style_id: "s_a".into(),
            class_index: 0,
        }];
        assert_eq!(first_match(&classes, &attrs(&[("x", Literal::Int(2))])).unwrap(), None);
    }

    #[test]
    fn eval_error_bubbles_up() {
        // unknown identifier causes ExprError::UnknownIdent
        let classes = vec![CompiledClass {
            name: "a".into(),
            when: Some(compile_expr("missing = 1")),
            style_id: "s_a".into(),
            class_index: 0,
        }];
        assert!(first_match(&classes, &attrs(&[])).is_err());
    }

    #[test]
    fn mixed_when_and_unconditional() {
        let classes = vec![
            CompiledClass {
                name: "conditional".into(),
                when: Some(compile_expr("x = 1")),
                style_id: "s1".into(),
                class_index: 0,
            },
            CompiledClass {
                name: "fallback".into(),
                when: None,
                style_id: "s2".into(),
                class_index: 1,
            },
        ];
        // matches conditional
        assert_eq!(
            first_match(&classes, &attrs(&[("x", Literal::Int(1))])).unwrap(),
            Some(0)
        );
        // falls through to fallback
        assert_eq!(
            first_match(&classes, &attrs(&[("x", Literal::Int(2))])).unwrap(),
            Some(1)
        );
    }
}
