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
