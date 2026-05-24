//! Bake the Lean toolchain's `lib/lean` directory into the rpath of any
//! binary this crate produces (the MCP server itself plus tests and
//! examples), so they can load `libleanshared.{dylib,so}` at run-time.
//!
//! `lean-rs-sys`'s build script already emits this rpath for *its own*
//! binaries; `cargo:rustc-link-arg` doesn't propagate to dependents, so
//! every crate that ships its own executable that loads Lean has to
//! repeat the dance. The recipe below is the one from
//! `lean-rs/crates/lean-rs/build.rs`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/bin/worker.rs");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=LEAN_SYSROOT");
    println!("cargo:rerun-if-env-changed=PATH");

    if env::var_os("DOCS_RS").is_some() {
        return;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !matches!(target_os.as_str(), "macos" | "linux") {
        return;
    }

    let Some(prefix) = discover_prefix() else {
        // Discovery failed — the link step in `lean-rs-sys` will surface
        // the underlying issue with a clearer error than we could here.
        return;
    };
    let lib_lean = prefix.join("lib").join("lean");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_lean.display());
}

fn discover_prefix() -> Option<PathBuf> {
    if let Some(p) = env::var_os("LEAN_SYSROOT") {
        return Some(PathBuf::from(p));
    }
    let output = Command::new("lean").arg("--print-prefix").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}
