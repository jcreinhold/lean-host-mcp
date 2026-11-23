//! Derive the conservative source-header closure before trusting compiled inputs.

use std::{
    collections::{BTreeSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

const MAX_MODULES: usize = 4096;
const HEADER_BATCH: usize = 32;

#[derive(Deserialize)]
struct Headers {
    imports: Vec<Header>,
}

#[derive(Deserialize)]
struct Header {
    errors: Vec<String>,
    result: Option<HeaderResult>,
}

#[derive(Deserialize)]
struct HeaderResult {
    imports: Vec<Import>,
}

#[derive(Deserialize)]
struct Import {
    module: String,
}

/// Read only the explicitly selected headers. Installed modules are covered
/// by the approved release identity. Dependency and artifact provenance is
/// checked separately before a compiled module can be used.
pub(super) fn derive(
    roots: &[PathBuf],
    release: &Path,
    domains: &[PathBuf],
    imports: impl IntoIterator<Item = String>,
    read: impl FnMut(&[PathBuf]) -> io::Result<Vec<u8>>,
) -> io::Result<Vec<(String, PathBuf)>> {
    derive_observed(roots, release, domains, imports, read, |_, _| {})
}

pub(super) fn derive_observed(
    roots: &[PathBuf],
    release: &Path,
    domains: &[PathBuf],
    imports: impl IntoIterator<Item = String>,
    mut read: impl FnMut(&[PathBuf]) -> io::Result<Vec<u8>>,
    mut observe: impl FnMut(&str, &[String]),
) -> io::Result<Vec<(String, PathBuf)>> {
    let mut seen = BTreeSet::new();
    let mut pending = VecDeque::new();
    let mut enqueue = |module: String, pending: &mut VecDeque<String>| {
        if !super::prepared_project::valid_module(&module) || module.len() > 256 {
            return Err(io::Error::other("invalid prepared closure module name"));
        }
        if seen.insert(module.clone()) {
            if seen.len() > MAX_MODULES {
                return Err(io::Error::other("prepared import closure exceeds 4096 modules"));
            }
            pending.push_back(module);
        }
        Ok(())
    };
    for module in imports {
        enqueue(module, &mut pending)?;
    }
    let mut selected = Vec::new();
    let source_roots = roots
        .iter()
        .filter_map(|root| {
            domains
                .iter()
                .find(|domain| domain.join(".lake/build/lib/lean") == *root)
        })
        .cloned()
        .collect::<Vec<_>>();

    while !pending.is_empty() {
        let start = selected.len();
        let mut files = Vec::new();
        while files.len() < HEADER_BATCH {
            let Some(name) = pending.pop_front() else {
                break;
            };
            let module_path = name.replace('.', "/");
            let prefix = name
                .split('.')
                .next()
                .ok_or_else(|| io::Error::other("missing module prefix"))?;
            let owner = source_roots
                .iter()
                .find(|root| root.join(prefix).is_dir() || root.join(prefix).with_extension("lean").is_file());
            let installed = release.join(&module_path).with_extension("olean");
            if installed.is_file() {
                if installed.canonicalize()? != installed || owner.is_some() {
                    return Err(io::Error::other("installed import is aliased or shadowed"));
                }
                continue;
            }
            let owner = owner.ok_or_else(|| io::Error::other(format!("unresolved prepared source import: {name}")))?;
            let file = owner.join(&module_path).with_extension("lean");
            if file.canonicalize()? != file || !fs::symlink_metadata(&file)?.is_file() {
                return Err(io::Error::other("prepared source import is not a canonical file"));
            }
            selected.push((name, owner.clone()));
            files.push(file);
        }
        if files.is_empty() {
            continue;
        }
        let bytes = read(&files)?;
        let headers: Headers = serde_json::from_slice(&bytes)?;
        if headers.imports.len() != files.len() {
            return Err(io::Error::other("incomplete compiler import header response"));
        }
        for ((name, _), header) in selected.iter().skip(start).zip(headers.imports) {
            let Some(result) = header.result.filter(|_| header.errors.is_empty()) else {
                return Err(io::Error::other("compiler refused a selected import header"));
            };
            let imports = result.imports.into_iter().map(|entry| entry.module).collect::<Vec<_>>();
            observe(name, &imports);
            for import in imports {
                enqueue(import, &mut pending)?;
            }
        }
    }
    Ok(selected)
}

#[test]
#[ignore = "requires a disposable pinned Mathlib source fixture"]
fn selects_real_mathlib_source_closure() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("TEST_CLOSURE_ROOT").ok_or("missing closure project")?);
    let sysroot = PathBuf::from(std::env::var_os("LEAN_SYSROOT").ok_or("missing selected Lean release")?);
    let domains = lean_toolchain::shared_writer::project_artifact_roots(&root)?;
    let roots = domains
        .iter()
        .map(|p| p.join(".lake/build/lib/lean"))
        .collect::<Vec<_>>();
    let selected = derive(
        &roots,
        &sysroot.join("lib/lean"),
        &domains,
        ["Root".to_owned()],
        |files| {
            let output = std::process::Command::new(sysroot.join("bin/lean"))
                .current_dir(&root)
                .args(["--deps-json"])
                .args(files)
                .env_remove("LEAN_PATH")
                .output()?;
            if !output.status.success() {
                return Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()));
            }
            Ok(output.stdout)
        },
    )?;
    assert_eq!(selected.len(), 172);
    assert_eq!(selected.iter().filter(|(_, owner)| owner == &root).count(), 2);
    assert!(selected.iter().any(|(name, _)| name == "Mathlib.Data.Nat.Basic"));
    Ok(())
}
