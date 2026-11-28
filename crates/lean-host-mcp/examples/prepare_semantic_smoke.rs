//! Prepare the exact released semantic-runtime cache for the installed-package smoke.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use lean_host_mcp::build::DEFAULT_TIMEOUT_SECS;
use lean_semantic_search_runtime::SemanticSearchRuntimeBuild;

fn main() -> Result<()> {
    let [cache, label, sysroot] = std::env::args_os()
        .skip(1)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected cache, toolchain label and sysroot"))?;
    let input = SemanticSearchRuntimeBuild {
        cache_root: PathBuf::from(cache).canonicalize()?,
        toolchain_label: label
            .into_string()
            .map_err(|_| anyhow::anyhow!("toolchain label is not UTF-8"))?,
        lean_sysroot: PathBuf::from(sysroot).canonicalize()?,
        // The smoke's fixture has no `[build]` table, so install would pass
        // the default `[build] timeout_secs`.
        timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
    };
    let started = Instant::now();
    lean_semantic_search_runtime::build(input.clone(), || false)
        .context("build the semantic search modules for the installed-package smoke")?;
    // A server only opens what install built; check that this opens too.
    let runtime =
        lean_semantic_search_runtime::open(input).context("open the semantic search modules that were just built")?;
    let lean_path = std::env::join_paths(runtime.lean_path()).context("join the semantic search import path")?;
    println!(
        "semantic search modules built in {:.1}s; LEAN_PATH={}",
        started.elapsed().as_secs_f64(),
        lean_path.to_string_lossy(),
    );
    Ok(())
}
