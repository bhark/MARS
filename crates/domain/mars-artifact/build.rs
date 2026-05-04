//! build script: writes an empty `generated.rs` until planus codegen lands.
//! when the first `.fbs` schema is added, swap the body for a planus_codegen call.

#![allow(clippy::expect_used)]

use std::{env, fs, path::PathBuf};

fn main() {
    let out_dir = env::var("OUT_DIR").expect("cargo always sets OUT_DIR");
    let schemas_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas");
    println!("cargo:rerun-if-changed={}", schemas_dir.display());

    if let Ok(entries) = fs::read_dir(&schemas_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "fbs") {
                println!("cargo:rerun-if-changed={}", path.display());
                println!(
                    "cargo:warning=mars-artifact: schema {} present but planus codegen not wired",
                    path.display()
                );
            }
        }
    }

    let dest = PathBuf::from(out_dir).join("generated.rs");
    fs::write(&dest, "// auto-generated: empty until planus codegen is wired.\n")
        .expect("write generated.rs");
}
