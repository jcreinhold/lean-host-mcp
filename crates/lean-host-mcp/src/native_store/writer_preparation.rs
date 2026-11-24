//! Fixed, writer-owned sequencer. The original helper owns all descendants.
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};

use crate::{
    native_store::store::AdmissionDeadline,
    toolchain::{ToolchainId, WindowVerdict},
};

pub(crate) const OPERATION: &str = "lake_module_preparation_v1";
pub(crate) const VERIFIER_OPERATION: &str = "lake_verifier_preparation_v1";
pub(crate) const GROUPED_VERIFIER_OPERATION: &str = "lake_verifier_grouped_preparation_v1";
const MAX_MODULES: usize = 64;
const MAX_PLAN_BYTES: usize = 32 * 1024;

#[path = "closure_preparation.rs"]
mod closure_preparation;
pub(crate) use closure_preparation::{PLAN_ENV, configured_for as closure_preparation_configured};

#[path = "blueprint_declarations.rs"]
mod blueprint_declarations;
#[path = "pinned_lake_config.rs"]
mod pinned_lake_config;
#[path = "regroup.rs"]
pub(super) mod regroup;
#[path = "root_library.rs"]
pub(super) mod root_library;
pub use pinned_lake_config::validate_pinned_lake_configs;
#[path = "first_party_clean.rs"]
pub(crate) mod first_party_clean;
#[path = "tooling_suites.rs"]
pub(crate) mod tooling_suites;
#[path = "whole_lint.rs"]
pub(crate) mod whole_lint;

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Target {
    Module {
        name: String,
    },
    RootLibrary {
        name: String,
        module_count: usize,
        module_names_sha256: [u8; 32],
        sources_sha256: [u8; 32],
        #[serde(default)]
        blueprint: Option<blueprint_declarations::Inputs>,
        lakefile_sha256: [u8; 32],
        manifest_sha256: [u8; 32],
    },
    Modules {
        names: Vec<String>,
    },
    VerifierModules {
        names: Vec<String>,
    },
    VerifierGroups {
        groups: Vec<Vec<String>>,
    },
    Ready {
        name: String,
        package: Option<String>,
    },
    Regroup(regroup::Request),
    ToolingSuites {
        lakefile_sha256: [u8; 32],
        manifest_sha256: [u8; 32],
    },
    WholeLint {
        lakefile_sha256: [u8; 32],
    },
    FirstPartyClean {
        lakefile_sha256: [u8; 32],
    },
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '\'' || c == '-')
        })
}

fn module_name(target: &str) -> bool {
    name(target) && !target.contains('-')
}

