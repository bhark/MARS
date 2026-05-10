//! `!include` resolver over a parsed `serde_yaml_ng::Value` tree.
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

use serde_yaml_ng::Value;

use crate::ConfigError;
use crate::env_subst::substitute;

const INCLUDE_TAG: &str = "!include";

/// Read+parse YAML with includes resolved. `path` must exist.
/// All included files must reside under `root` (typically the config file's
/// directory) to prevent directory traversal.
pub(crate) fn load_with_includes(path: &Path, root: &Path) -> Result<Value, ConfigError> {
    let root = if root.as_os_str().is_empty() {
        std::env::current_dir().map_err(|e| ConfigError::Io {
            context: "current dir".into(),
            source: e,
        })?
    } else {
        root.to_path_buf()
    };
    let root = fs::canonicalize(&root).map_err(|e| ConfigError::Io {
        context: format!("canonicalize config root {}", root.display()),
        source: e,
    })?;
    let mut stack: HashSet<PathBuf> = HashSet::new();
    load_inner(path, &root, &mut stack)
}

fn load_inner(path: &Path, root: &Path, stack: &mut HashSet<PathBuf>) -> Result<Value, ConfigError> {
    let canon = fs::canonicalize(path).map_err(|e| ConfigError::Io {
        context: format!("canonicalize {}", path.display()),
        source: e,
    })?;
    if !canon.starts_with(root) {
        return Err(ConfigError::Invalid(format!(
            "include path {} escapes config directory {}",
            canon.display(),
            root.display()
        )));
    }
    if !stack.insert(canon.clone()) {
        return Err(ConfigError::Invalid(format!(
            "include cycle detected at {}",
            canon.display()
        )));
    }

    let raw = fs::read_to_string(&canon).map_err(|e| ConfigError::Io {
        context: format!("read {}", canon.display()),
        source: e,
    })?;
    let expanded = substitute(&raw)?;
    let mut value: Value = serde_yaml_ng::from_str(&expanded).map_err(|e| ConfigError::Parse {
        path: canon.display().to_string(),
        source: e,
    })?;

    let dir = canon.parent().map(Path::to_path_buf).unwrap_or_default();
    resolve(&mut value, &dir, root, stack)?;

    stack.remove(&canon);
    Ok(value)
}

fn resolve(value: &mut Value, base_dir: &Path, root: &Path, stack: &mut HashSet<PathBuf>) -> Result<(), ConfigError> {
    // tagged scalars surface via Value::Tagged in serde_yaml_ng
    if let Value::Tagged(tagged) = value
        && tagged.tag == INCLUDE_TAG
    {
        let rel = tagged
            .value
            .as_str()
            .ok_or_else(|| ConfigError::Invalid("!include argument must be a string path".into()))?;
        let target = base_dir.join(rel);
        let target_canon = fs::canonicalize(&target).map_err(|e| ConfigError::Io {
            context: format!("canonicalize {}", target.display()),
            source: e,
        })?;
        if !target_canon.starts_with(root) {
            return Err(ConfigError::Invalid(format!(
                "include path {} escapes config directory {}",
                target.display(),
                root.display()
            )));
        }
        let included = load_inner(&target, root, stack)?;
        *value = included;
        return Ok(());
    }

    match value {
        Value::Sequence(seq) => {
            // a sequence can contain !include items that themselves resolve to
            // sequences; flatten those one level.
            let mut out: Vec<Value> = Vec::with_capacity(seq.len());
            for mut item in std::mem::take(seq) {
                resolve(&mut item, base_dir, root, stack)?;
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
            // a placeholder marker - the design keeps it simple: at a mapping value
            // position, we just substitute the value (no merging). merging
            // is reserved for sequence position (e.g. layers: !include ...).
            for (_, v) in map.iter_mut() {
                resolve(v, base_dir, root, stack)?;
            }
        }
        Value::Tagged(tagged) => {
            resolve(&mut tagged.value, base_dir, root, stack)?;
        }
        _ => {}
    }
    Ok(())
}
