//! Prepare the exact released semantic-runtime cache for the installed-package smoke.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use lean_semantic_search_runtime::{SemanticSearchRuntimeBuild, build_cached_with_writer};

fn main() -> Result<()> {
    let [cache, label, sysroot, writer] = std::env::args_os()
        .skip(1)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected cache, toolchain label, sysroot and packaged host"))?;
    let started = Instant::now();
    let runtime = build_cached_with_writer(
        SemanticSearchRuntimeBuild {
            cache_root: PathBuf::from(cache).canonicalize()?,
            toolchain_label: label
                .into_string()
                .map_err(|_| anyhow::anyhow!("toolchain label is not UTF-8"))?,
            lean_sysroot: PathBuf::from(sysroot).canonicalize()?,
        },
        &PathBuf::from(writer).canonicalize()?,
        Duration::from_secs(120),
        || false,
    )
    .context("prepare installed semantic smoke capability")?;
    println!(
        "semantic smoke capability prepared in {:.1}s at {}",
        started.elapsed().as_secs_f64(),
        runtime.built.dylib_path()?.display(),
    );
    Ok(())
}
