//! Bake the Lean toolchain's `lib/lean` directory into the rpath of the
//! worker binary so it can load `libleanshared.{dylib,so}` at run-time.
//!
//! Discovery order:
//!
//! 1. `LEAN_SYSROOT`: the explicit Lean prefix used for both headers and
//!    runtime linking. The installer sets it to the selected elan toolchain.
//! 2. `LEAN_HOST_MCP_TARGET_TOOLCHAIN`: short toolchain ID resolved under
//!    `$ELAN_HOME/toolchains`, else `$HOME/.elan/toolchains`, for builds
//!    without an explicit prefix.
//! 3. `lean --print-prefix`: fall back to whatever `lean` resolves to on
//!    `PATH` (the developer's default toolchain).
//!
//! Only the worker emits Lean runtime rpath/linker directives. The parent
//! (`lean-host-mcp`) does not link `libleanshared`; its `build.rs` embeds Git
//! source identity only.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), String> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=LEAN_HOST_MCP_TARGET_TOOLCHAIN");
    println!("cargo:rerun-if-env-changed=LEAN_SYSROOT");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=ELAN_HOME");
    println!("cargo:rerun-if-env-changed=HOME");

    if env::var_os("CARGO_FEATURE_NATIVE_WORKER").is_none() || env::var_os("DOCS_RS").is_some() {
        return Ok(());
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !matches!(target_os.as_str(), "macos" | "linux") {
        return Ok(());
    }

    let prefix = match env::var_os("LEAN_SYSROOT") {
        Some(value) => Some(explicit_prefix(PathBuf::from(value), &target_os)?),
        None => discover_prefix(),
    };
    let Some(prefix) = prefix else {
        // Discovery failed; the link step in `lean-rs-sys` will surface
        // the underlying issue with a clearer error than we could here.
        return Ok(());
    };
    println!(
        "cargo:rustc-env=LEAN_HOST_COMPILED_SYSROOT={}",
        prefix.canonicalize().map_err(|e| e.to_string())?.display()
    );
    let lib_lean = prefix.join("lib").join("lean");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_lean.display());
    Ok(())
}

fn explicit_prefix(prefix: PathBuf, target_os: &str) -> Result<PathBuf, String> {
    let shared = if target_os == "macos" {
        "libleanshared.dylib"
    } else {
        "libleanshared.so"
    };
    if !prefix.is_absolute()
        || !prefix.join("include/lean/lean.h").is_file()
        || !prefix.join("lib/lean").join(shared).is_file()
    {
        return Err(format!(
            "LEAN_SYSROOT does not name an absolute complete Lean prefix: {}",
            prefix.display()
        ));
    }
    Ok(prefix)
}

fn discover_prefix() -> Option<PathBuf> {
    if let Some(id) = env::var_os("LEAN_HOST_MCP_TARGET_TOOLCHAIN") {
        let id = id.to_string_lossy().into_owned();
        let elan = env::var_os("ELAN_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".elan")));
        if let Some(elan) = elan {
            let path = elan.join("toolchains").join(format!("leanprover--lean4---{id}"));
            if path.is_dir() {
                return Some(path);
            }
        }
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