impl Target {
    pub(crate) fn validate(&self) -> io::Result<()> {
        let valid = match self {
            Self::Module { name: target } => module_name(target),
            Self::RootLibrary {
                name,
                module_count,
                blueprint,
                ..
            } => {
                module_name(name)
                    && *module_count <= root_library::MAX_MODULES
                    && (blueprint.is_none() || name == "Proofs")
            }
            Self::Modules { names } | Self::VerifierModules { names } => {
                let mut unique = std::collections::BTreeSet::new();
                !names.is_empty()
                    && names.len() <= MAX_MODULES
                    && names.iter().all(|target| module_name(target) && unique.insert(target))
            }
            Self::VerifierGroups { groups } => {
                let mut unique = HashSet::new();
                !groups.is_empty()
                    && groups.len() <= MAX_MODULES
                    && groups.iter().all(|group| {
                        !group.is_empty()
                            && group.len() <= root_library::BATCH
                            && group.iter().all(|module| module_name(module) && unique.insert(module))
                    })
                    && unique.len() <= MAX_MODULES
            }
            Self::Regroup(request) => request.valid(),
            Self::ToolingSuites { .. } | Self::WholeLint { .. } | Self::FirstPartyClean { .. } => true,
            Self::Ready { name: target, package } => name(target) && package.as_deref().is_none_or(name),
        };
        if !valid {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid fixed Lake target"));
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub(crate) root: PathBuf,
    pub(crate) target: Target,
    pub(crate) boot: [u8; 16],
    pub(crate) expires_ns: u64,
}

impl Plan {
    pub(crate) fn encode(&self) -> io::Result<String> {
        let encoded = serde_json::to_string(self)?;
        if encoded.len() > MAX_PLAN_BYTES {
            return Err(io::Error::other("fixed Lake plan exceeds limit"));
        }
        Ok(encoded)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u32,
    stage: Stage,
    dependency_exit_code: Option<i32>,
    build_exit_code: Option<i32>,
    ready_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    modules: Vec<ModuleReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preflight: Option<Preflight>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Preflight {
    mathlib_exit_code: i32,
    linter_exit_code: Option<i32>,
    linter_executable_ready: bool,
}

impl Preflight {
    fn ready(&self) -> bool {
        self.mathlib_exit_code == 0 && self.linter_exit_code == Some(0) && self.linter_executable_ready
    }

    fn valid(&self) -> bool {
        (self.mathlib_exit_code == 0) == self.linter_exit_code.is_some()
            && (!self.linter_executable_ready || self.linter_exit_code == Some(0))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Module,
    Modules,
    VerifierModules,
    Ready,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModuleReceipt {
    name: String,
    dependency_exit_code: i32,
    build_exit_code: Option<i32>,
}

impl ModuleReceipt {
    fn succeeded(&self) -> bool {
        self.dependency_exit_code == 0 && self.build_exit_code == Some(0)
    }

    fn valid(&self) -> bool {
        (self.dependency_exit_code == 0) == self.build_exit_code.is_some()
    }
}

impl Receipt {
    pub(super) fn target_ready(&self) -> bool {
        self.ready_exit_code == Some(0)
    }

    pub(crate) fn parse(output: &[u8], target: &Target) -> io::Result<Self> {
        let receipt: Self = serde_json::from_slice(output)?;
        let valid = match (&receipt.stage, target) {
            (Stage::Module, Target::Module { .. }) => {
                receipt.ready_exit_code.is_none()
                    && receipt.modules.is_empty()
                    && receipt.dependency_exit_code.is_some()
                    && (receipt.dependency_exit_code == Some(0)) == receipt.build_exit_code.is_some()
            }
            (Stage::Modules, Target::Modules { names }) => {
                receipt.dependency_exit_code.is_none()
                    && receipt.build_exit_code.is_none()
                    && receipt.ready_exit_code.is_none()
                    && valid_modules_receipt(&receipt.modules, names)
            }
            (Stage::VerifierModules, Target::VerifierModules { names }) => {
                receipt.dependency_exit_code.is_none()
                    && receipt.build_exit_code.is_none()
                    && receipt.ready_exit_code.is_none()
                    && receipt.preflight.as_ref().is_some_and(|check| {
                        check.valid()
                            && if check.ready() {
                                valid_modules_receipt(&receipt.modules, names)
                            } else {
                                receipt.modules.is_empty()
                            }
                    })
            }
            (Stage::Ready, Target::Ready { .. }) => {
                receipt.ready_exit_code.is_some()
                    && receipt.modules.is_empty()
                    && receipt.dependency_exit_code.is_none()
                    && receipt.build_exit_code.is_none()
            }
            _ => false,
        };
        if receipt.version != 1
            || !valid
            || receipt.preflight.is_some() != matches!(target, Target::VerifierModules { .. })
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid fixed Lake result"));
        }
        Ok(receipt)
    }
}

fn valid_modules_receipt(modules: &[ModuleReceipt], names: &[String]) -> bool {
    modules.split_last().is_some_and(|(last, earlier)| {
        modules.len() <= names.len()
            && modules.iter().zip(names).all(|(stage, name)| stage.name == *name)
            && earlier.iter().all(ModuleReceipt::succeeded)
            && last.valid()
            && (!last.succeeded() || modules.len() == names.len())
    })
}

fn verifier_preflight(root: &Path, watched: &impl Fn(&[&str]) -> io::Result<i32>) -> io::Result<Preflight> {
    let mathlib_exit_code = watched(&["--no-build", "build", "@mathlib/+Mathlib:olean"])?;
    let mut report = Preflight {
        mathlib_exit_code,
        linter_exit_code: None,
        linter_executable_ready: false,
    };
    if mathlib_exit_code == 0 {
        let exit = watched(&["--no-build", "build", "@/runLinter"])?;
        report.linter_exit_code = Some(exit);
        if exit == 0 {
            use std::os::unix::fs::PermissionsExt as _;
            let executable = root.join(crate::lake_meta::RUN_LINTER_REL);
            report.linter_executable_ready = executable.canonicalize().is_ok_and(|path| path == executable)
                && executable
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
        }
    }
    Ok(report)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupedVerifierReceipt {
    version: u32,
    stage: GroupedVerifierStage,
    preflight: Preflight,
    completed_groups: usize,
    first_failure: Option<GroupFailure>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum GroupedVerifierStage {
    VerifierGroups,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GroupFailure {
    group_index: Option<usize>,
    phase: root_library::Phase,
    exit_code: Option<i32>,
}

impl GroupedVerifierReceipt {
    pub(super) fn parse(bytes: &[u8], expected_groups: usize) -> io::Result<Self> {
        use root_library::Phase;

        let receipt: Self = serde_json::from_slice(bytes)?;
        let valid = if !receipt.preflight.ready() {
            receipt.completed_groups == 0 && receipt.first_failure.is_none()
        } else if let Some(first) = &receipt.first_failure {
            match first.phase {
                Phase::Imports => {
                    receipt.completed_groups == 0
                        && match first.group_index {
                            None => first.exit_code.is_some_and(|code| code != 0),
                            Some(index) => index < expected_groups && first.exit_code.is_none(),
                        }
                }
                Phase::Dependencies | Phase::Setup | Phase::Build => {
                    receipt.completed_groups < expected_groups
                        && first.group_index == Some(receipt.completed_groups)
                        && first.exit_code.is_some_and(|code| code != 0)
                }
                Phase::Configuration
                | Phase::BlueprintInputs
                | Phase::Inventory
                | Phase::InventoryMismatch
                | Phase::Sources => false,
            }
        } else {
            receipt.completed_groups == expected_groups
        };
        if receipt.version != 1 || !receipt.preflight.valid() || !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid grouped verifier receipt",
            ));
        }
        Ok(receipt)
    }
}

fn grouped_verifier(
    groups: &[Vec<String>],
    preflight: Preflight,
    command: &impl Fn() -> Command,
    authorize: &impl Fn() -> io::Result<std::time::Duration>,
    after_deps: &impl Fn() -> io::Result<()>,
) -> io::Result<GroupedVerifierReceipt> {
    use root_library::Phase;

    let mut receipt = GroupedVerifierReceipt {
        version: 1,
        stage: GroupedVerifierStage::VerifierGroups,
        preflight,
        completed_groups: 0,
        first_failure: None,
    };
    if !receipt.preflight.ready() {
        return Ok(receipt);
    }

    // Check the admitted grouping against Lake's actual root-package imports.
    // This one query cannot build dependencies or load elaborated module code.
    let by_name: HashMap<_, _> = groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| group.iter().map(move |name| (name.as_str(), index)))
        .collect();
    let targets: Vec<_> = groups
        .iter()
        .flatten()
        .map(|name| format!("@/+{name}:imports"))
        .collect();
    authorize()?;
    let imports = match root_library::query::<Vec<String>>(command(), &targets)? {
        Ok(imports) => imports,
        Err(code) => {
            receipt.first_failure = Some(GroupFailure {
                group_index: None,
                phase: Phase::Imports,
                exit_code: Some(code),
            });
            return Ok(receipt);
        }
    };
    let group_indices = groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| group.iter().map(move |_| index));
    for (index, imports) in group_indices.zip(imports) {
        if imports
            .iter()
            .any(|name| !module_name(name) || by_name.get(name.as_str()).is_some_and(|&dep| dep >= index))
        {
            receipt.first_failure = Some(GroupFailure {
                group_index: Some(index),
                phase: Phase::Imports,
                exit_code: None,
            });
            return Ok(receipt);
        }
    }

    for (index, group) in groups.iter().enumerate() {
        let names: Vec<_> = group.iter().map(String::as_str).collect();
        if let Some(failure) = root_library::run_ir_batch(&names, command, authorize, after_deps)? {
            receipt.first_failure = Some(GroupFailure {
                group_index: Some(index),
                phase: failure.phase,
                exit_code: failure.exit_code,
            });
            break;
        }
        receipt.completed_groups = index.saturating_add(1);
    }
    Ok(receipt)
}

fn run_module(
    name: String,
    watched: &impl Fn(&[&str]) -> io::Result<i32>,
    after_deps: &impl Fn() -> io::Result<()>,
) -> io::Result<ModuleReceipt> {
    let (dependency_exit_code, build_exit_code) =
        lean_host_mcp_worker::module_preparation::run_module(&name, watched, after_deps)?;
    Ok(ModuleReceipt {
        name,
        dependency_exit_code,
        build_exit_code,
    })
}

fn run_modules(
    names: Vec<String>,
    watched: &impl Fn(&[&str]) -> io::Result<i32>,
    after_deps: &impl Fn() -> io::Result<()>,
) -> io::Result<Vec<ModuleReceipt>> {
    let mut modules = Vec::with_capacity(names.len());
    for name in names {
        let outcome = run_module(name, watched, after_deps)?;
        let succeeded = outcome.succeeded();
        modules.push(outcome);
        if !succeeded {
            break;
        }
    }
    Ok(modules)
}

pub(crate) fn lake_executable(root: &Path) -> io::Result<PathBuf> {
    let toolchain = ToolchainId::from_lake_root(root).map_err(io::Error::other)?;
    if toolchain.window_verdict() != WindowVerdict::Supported {
        return Err(io::Error::other("unsupported writer toolchain"));
    }
    let lake = toolchain
        .elan_dir()
        .map_err(io::Error::other)?
        .canonicalize()?
        .join("bin/lake");
    if !lake.is_file() || lake.canonicalize()? != lake {
        return Err(io::Error::other("Lake executable is not canonical"));
    }
    Ok(lake)
}

/// Run one private fixed plan from the exact committed writer process group.
///
/// # Errors
/// Refuses absent custody, invalid target selection, elapsed budget, or subprocess failure.
pub fn run(encoded: &str) -> io::Result<()> {
    if encoded.len() > MAX_PLAN_BYTES {
        return Err(io::Error::other("fixed Lake plan exceeds limit"));
    }
    let plan: Plan = serde_json::from_str(encoded)?;
    plan.target.validate()?;
    if plan.root.canonicalize()? != plan.root || std::env::current_dir()? != plan.root {
        return Err(io::Error::other("fixed Lake plan root differs from writer project"));
    }
    let lock = lean_toolchain::shared_writer::project_lock_path(&plan.root)?;
    let deadline = AdmissionDeadline {
        boot: plan.boot,
        expires_ns: plan.expires_ns,
    };
    let authorize = || {
        crate::shared_writer::verify_committed_writer_process(&lock)?;
        deadline.remaining().map_err(io::Error::other)
    };
    authorize()?;
    if let Target::FirstPartyClean { lakefile_sha256 } = &plan.target {
        return first_party_clean::run(&plan.root, lakefile_sha256, authorize);
    }
    if let Target::ToolingSuites {
        lakefile_sha256,
        manifest_sha256,
    } = &plan.target
    {
        let receipt = tooling_suites::run(&plan.root, lakefile_sha256, manifest_sha256, authorize)?;
        return serde_json::to_writer(std::io::stdout().lock(), &receipt).map_err(io::Error::other);
    }
    let lake = lake_executable(&plan.root)?;
    if let Target::Ready { name, package: None } = &plan.target
        && let Some(receipt) = closure_preparation::run(&plan.root, &lake, name, &authorize)?
    {
        return serde_json::to_writer(io::stdout().lock(), &receipt).map_err(io::Error::other);
    }
    if let Target::WholeLint { lakefile_sha256 } = &plan.target {
        return whole_lint::run(&plan.root, &lake, lakefile_sha256, authorize);
    }
    let command = || {
        let mut command = Command::new(&lake);
        command
            .current_dir(&plan.root)
            .arg("--no-cache")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    };
    let watched = |args: &[&str]| -> io::Result<i32> {
        authorize()?;
        command()
            .args(args)
            .status()?
            .code()
            .ok_or_else(|| io::Error::other("fixed Lake command was interrupted"))
    };

    let after_deps = || test_cut_after_dependencies(&authorize);
    if let Target::RootLibrary { .. } = &plan.target {
        let receipt = root_library::run(&plan, &lake, command, &authorize, &after_deps, |_, _| Ok(()))?;
        return serde_json::to_writer(io::stdout().lock(), &receipt).map_err(io::Error::other);
    }
    if let Target::Regroup(request) = &plan.target {
        let receipt = request.run(&plan, &lake, command, &watched, &authorize, &after_deps)?;
        return serde_json::to_writer(io::stdout().lock(), &receipt).map_err(io::Error::other);
    }
    if let Target::VerifierGroups { groups } = &plan.target {
        let preflight = verifier_preflight(&plan.root, &watched)?;
        let receipt = grouped_verifier(groups, preflight, &command, &authorize, &after_deps)?;
        return serde_json::to_writer(io::stdout().lock(), &receipt).map_err(io::Error::other);
    }
    let receipt = match plan.target {
        Target::RootLibrary { .. }
        | Target::VerifierGroups { .. }
        | Target::Regroup(_)
        | Target::ToolingSuites { .. }
        | Target::WholeLint { .. }
        | Target::FirstPartyClean { .. } => {
            return Err(io::Error::other("fixed writer command already dispatched"));
        }
        Target::Module { name } => {
            let outcome = run_module(name, &watched, &after_deps)?;
            Receipt {
                version: 1,
                stage: Stage::Module,
                dependency_exit_code: Some(outcome.dependency_exit_code),
                build_exit_code: outcome.build_exit_code,
                ready_exit_code: None,
                modules: Vec::new(),
                preflight: None,
            }
        }
        Target::Modules { names } => Receipt {
            version: 1,
            stage: Stage::Modules,
            dependency_exit_code: None,
            build_exit_code: None,
            ready_exit_code: None,
            modules: run_modules(names, &watched, &after_deps)?,
            preflight: None,
        },
        Target::VerifierModules { names } => {
            let preflight = verifier_preflight(&plan.root, &watched)?;
            let modules = if preflight.ready() {
                run_modules(names, &watched, &after_deps)?
            } else {
                Vec::new()
            };
            Receipt {
                version: 1,
                stage: Stage::VerifierModules,
                dependency_exit_code: None,
                build_exit_code: None,
                ready_exit_code: None,
                modules,
                preflight: Some(preflight),
            }
        }
        Target::Ready { name, package } => {
            let target = format!("@{}/{name}", package.as_deref().unwrap_or(""));
            let ready = watched(&["--no-build", "build", &target])?;
            Receipt {
                version: 1,
                stage: Stage::Ready,
                dependency_exit_code: None,
                build_exit_code: None,
                ready_exit_code: Some(ready),
                modules: Vec::new(),
                preflight: None,
            }
        }
    };
    println!("{}", serde_json::to_string(&receipt)?);
    Ok(())
}

fn test_cut_after_dependencies(authorize: &impl Fn() -> io::Result<std::time::Duration>) -> io::Result<()> {
    if option_env!("LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT").is_some()
        && let Some(path) = std::env::var_os("LEAN_HOST_MCP_TEST_MODULE_AFTER_DEPS")
        && let Ok(path) = PathBuf::from(path).canonicalize()
        && path.is_dir()
        && path.join("hold").is_file()
    {
        std::fs::write(path.join("checked"), b"guard passed\n")?;
        while path.join("hold").is_file() {
            authorize()?;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn ordered_modules_stop_before_building_a_cold_prerequisite() {
        let requested = vec![
            "Root.First".into(),
            "Root.NeedsColdImport".into(),
            "Root.Unreached".into(),
        ];
        let calls = RefCell::new(Vec::new());
        let run = |args: &[&str]| {
            calls
                .borrow_mut()
                .push(args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>());
            Ok(if args.last() == Some(&"@/+Root.NeedsColdImport:deps") {
                3
            } else {
                0
            })
        };
        let modules = run_modules(requested.clone(), &run, &|| Ok(())).unwrap();
        let expected = [
            ["--no-build", "build", "@/+Root.First:deps"],
            ["--no-build", "query", "@/+Root.First:setup"],
            ["--log-level=warning", "build", "@/+Root.First:irArts"],
            ["--no-build", "build", "@/+Root.NeedsColdImport:deps"],
        ]
        .map(|stage| stage.map(str::to_owned).to_vec());
        assert_eq!(calls.into_inner(), expected.to_vec());
        let body = serde_json::to_vec(&Receipt {
            version: 1,
            stage: Stage::Modules,
            dependency_exit_code: None,
            build_exit_code: None,
            ready_exit_code: None,
            modules,
            preflight: None,
        })
        .unwrap();
        assert!(Receipt::parse(&body, &Target::Modules { names: requested }).is_ok());
        assert!(
            Receipt::parse(
                &body,
                &Target::Modules {
                    names: vec!["Root.NeedsColdImport".into()]
                }
            )
            .is_err()
        );
    }

    #[test]
    fn module_batch_rejects_duplicate_and_nonlocal_target_syntax() {
        for names in [
            vec![],
            vec!["Root.One".into(); 2],
            vec!["@dependency/+Root.One".into()],
            vec!["Root.One:irArts".into()],
        ] {
            assert!(Target::Modules { names }.validate().is_err());
        }
        assert!(
            Target::Modules {
                names: vec!["Root.First".into(), "Root.Second".into()]
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn verifier_groups_validate_size_uniqueness_and_receipt() {
        for groups in [
            vec![],
            vec![vec![]],
            vec![vec!["Demo.A".into()], vec!["Demo.A".into()]],
            vec![vec!["other/Target".into()]],
            vec![(0..17).map(|n| format!("Demo.M{n}")).collect()],
            (0..65).map(|n| vec![format!("Demo.M{n}")]).collect(),
        ] {
            assert!(Target::VerifierGroups { groups }.validate().is_err());
        }
        let body = |receipt| serde_json::to_vec(&receipt).unwrap();
        let base = serde_json::json!({
            "version": 1, "stage": "verifier_groups",
            "preflight": {"mathlib_exit_code": 0, "linter_exit_code": 0, "linter_executable_ready": true},
            "completed_groups": 1, "first_failure": {"group_index": 1, "phase": "dependencies", "exit_code": 3},
        });
        assert!(GroupedVerifierReceipt::parse(&body(base.clone()), 2).is_ok());
        let mut invented = base;
        invented["first_failure"]["module"] = serde_json::json!("Demo.A");
        assert!(GroupedVerifierReceipt::parse(&body(invented), 2).is_err());
    }

    #[test]
    #[ignore = "requires installed Elan selected by LEAN_SYSROOT"]
    fn grouped_verifier_real_lean_preserves_no_build_boundary() -> Result<(), Box<dyn std::error::Error>> {
        use std::cell::Cell;
        use std::fs;
        use std::time::Duration;

        let sysroot = PathBuf::from(std::env::var_os("LEAN_SYSROOT").ok_or("LEAN_SYSROOT required")?);
        let lake = sysroot.join("bin/lake");
        let fixture = tempfile::tempdir()?;
        let base = fixture.path().canonicalize()?;
        let fixture_at = |name: &str| -> io::Result<PathBuf> {
            let root = base.join(name);
            let workspace = |path: &Path| {
                let status = Command::new(&lake)
                    .current_dir(path)
                    .args(["--no-cache", "update"])
                    .env("GIT_ALLOW_PROTOCOL", "")
                    .status()?;
                if !status.success() {
                    return Err(io::Error::other("fixture Lake update failed"));
                }
                Ok(())
            };
            fs::create_dir_all(root.join("Demo"))?;
            fs::write(
                root.join("lakefile.lean"),
                "import Lake\nopen Lake DSL\npackage demo\nrequire mathlib from \"mathlib\"\nrequire extra from \"extra\"\nlean_lib Demo where\n  globs := #[`Demo.*]\nlean_exe runLinter where\n  root := `LinterFixture\n",
            )?;
            fs::write(root.join("LinterFixture.lean"), "def main : IO Unit := pure ()\n")?;
            for (package, module) in [("mathlib", "Mathlib"), ("extra", "Extra")] {
                let dep = root.join(package);
                fs::create_dir(&dep)?;
                fs::write(
                    dep.join("lakefile.toml"),
                    format!("name = '{package}'\n[[lean_lib]]\nname = '{module}'\n"),
                )?;
                fs::write(
                    dep.join(format!("{module}.lean")),
                    "theorem cacheFixture : True := True.intro\n",
                )?;
                workspace(&dep)?;
            }
            for module in ["A", "B", "C", "Unreached"] {
                fs::write(
                    root.join(format!("Demo/{module}.lean")),
                    format!("theorem {module}.ok : True := True.intro\n"),
                )?;
            }
            fs::write(
                root.join("Demo/D.lean"),
                "import Demo.A\nimport Demo.B\nimport Demo.C\ntheorem D.ok : True := A.ok\n",
            )?;
            fs::write(
                root.join("Demo/Cold.lean"),
                "import Extra\ntheorem Cold.ok : True := cacheFixture\n",
            )?;
            workspace(&root)?;
            let status = Command::new(&lake)
                .current_dir(&root)
                .args(["--no-cache", "build", "@/runLinter", "@mathlib/+Mathlib:olean"])
                .status()?;
            if !status.success() {
                return Err(io::Error::other("fixture readiness build failed"));
            }
            Ok(root)
        };
        let groups = vec![
            ["Demo.A", "Demo.B", "Demo.C"].map(str::to_owned).to_vec(),
            vec!["Demo.D".to_owned()],
        ];
        let run = |root: &Path, grouped: bool| -> io::Result<usize> {
            let calls = Cell::new(0_usize);
            let command = || {
                calls.set(calls.get().saturating_add(1));
                let mut c = Command::new(&lake);
                c.current_dir(root).arg("--no-cache").stdout(Stdio::null());
                c
            };
            let watched = |args: &[&str]| {
                command()
                    .args(args)
                    .status()?
                    .code()
                    .ok_or_else(|| io::Error::other("fixture Lake interrupted"))
            };
            let authorize = || Ok(Duration::from_secs(20));
            let preflight = verifier_preflight(root, &watched)?;
            assert!(preflight.ready());
            if grouped {
                let receipt = grouped_verifier(&groups, preflight, &command, &authorize, &|| Ok(()))?;
                let encoded = serde_json::to_vec(&receipt)?;
                let receipt = GroupedVerifierReceipt::parse(&encoded, groups.len())?;
                assert!(receipt.first_failure.is_none());
                assert_eq!(receipt.completed_groups, 2);
            } else {
                let modules = run_modules(groups.iter().flatten().cloned().collect(), &watched, &|| Ok(()))?;
                assert_eq!(modules.len(), 4);
                assert!(modules.iter().all(ModuleReceipt::succeeded));
            }
            assert!(root.join(".lake/build/lib/lean/Demo/D.olean").is_file());
            Ok(calls.get())
        };
        let old = fixture_at("old")?;
        let grouped = fixture_at("grouped")?;
        assert_eq!(run(&old, false)?, 14);
        assert_eq!(run(&grouped, true)?, 9);

        let launches = Cell::new(0_usize);
        let command = || {
            launches.set(launches.get().saturating_add(1));
            let mut c = Command::new(&lake);
            c.current_dir(&grouped).arg("--no-cache").stdout(Stdio::null());
            c
        };
        let authorize = || Ok(Duration::from_secs(20));
        let ready = || Preflight {
            mathlib_exit_code: 0,
            linter_exit_code: Some(0),
            linter_executable_ready: true,
        };
        let deps = vec![vec!["Demo.Unreached".to_owned(), "Demo.Cold".to_owned()]];
        let receipt = grouped_verifier(&deps, ready(), &command, &authorize, &|| {
            Err(io::Error::other(
                "managed IR attempted despite missing external dependency",
            ))
        })?;
        assert_eq!(receipt.completed_groups, 0);
        let failure = receipt.first_failure.ok_or("expected cold dependency failure")?;
        assert!(matches!(
            failure.phase,
            root_library::Phase::Dependencies | root_library::Phase::Setup
        ));
        assert_eq!(failure.group_index, Some(0));
        assert!(launches.get() <= 3);
        assert!(!grouped.join("extra/.lake/build/lib/lean/Extra.olean").exists());
        assert!(!grouped.join(".lake/build/lib/lean/Demo/Unreached.olean").exists());

        launches.set(0);
        let invalid = vec![vec!["Demo.A".to_owned(), "Demo.D".to_owned()]];
        let receipt = grouped_verifier(&invalid, ready(), &command, &authorize, &|| {
            Err(io::Error::other("managed IR attempted for dependent siblings"))
        })?;
        let failure = receipt.first_failure.ok_or("expected imports failure")?;
        assert!(matches!(failure.phase, root_library::Phase::Imports));
        assert_eq!(failure.group_index, Some(0));
        assert_eq!(launches.get(), 1);
        Ok(())
    }
}
