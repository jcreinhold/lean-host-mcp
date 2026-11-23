//! Build only admitted root-package module IR facets after each batch's no-build guards.
use std::{
    collections::{BTreeMap, HashMap},
    io::{self, BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Plan, Target, blueprint_declarations, module_name, pinned_lake_config};

pub(super) const MAX_MODULES: usize = 16_384;
const BATCH: usize = 16;
const MAX_QUERY_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u32,
    stage: Stage,
    name: String,
    module_count: usize,
    completed_modules: usize,
    completed_batches: usize,
    first_failure: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blueprint: Option<blueprint_declarations::Receipt>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    RootLibrary,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Configuration,
    BlueprintInputs,
    Inventory,
    InventoryMismatch,
    Imports,
    Dependencies,
    Setup,
    Build,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Failure {
    phase: Phase,
    exit_code: Option<i32>,
}

impl Receipt {
    pub(crate) fn parse(output: &[u8], name: &str, expected_count: usize, expect_blueprint: bool) -> io::Result<Self> {
        let receipt: Self = serde_json::from_slice(output)?;
        if receipt.version != 1
            || receipt.name != name
            || receipt.module_count > MAX_MODULES
            || receipt.completed_modules > receipt.module_count
            || receipt.completed_batches > receipt.completed_modules
            || receipt.blueprint.is_some() != (expect_blueprint && receipt.first_failure.is_none())
            || receipt.blueprint.as_ref().is_some_and(|outcome| !outcome.valid())
            || match &receipt.first_failure {
                Some(Failure {
                    phase: Phase::Configuration | Phase::InventoryMismatch | Phase::BlueprintInputs,
                    exit_code,
                }) => exit_code.is_some() || receipt.completed_modules != 0,
                Some(Failure {
                    phase: Phase::Inventory,
                    exit_code,
                }) => exit_code.is_none_or(|code| code <= 0) || receipt.module_count != 0,
                Some(failure) => {
                    failure.exit_code.is_none_or(|code| code <= 0) || receipt.module_count != expected_count
                }
                None => receipt.completed_modules != expected_count || receipt.module_count != expected_count,
            }
        {
            return Err(invalid_inventory());
        }
        Ok(receipt)
    }
}

fn invalid_inventory() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or oversized Lake root-library inventory",
    )
}

fn query(mut command: Command, targets: &[String]) -> io::Result<Result<Vec<Vec<String>>, i32>> {
    let mut child = command
        .args(["--no-build", "query", "--json"])
        .args(targets)
        .stdout(Stdio::piped())
        .spawn()?;
    let rows = (|| {
        let stdout = child.stdout.take().ok_or_else(invalid_inventory)?;
        let mut reader = BufReader::new(stdout.take(MAX_QUERY_BYTES + 1));
        let mut rows = Vec::with_capacity(targets.len());
        let mut bytes = 0_u64;
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? > 0 {
            bytes = bytes.checked_add(line.len() as u64).ok_or_else(invalid_inventory)?;
            if bytes > MAX_QUERY_BYTES || rows.len() == targets.len() {
                return Err(invalid_inventory());
            }
            rows.push(serde_json::from_slice(&line)?);
            line.clear();
        }
        Ok(rows)
    })();
    if rows.is_err() {
        drop(child.kill());
    }
    let code = child.wait()?.code().ok_or_else(invalid_inventory)?;
    let rows = rows?;
    if code != 0 {
        return Ok(Err(code));
    }
    if rows.len() != targets.len() {
        return Err(invalid_inventory());
    }
    Ok(Ok(rows))
}

