//! Fixed, writer-owned sequencer. The original helper owns all descendants.
use std::{
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
const MAX_MODULES: usize = 64;
const MAX_PLAN_BYTES: usize = 32 * 1024;

#[path = "first_party_clean.rs"]
pub(crate) mod first_party_clean;
#[path = "whole_lint.rs"]
pub(crate) mod whole_lint;

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Target {
    Module { name: String },
    Modules { names: Vec<String> },
    VerifierModules { names: Vec<String> },
    Ready { name: String, package: Option<String> },
    WholeLint { lakefile_sha256: [u8; 32] },
    FirstPartyClean { lakefile_sha256: [u8; 32] },
}

impl Target {
    pub(crate) fn validate(&self) -> io::Result<()> {
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
        let module = |target: &str| name(target) && !target.contains('-');
        let valid = match self {
            Self::Module { name: target } => module(target),
            Self::Modules { names } | Self::VerifierModules { names } => {
                let mut unique = std::collections::BTreeSet::new();
                !names.is_empty()
                    && names.len() <= MAX_MODULES
                    && names.iter().all(|target| module(target) && unique.insert(target))
            }
            Self::WholeLint { .. } | Self::FirstPartyClean { .. } => true,
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

fn run_module(
    name: String,
    watched: &impl Fn(&[&str]) -> io::Result<i32>,
    after_deps: &impl Fn() -> io::Result<()>,
) -> io::Result<ModuleReceipt> {
    let mut dependency_exit_code = watched(&["--no-build", "build", &format!("@/+{name}:deps")])?;
    if dependency_exit_code == 0 {
        // Elaboration and IR generation both widen transitive imports in setup.
        dependency_exit_code = watched(&["--no-build", "query", &format!("@/+{name}:setup")])?;
    }
    let build_exit_code = if dependency_exit_code == 0 {
        after_deps()?;
        Some(watched(&[
            "--log-level=warning",
            "build",
            &format!("@/+{name}:irArts"),
        ])?)
    } else {
        None
    };
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
    let lake = lake_executable(&plan.root)?;
    if let Target::WholeLint { lakefile_sha256 } = &plan.target {
        return whole_lint::run(&plan.root, &lake, lakefile_sha256, authorize);
    }
    let watched = |args: &[&str]| -> io::Result<i32> {
        authorize()?;
        Command::new(&lake)
            .current_dir(&plan.root)
            .arg("--no-cache")
            .args(args)
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .code()
            .ok_or_else(|| io::Error::other("fixed Lake command was interrupted"))
    };

    let after_deps = || test_cut_after_dependencies(&authorize);
    let receipt = match plan.target {
        Target::WholeLint { .. } | Target::FirstPartyClean { .. } => {
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
}
