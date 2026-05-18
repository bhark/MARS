use super::*;

#[test]
fn flow_array_quotes_and_separates() {
    assert_eq!(flow_array(&["get", "list"]), r#"["get", "list"]"#);
    assert_eq!(flow_array(&[""]), r#"[""]"#);
    assert_eq!(flow_array(&[]), "[]");
}

#[test]
fn every_rule_has_verbs() {
    for section in RULES {
        for rule in section.rules {
            assert!(!rule.verbs.is_empty(), "rule {rule:?} has no verbs");
            assert!(!rule.resources.is_empty(), "rule {rule:?} has no resources");
        }
    }
}
