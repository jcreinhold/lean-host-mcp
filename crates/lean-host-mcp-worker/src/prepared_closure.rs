//! Derive the conservative source-header closure before trusting compiled inputs.

use std::{
    collections::{BTreeSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::module_preparation::MAX_PROJECT_MODULES;
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
            if seen.len() > MAX_PROJECT_MODULES {
                return Err(io::Error::other(format!(
                    "prepared import closure exceeds {MAX_PROJECT_MODULES} modules"
                )));
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

#[cfg(test)]
mod bounds {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_module_ceiling_allows_large_batched_closures_and_refuses_overflow() -> io::Result<()> {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                drop(fs::remove_dir_all(&self.0));
            }
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lean-closure-{}-{unique}", std::process::id()));
        fs::create_dir(&path)?;
        let scratch = Scratch(path);
        let project = scratch.0.canonicalize()?;
        let sources = project.join("Root");
        let release = project.join("release");
        fs::create_dir(&sources)?;
        fs::create_dir(&release)?;
        fs::write(project.join("Root.lean"), "")?;
        let roots = [project.join(".lake/build/lib/lean")];
        let domains = [project.clone()];
        let walk = |total: usize, create_sources: bool| {
            if create_sources {
                for number in 1..total {
                    fs::write(sources.join(format!("M{number}.lean")), "")?;
                }
            }
            derive(&roots, &release, &domains, ["Root".to_owned()], |files| {
                assert!(files.len() <= HEADER_BATCH);
                let imports = files
                    .iter()
                    .map(|file| {
                        let imports = if *file == project.join("Root.lean") {
                            (1..total).map(|n| json!({"module":format!("Root.M{n}")})).collect()
                        } else {
                            Vec::new()
                        };
                        json!({"errors": [], "result": {"imports": imports}})
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::to_vec(&json!({"imports": imports}))?)
            })
        };
        let allowed = walk(4_653, true)?;
        assert_eq!(allowed.len(), 4_653);
        let refused = walk(MAX_PROJECT_MODULES + 1, false).unwrap_err();
        assert_eq!(
            refused.to_string(),
            format!("prepared import closure exceeds {MAX_PROJECT_MODULES} modules")
        );
        Ok(())
    }
}
