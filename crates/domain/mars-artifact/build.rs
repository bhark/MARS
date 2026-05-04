//! build script: compiles `schemas/footer.fbs` into `$OUT_DIR/generated.rs`
//! using planus-translation + planus-codegen (the public planus pipeline).

#![allow(clippy::expect_used)]

use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let schema = manifest_dir.join("schemas/footer.fbs");
    println!("cargo:rerun-if-changed={}", schema.display());

    let declarations = planus_translation::translate_files(&[&schema]).expect("translate footer.fbs");
    let generated = planus_codegen::generate_rust(&declarations, true).expect("planus rust codegen");

    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR");
    let dest = PathBuf::from(out_dir).join("generated.rs");
    fs::write(&dest, generated).expect("write generated.rs");
}