pub(super) fn run(
    plan: &Plan,
    lake: &Path,
    command: impl Fn() -> Command,
    authorize: &impl Fn() -> io::Result<Duration>,
    after_deps: &impl Fn() -> io::Result<()>,
) -> io::Result<Receipt> {
    let Target::RootLibrary {
        name,
        module_count,
        module_names_sha256,
        lakefile_sha256,
        manifest_sha256,
        blueprint,
    } = &plan.target
    else {
        return Err(invalid_inventory());
    };
    let mut receipt = Receipt {
        version: 1,
        stage: Stage::RootLibrary,
        name: name.clone(),
        module_count: 0,
        completed_modules: 0,
        completed_batches: 0,
        first_failure: None,
        blueprint: None,
    };
    authorize()?;
    if pinned_lake_config::validate_root_config(&plan.root, lakefile_sha256)
        .and_then(|()| pinned_lake_config::validate_pinned_lake_configs(&plan.root, manifest_sha256))
        .is_err()
    {
        receipt.first_failure = Some(Failure {
            phase: Phase::Configuration,
            exit_code: None,
        });
        return Ok(receipt);
    }
    authorize()?;
    if blueprint
        .as_ref()
        .is_some_and(|inputs| inputs.check(&plan.root).is_err())
    {
        receipt.first_failure = Some(Failure {
            phase: Phase::BlueprintInputs,
            exit_code: None,
        });
        return Ok(receipt);
    }
    authorize()?;
    let mut configure = command();
    configure.arg("--reconfigure");
    let inventory = query(configure, &[format!("@/{name}:modules")])?;
    let names = match inventory {
        Ok(mut rows) => rows.pop().ok_or_else(invalid_inventory)?,
        Err(code) => {
            receipt.first_failure = Some(Failure {
                phase: Phase::Inventory,
                exit_code: Some(code),
            });
            return Ok(receipt);
        }
    };
    if names.len() > MAX_MODULES || !names.iter().all(|name| module_name(name)) {
        return Err(invalid_inventory());
    }
    receipt.module_count = names.len();
    let positions: HashMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    if positions.len() != names.len() {
        return Err(invalid_inventory());
    }
    let mut canonical_names: Vec<_> = positions.keys().copied().collect();
    canonical_names.sort_unstable();
    let mut digest = Sha256::new();
    for module in canonical_names {
        digest.update(module.as_bytes());
        digest.update([0]);
    }
    let actual_sha256: [u8; 32] = digest.finalize().into();
    if names.len() != *module_count || &actual_sha256 != module_names_sha256 {
        receipt.first_failure = Some(Failure {
            phase: Phase::InventoryMismatch,
            exit_code: None,
        });
        return Ok(receipt);
    }
    let mut levels: Vec<usize> = Vec::with_capacity(names.len());
    let mut batches = BTreeMap::<usize, Vec<&str>>::new();
    for modules in names.chunks(BATCH) {
        authorize()?;
        // @/+ requires the module to resolve in the root package, with no workspace fallback.
        let targets: Vec<_> = modules.iter().map(|name| format!("@/+{name}:imports")).collect();
        let imports = match query(command(), &targets)? {
            Ok(rows) => rows,
            Err(code) => {
                receipt.first_failure = Some(Failure {
                    phase: Phase::Imports,
                    exit_code: Some(code),
                });
                return Ok(receipt);
            }
        };
        for (name, imports) in modules.iter().zip(imports) {
            if !imports.iter().all(|name| module_name(name)) {
                return Err(invalid_inventory());
            }
            let mut level = 0;
            for dependency in imports {
                if let Some(index) = positions.get(dependency.as_str()) {
                    let prior = levels.get(*index).ok_or_else(invalid_inventory)?;
                    level = level.max(prior.checked_add(1).ok_or_else(invalid_inventory)?);
                }
            }
            levels.push(level);
            batches.entry(level).or_default().push(name);
        }
    }
    for modules in batches.values().flat_map(|level| level.chunks(BATCH)) {
        for (phase, flags, facet) in [
            (Phase::Dependencies, ["--no-build", "build"], "deps"),
            (Phase::Setup, ["--no-build", "query"], "setup"),
            (Phase::Build, ["--log-level=warning", "build"], "irArts"),
        ] {
            if matches!(phase, Phase::Build) {
                after_deps()?;
            }
            authorize()?;
            let status = command()
                .args(flags)
                .args(modules.iter().map(|name| format!("@/+{name}:{facet}")))
                .status()?;
            let code = status
                .code()
                .ok_or_else(|| io::Error::other("fixed Lake batch was interrupted"))?;
            if code != 0 {
                receipt.first_failure = Some(Failure {
                    phase,
                    exit_code: Some(code),
                });
                return Ok(receipt);
            }
        }
        receipt.completed_modules = receipt
            .completed_modules
            .checked_add(modules.len())
            .ok_or_else(invalid_inventory)?;
        receipt.completed_batches = receipt.completed_batches.checked_add(1).ok_or_else(invalid_inventory)?;
    }
    if let Some(inputs) = blueprint {
        authorize()?;
        receipt.blueprint = Some(inputs.run(&plan.root, lake, command())?);
    }
    Ok(receipt)
}
