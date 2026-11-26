//! The build path leaves the import graph to Lake.
//!
//! `lean_build` hands Lake its targets and steps and `lean_imports` reads
//! what `lake setup-file` reports. Neither may walk Lake's import graph
//! or order modules itself: Lake owns that work. This test fails when a
//! file on that path names one of Lake's graph queries or a sorter.

use std::fs;
use std::path::{Path, PathBuf};

/// Words that mark an import graph walk or a build order of our own.
const FORBIDDEN: [&str; 3] = ["TopologicalSorter", ":imports", "--deps-json"];

/// The build path: the build module and its config, the project lock, the
/// tool handlers and the setup-file reader. A directory stands for every
/// file in it.
const BUILD_PATH: [&str; 6] = [
    "src/build.rs",
    "src/build",
    "src/project_lock.rs",
    "src/project_lock",
    "src/tools/build.rs",
    "src/broker/module_setup.rs",
];

#[test]
fn build_path_never_queries_the_import_graph() -> Result<(), Box<dyn std::error::Error>> {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for entry in BUILD_PATH {
        let path = crate_root.join(entry);
        if path.is_dir() {
            files.extend(rust_files(&path)?);
        } else {
            files.push(path);
        }
    }
    let mut found = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).map_err(|error| format!("{}: {error}", file.display()))?;
        for (number, line) in text.lines().enumerate() {
            for word in FORBIDDEN.iter().filter(|word| line.contains(**word)) {
                found.push(format!("{}:{}: {word}", file.display(), number + 1));
            }
        }
    }
    if found.is_empty() {
        Ok(())
    } else {
        Err(format!("the build path queries Lake's import graph:\n{}", found.join("\n")).into())
    }
}

/// Every `.rs` file under `dir`, sorted.
fn rust_files(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(rust_files(&path)?);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}
