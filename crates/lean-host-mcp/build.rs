//! Embed source identity only; the parent must never link the Lean runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

fn watch(path: &Path) {
    let path = path.to_string_lossy();
    if !path.contains(['\n', '\r']) {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn main() {
    let Some(manifest) =
        std::env::var_os("CARGO_MANIFEST_DIR").and_then(|path| PathBuf::from(path).canonicalize().ok())
    else {
        return;
    };
    watch(&manifest.join("build.rs"));
    watch(&manifest.join("src"));
    let Some(root) = git(&manifest, &["rev-parse", "--show-toplevel"]).map(|root| PathBuf::from(root.trim())) else {
        // Source archives and registry packages need not contain Git metadata.
        return;
    };
    let Some(package_manifest) = manifest.strip_prefix(&root).ok().map(|path| path.join("Cargo.toml")) else {
        return;
    };
    let Some(package_manifest) = package_manifest.to_str() else {
        return;
    };
    if git(&root, &["ls-files", "--error-unmatch", "--", package_manifest]).is_none() {
        // Do not mislabel an unpacked package with an unrelated enclosing repo.
        return;
    }
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&root, &["rev-parse", "--git-path", name]) {
            watch(&root.join(path.trim()));
        }
    }
    if let Some(reference) = git(&root, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&root, &["rev-parse", "--git-path", reference.trim()])
    {
        watch(&root.join(path.trim()));
    }
    // Track workspace/dependency edits too, without watching generated target
    // output and rebuilding forever. Dirty means the source tree at build time,
    // not whatever checkout happens to be on disk when the server is queried.
    if let Some(files) = git(&root, &["ls-files", "-z"]) {
        for path in files.split('\0').filter(|path| !path.is_empty()) {
            watch(&root.join(path));
        }
    }
    if let Some(revision) = git(&root, &["rev-parse", "--verify", "HEAD"]) {
        let revision = revision.trim();
        if matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            println!("cargo:rustc-env=LEAN_HOST_MCP_BUILD_REVISION={revision}");
        }
    }
    if let Some(status) = git(&root, &["status", "--porcelain", "--untracked-files=normal"]) {
        println!(
            "cargo:rustc-env=LEAN_HOST_MCP_BUILD_DIRTY={}",
            !status.trim().is_empty()
        );
    }
}
