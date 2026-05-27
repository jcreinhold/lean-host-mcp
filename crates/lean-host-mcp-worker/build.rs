//! Bake the Lean toolchain's `lib/lean` directory into the rpath of the
//! worker binary so it can load `libleanshared.{dylib,so}` at run-time.
//!
//! Discovery order:
//!
//! 1. `LEAN_HOST_MCP_TARGET_TOOLCHAIN`: short toolchain id (e.g.
//!    `v4.30.0`). Resolved as
//!    `~/.elan/toolchains/leanprover--lean4---<id>`. Set by
//!    `lean-host-mcp install-worker` when producing a toolchain-specific
//!    worker binary.
//! 2. `LEAN_SYSROOT`: explicit prefix (the matching `lean` toolchain
//!    root, containing `lib/lean/libleanshared`).
//! 3. `lean --print-prefix`: fall back to whatever `lean` resolves to on
//!    `PATH` (the developer's default toolchain).
//!
//! Only this crate needs the dance; the parent (`lean-host-mcp`) does not
//! link `libleanshared` and has no `build.rs`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=LEAN_HOST_MCP_TARGET_TOOLCHAIN");
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
        // Discovery failed; the link step in `lean-rs-sys` will surface
        // the underlying issue with a clearer error than we could here.
        return;
    };
    let lib_lean = prefix.join("lib").join("lean");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_lean.display());
}

fn discover_prefix() -> Option<PathBuf> {
    if let Some(id) = env::var_os("LEAN_HOST_MCP_TARGET_TOOLCHAIN") {
        let id = id.to_string_lossy().into_owned();
        if let Some(home) = env::var_os("HOME") {
            let path = PathBuf::from(home)
                .join(".elan")
                .join("toolchains")
                .join(format!("leanprover--lean4---{id}"));
            if path.is_dir() {
                return Some(path);
            }
        }
    }
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
