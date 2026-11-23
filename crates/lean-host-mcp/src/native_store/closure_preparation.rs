//! Fixed cache materialization and first-party build under the committed writer.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use lean_host_mcp_worker::prepared_project::{ImportPlan, PreparedGeneration};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{Receipt, Stage, pinned_lake_config, run_module};
use crate::{
    broker::verify_file::VerifierShims, lake_meta::LakeProjectMeta,
    managed_prepared::readiness_writer::PreparedReadiness,
};

pub(crate) const PLAN_ENV: &str = "LEAN_HOST_MCP_MANAGED_CLOSURE_PLAN";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    project: PathBuf,
    lakefile_sha256: [u8; 32],
    manifest_sha256: [u8; 32],
    adapter: PathBuf,
    adapter_sha256: [u8; 32],
    runtime_roots: Vec<PathBuf>,
    cache_imports: Vec<(String, PathBuf)>,
    build: Vec<String>,
}

fn checked(command: &mut Command) -> io::Result<Output> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "guarded closure command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output)
}

pub(super) fn run(
    root: &Path,
    lake: &Path,
    seed: &str,
    authorize: &impl Fn() -> io::Result<Duration>,
) -> io::Result<Option<PreparedReadiness>> {
    let Some(path) = std::env::var_os(PLAN_ENV).map(PathBuf::from) else {
        return Ok(None);
    };
    let policy: Policy = serde_json::from_slice(&pinned_lake_config::file_bytes(&path)?)?;
    if policy.project != root
        || policy.build.last().is_none_or(|name| name != seed)
        || policy.build.len() > 32
        || policy.build.iter().any(|name| !super::name(name))
        || policy.cache_imports.is_empty()
        || policy.cache_imports.len() > 32
    {
        return Err(io::Error::other("invalid selected closure preparation"));
    }
    authorize()?;
    pinned_lake_config::validate_root_config(root, &policy.lakefile_sha256)?;
    let package_order = pinned_lake_config::checked_package_order(root, &policy.manifest_sha256)?;
    let domains = lean_toolchain::shared_writer::project_artifact_roots(root)?;
    for domain in &domains {
        crate::shared_writer::verify_committed_writer_process(&lean_toolchain::shared_writer::project_lock_path(
            domain,
        )?)?;
    }
    let adapter_source = pinned_lake_config::file_bytes(&policy.adapter)?;
    if <[u8; 32]>::from(Sha256::digest(&adapter_source)) != policy.adapter_sha256 {
        return Err(io::Error::other("closure cache adapter digest changed"));
    }
    for path in &policy.runtime_roots {
        if !path.is_dir() || path.canonicalize()? != *path {
            return Err(io::Error::other("noncanonical cache tool runtime input"));
        }
    }
    let meta = LakeProjectMeta::from_explicit(root).map_err(io::Error::other)?;
    let shims = VerifierShims::select(&meta).map_err(io::Error::other)?;
    if Some(shims.sysroot.as_path()) != lake.parent().and_then(Path::parent) {
        return Err(io::Error::other("prepared worker selected a different Lean release"));
    }
    let _shim_custody = lean_toolchain::shared_writer::LocalArtifactRead::try_acquire(&shims.domains)?;
    shims.validate().map_err(io::Error::other)?;
    let release = shims.sysroot.join("lib/lean");
    let lean = shims.sysroot.join("bin/lean");
    let lean_command = || {
        let mut command = Command::new(&lean);
        command
            .current_dir(root)
            .env("LEAN_SYSROOT", &shims.sysroot)
            .env_remove("LEAN_PATH")
            .env_remove("LEAN_SRC_PATH")
            .env_remove("ELAN_TOOLCHAIN");
        command
    };
    let lake_command = || {
        let mut command = Command::new(lake);
        command
            .current_dir(root)
            .arg("--no-cache")
            .env("LEAN_SYSROOT", &shims.sysroot)
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env_remove("LEAN_PATH")
            .env_remove("LEAN_SRC_PATH");
        command
    };
    authorize()?;
    let search =
        checked(lake_command().args(["--reconfigure", "--no-build", "env", "/usr/bin/printenv", "LEAN_PATH"]))?;
    let search = std::str::from_utf8(&search.stdout).map_err(io::Error::other)?;
    let lake_roots = std::env::split_paths(search.trim())
        .map(|path| if path.is_absolute() { path } else { root.join(path) })
        .filter(|path| path != &release)
        .collect::<Vec<_>>();
    let expected = domains
        .iter()
        .map(|domain| domain.join(".lake/build/lib/lean"))
        .collect::<BTreeSet<_>>();
    if lake_roots.iter().cloned().collect::<BTreeSet<_>>() != expected || lake_roots.len() != expected.len() {
        return Err(io::Error::other("Lake resolved a different artifact domain union"));
    }
    // The installed worker uses the root followed by manifest package order.
    let mut roots = package_order
        .iter()
        .map(|path| path.join(".lake/build/lib/lean"))
        .collect::<Vec<_>>();
    roots.extend(shims.paths.iter().cloned());
    roots.push(release.clone());
    let plan = ImportPlan::new(
        root.to_owned(),
        roots.clone(),
        release.clone(),
        vec![(seed.to_owned(), root.to_owned())],
    );
    let read_headers = |files: &[PathBuf]| {
        authorize()?;
        Ok(checked(lean_command().arg("--deps-json").args(files))?.stdout)
    };
    let closure = plan.derive(&domains, read_headers)?;
    let mut lake_roots = lake_roots;
    lake_roots.extend(shims.paths.iter().cloned());
    lake_roots.push(release.clone());
    let lake_plan = ImportPlan::new(
        root.to_owned(),
        lake_roots,
        release.clone(),
        vec![(seed.to_owned(), root.to_owned())],
    );
    if lake_plan.derive(&domains, read_headers)? != closure {
        return Err(io::Error::other(
            "Lake and the worker resolve different source closures",
        ));
    }
    let first_party = closure
        .iter()
        .filter(|(_, owner)| owner == root)
        .map(|(name, _)| name)
        .collect::<BTreeSet<_>>();
    if first_party != policy.build.iter().collect() || first_party.len() != policy.build.len() {
        return Err(io::Error::other(
            "approved build does not cover the exact first-party closure",
        ));
    }
    let cache_plan = ImportPlan::new(root.to_owned(), roots.clone(), release, policy.cache_imports.clone());
    let cache_closure = cache_plan
        .derive(&domains, read_headers)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let dependencies = closure
        .into_iter()
        .filter(|(_, owner)| owner != root)
        .collect::<BTreeSet<_>>();
    if dependencies != cache_closure {
        return Err(io::Error::other(
            "approved cache does not cover the exact dependency closure",
        ));
    }
    let scratch = tempfile::Builder::new().prefix("closure-cache-").tempdir()?;
    let archives = scratch.path().join("archives");
    fs::create_dir(&archives)?;
    let adapter = scratch.path().join("CacheAdapter.lean");
    fs::write(&adapter, adapter_source)?;
    authorize()?;
    checked(
        lean_command()
            .arg("--run")
            .arg(&adapter)
            .args(policy.cache_imports.iter().map(|(module, _)| module))
            .env(
                "LEAN_PATH",
                std::env::join_paths(&policy.runtime_roots).map_err(io::Error::other)?,
            )
            .env(
                "LEAN_SRC_PATH",
                std::env::join_paths(&domains).map_err(io::Error::other)?,
            )
            .env("MATHLIB_CACHE_DIR", &archives)
            .env_remove("MATHLIB_CACHE_GET_URL")
            .env_remove("MATHLIB_CACHE_BASE_URL"),
    )?;
    for name in policy.build {
        let run = |args: &[&str]| {
            authorize()?;
            let status = lake_command().args(args).stdout(Stdio::null()).status()?;
            status
                .code()
                .ok_or_else(|| io::Error::other("guarded Lake was interrupted"))
        };
        let receipt = run_module(name, &run, &|| authorize().map(|_| ()))?;
        if receipt.dependency_exit_code != 0 || receipt.build_exit_code != Some(0) {
            return Err(io::Error::other("first-party closure build did not complete"));
        }
    }
    for domain in &domains {
        let mut path = domain.clone();
        for component in [".lake", "build", "lib", "lean"] {
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&path)?,
                Ok(_) => return Err(io::Error::other("aliased prepared domain")),
                Err(error) => return Err(error),
            }
        }
    }
    authorize()?;
    let prepared_generation = PreparedGeneration::capture_derived(plan, &domains, read_headers)?.launch_input()?;
    authorize()?;
    Ok(Some(PreparedReadiness {
        readiness: Receipt {
            version: 1,
            stage: Stage::Ready,
            dependency_exit_code: None,
            build_exit_code: None,
            ready_exit_code: Some(0),
            modules: vec![],
            preflight: None,
        },
        prepared_generation,
    }))
}
