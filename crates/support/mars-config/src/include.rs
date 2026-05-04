//! `!include` resolver over a parsed `serde_yml::Value` tree.
//!
//! Strategy: parse a YAML document, then walk it. Whenever we see a tagged
//! scalar with tag `!include`, read+parse the referenced file (recursively),
//! substitute env vars in its source, then splice its root value in place.
//!
//! Cycle detection uses a canonicalised path stack. Includes resolve relative
//! to the *including* file's directory, not the entry config dir.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_yml::Value;

use crate::ConfigError;
use crate::env_subst::substitute;

const INCLUDE_TAG: &str = "!include";

/// Read+parse YAML with includes resolved. `path` must exist.
pub(crate) fn load_with_includes(path: &Path) -> Result<Value, ConfigError> {
    let mut stack: HashSet<PathBuf> = HashSet::new();
    load_inner(path, &mut stack)
}

fn load_inner(path: &Path, stack: &mut HashSet<PathBuf>) -> Result<Value, ConfigError> {
    let canon = fs::canonicalize(path)
        .map_err(|e| ConfigError::Io(format!("canonicalize {}: {e}", path.display())))?;
    if !stack.insert(canon.clone()) {
        return Err(ConfigError::Invalid(format!(
            "include cycle detected at {}",
            canon.display()
        )));
    }

    let raw = fs::read_to_string(&canon)
        .map_err(|e| ConfigError::Io(format!("read {}: {e}", canon.display())))?;
    let expanded = substitute(&raw)?;
    let mut value: Value = serde_yml::from_str(&expanded)
        .map_err(|e| ConfigError::Parse(format!("{}: {e}", canon.display())))?;

    let dir = canon.parent().map(Path::to_path_buf).unwrap_or_default();
    resolve(&mut value, &dir, stack)?;

    stack.remove(&canon);
    Ok(value)
}

fn resolve(
    value: &mut Value,
    base_dir: &Path,
    stack: &mut HashSet<PathBuf>,
) -> Result<(), ConfigError> {
    // tagged scalars surface via Value::Tagged in serde_yml
    if let Value::Tagged(tagged) = value
        && tagged.tag == INCLUDE_TAG
    {
        let rel = tagged
            .value
            .as_str()
            .ok_or_else(|| ConfigError::Invalid("!include argument must be a string path".into()))?;
        let target = base_dir.join(rel);
        let included = load_inner(&target, stack)?;
        *value = included;
        return Ok(());
    }

    match value {
        Value::Sequence(seq) => {
            // a sequence can contain !include items that themselves resolve to
            // sequences; flatten those one level.
            let mut out: Vec<Value> = Vec::with_capacity(seq.len());
            for mut item in std::mem::take(seq) {
                resolve(&mut item, base_dir, stack)?;
                if let Value::Sequence(inner) = item {
                    out.extend(inner);
                } else {
                    out.push(item);
                }
            }
            *seq = out;
        }
        Value::Mapping(map) => {
            // similarly, !include under a mapping value may resolve to a
            // mapping that should merge into the parent. we splice in place
            // unless the value resolves to a mapping AND the original key was
            // a placeholder marker - SPEC keeps it simple: at a mapping value
            // position, we just substitute the value (no merging). merging
            // is reserved for sequence position (e.g. layers: !include ...).
            for (_, v) in map.iter_mut() {
                resolve(v, base_dir, stack)?;
            }
        }
        Value::Tagged(tagged) => {
            resolve(&mut tagged.value, base_dir, stack)?;
        }
        _ => {}
    }
    Ok(())
}
