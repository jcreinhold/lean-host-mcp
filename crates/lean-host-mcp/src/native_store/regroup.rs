//! Guarded preparation and read-only `RegroupVariables` analysis in one writer.
use std::{collections::HashSet, io, path::Path, process::Command, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{Plan, Target, module_name, pinned_lake_config::file_bytes, root_library, run_module, tooling_suites};

pub(crate) const OPERATION: &str = "lean_regroup_v1";
const CLI: &str = "scripts/lean/regroup-variables.lean";
const TOOL: &str = "scripts/lean/RegroupVariables.lean";
const CORE: &str = "scripts/lean/RegroupVariables/Core.lean";
const BATCH_SIZE: usize = 16;
const OUTPUT_LIMIT: usize = 2048;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Check,
    Explain,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Input {
    file: String,
    sha256: [u8; 32],
}

impl Input {
    fn module(&self) -> Option<String> {
        let relative = self.file.strip_prefix("Proofs/")?.strip_suffix(".lean")?;
        let name = format!("Proofs.{}", relative.replace('/', "."));
        (self.file.len() <= 255 && !relative.contains('.') && module_name(&name)).then_some(name)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    mode: Mode,
    files: Option<Vec<Input>>,
    library: Option<Library>,
    lakefile_sha256: [u8; 32],
    manifest_sha256: [u8; 32],
    cli_sha256: [u8; 32],
    library_sha256: [u8; 32],
    core_sha256: [u8; 32],
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Library {
    module_count: usize,
    module_names_sha256: [u8; 32],
    sources_sha256: [u8; 32],
}

impl Library {
    fn verify_sources(&self, root: &Path, files: &[String]) -> io::Result<()> {
        let mut digest = Sha256::new();
        // Lexicographically sorted canonical paths, each followed by NUL and its raw SHA-256.
        for file in files {
            digest.update(file.as_bytes());
            digest.update([0]);
            digest.update(Sha256::digest(file_bytes(&root.join(file))?));
        }
        if <[u8; 32]>::from(digest.finalize()) != self.sources_sha256 {
            return Err(io::Error::other("admitted regroup library sources changed"));
        }
        Ok(())
    }

    fn files(&self, root: &Path, modules: &[String]) -> io::Result<Vec<String>> {
        let mut files = Vec::with_capacity(modules.len());
        for module in modules {
            if module != "Proofs" && !module.starts_with("Proofs.") {
                return Err(io::Error::other("regroup inventory contains a non-Proofs module"));
            }
            files.push(format!("{}.lean", module.replace('.', "/")));
        }
        files.sort_unstable();
        if files.len() < 2 || !files.iter().any(|file| file == "Proofs.lean") {
            return Err(io::Error::other(
                "regroup requires the root and at least one Proofs module",
            ));
        }
        self.verify_sources(root, &files)?;
        Ok(files)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Inputs,
    Environment,
    Inventory,
    Preparation,
    Dependencies,
    Build,
    Tooling,
    Setup,
    Analysis,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u8,
    stage: String,
    phase: Phase,
    exit_code: Option<i32>,
    output: String,
    output_truncated: bool,
    selected_files: usize,
    completed_files: usize,
    completed_batches: usize,
}

impl Receipt {
    fn new(phase: Phase, exit_code: Option<i32>) -> Self {
        Self {
            version: 1,
            stage: "regroup".into(),
            phase,
            exit_code,
            output: String::new(),
            output_truncated: false,
            selected_files: 0,
            completed_files: 0,
            completed_batches: 0,
        }
    }

    fn observed(phase: Phase, outcome: tooling_suites::Check) -> Self {
        Self {
            output: outcome.output,
            output_truncated: outcome.output_truncated,
            ..Self::new(phase, Some(outcome.exit_code))
        }
    }

    fn append(&mut self, outcome: &tooling_suites::Check) {
        let room = OUTPUT_LIMIT.saturating_sub(self.output.len());
        let end = outcome.output.floor_char_boundary(room.min(outcome.output.len()));
        if !self.output_truncated
            && let Some(prefix) = outcome.output.get(..end)
        {
            self.output.push_str(prefix);
        }
        self.output_truncated |= outcome.output_truncated || end < outcome.output.len();
    }

    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let value: Self = serde_json::from_slice(bytes)?;
        if value.version != 1
            || value.stage != "regroup"
            || value.output.len() > OUTPUT_LIMIT
            || value.selected_files > root_library::MAX_MODULES
            || value.completed_files > value.selected_files
            || value.completed_batches > value.completed_files
            || value.completed_files > value.completed_batches.saturating_mul(BATCH_SIZE)
            || match value.phase {
                Phase::Analysis => {
                    value.exit_code.is_none()
                        || (matches!(value.exit_code, Some(0 | 1))
                            && (value.selected_files == 0 || value.completed_files != value.selected_files))
                }
                Phase::Inputs | Phase::Setup | Phase::Inventory => value.exit_code.is_some(),
                Phase::Preparation => value.exit_code.is_some_and(|code| code <= 0),
                Phase::Environment | Phase::Dependencies | Phase::Build | Phase::Tooling => {
                    value.exit_code.is_none_or(|code| code <= 0)
                }
            }
        {
            return Err(io::Error::other("invalid regroup receipt"));
        }
        Ok(value)
    }
}

impl Request {
    pub(super) fn valid(&self) -> bool {
        match (&self.files, &self.library) {
            (Some(files), None) => {
                let mut seen = HashSet::new();
                !files.is_empty()
                    && files.len() <= BATCH_SIZE
                    && files
                        .iter()
                        .all(|file| file.module().is_some() && seen.insert(&file.file))
            }
            (None, Some(library)) => {
                matches!(self.mode, Mode::Check) && (2..=root_library::MAX_MODULES).contains(&library.module_count)
            }
            _ => false,
        }
    }

    fn check_files(&self, root: &Path) -> io::Result<()> {
        let check = |file: &str, digest: &[u8; 32]| {
            if <[u8; 32]>::from(Sha256::digest(file_bytes(&root.join(file))?)) != *digest {
                return Err(io::Error::other("admitted regroup input changed"));
            }
            Ok(())
        };
        check(CLI, &self.cli_sha256)?;
        check(TOOL, &self.library_sha256)?;
        check(CORE, &self.core_sha256)?;
        for file in self.files.iter().flatten() {
            check(&file.file, &file.sha256)?;
        }
        Ok(())
    }

    pub(super) fn run(
        &self,
        plan: &Plan,
        lake: &Path,
        command: impl Fn() -> Command,
        watched: &impl Fn(&[&str]) -> io::Result<i32>,
        authorize: &impl Fn() -> io::Result<Duration>,
        after_deps: &impl Fn() -> io::Result<()>,
    ) -> io::Result<Receipt> {
        let root = &plan.root;
        authorize()?;
        if tooling_suites::check(root, &self.lakefile_sha256, &self.manifest_sha256)
            .and_then(|()| self.check_files(root))
            .is_err()
        {
            return Ok(Receipt::new(Phase::Inputs, None));
        }
        let sysroot = lake
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| io::Error::other("invalid Lake toolchain"))?;
        let tool_command = || {
            let mut command = command();
            command
                .env("LEAN_SYSROOT", sysroot)
                .env_remove("LEAN_PATH")
                .env_remove("LEAN_SRC_PATH");
            command
        };
        let mut library_files = Vec::new();
        if let Some(library) = &self.library {
            let preparation = Plan {
                root: root.clone(),
                target: Target::RootLibrary {
                    name: "Proofs".into(),
                    module_count: library.module_count,
                    module_names_sha256: library.module_names_sha256,
                    blueprint: None,
                    lakefile_sha256: self.lakefile_sha256,
                    manifest_sha256: self.manifest_sha256,
                },
                boot: plan.boot,
                expires_ns: plan.expires_ns,
            };
            let receipt = root_library::run(&preparation, lake, tool_command, authorize, after_deps, |modules| {
                library_files = library.files(root, modules)?;
                Ok(())
            })?;
            if let Some(failure) = receipt.first_failure {
                let phase = if matches!(failure.phase, root_library::Phase::InventoryMismatch) {
                    Phase::Inventory
                } else {
                    Phase::Preparation
                };
                return Ok(Receipt::new(phase, failure.exit_code));
            }
        }
        authorize()?;
        let paths = match tooling_suites::lean_path(tool_command(), self.library.is_none())? {
            Ok(paths) => paths,
            Err(outcome) => return Ok(Receipt::observed(Phase::Environment, outcome)),
        };
        for file in self.files.iter().flatten() {
            let name = file.module().ok_or_else(|| io::Error::other("invalid regroup file"))?;
            let outcome = run_module(name, watched, after_deps)?;
            if outcome.dependency_exit_code != 0 {
                return Ok(Receipt::new(Phase::Dependencies, Some(outcome.dependency_exit_code)));
            }
            if let Some(code) = outcome.build_exit_code.filter(|&code| code != 0) {
                return Ok(Receipt::new(Phase::Build, Some(code)));
            }
        }
        authorize()?;
        let mut build = tool_command();
        build
            .current_dir(tooling_suites::package_root(root)?)
            .args(["build", "@/+RegroupVariables:irArts"]);
        let outcome = tooling_suites::capture(build, "regroup-tooling")?;
        if outcome.exit_code != 0 {
            return Ok(Receipt::observed(Phase::Tooling, outcome));
        }
        authorize()?;
        if self.check_files(root).is_err()
            || self
                .library
                .as_ref()
                .is_some_and(|library| library.verify_sources(root, &library_files).is_err())
        {
            return Ok(Receipt::new(Phase::Inputs, None));
        }
        let files: Vec<_> = if let Some(files) = &self.files {
            files.iter().map(|file| file.file.as_str()).collect()
        } else {
            library_files
                .iter()
                .map(String::as_str)
                .filter(|file| *file != "Proofs.lean")
                .collect()
        };
        for file in &files {
            let base = file.trim_end_matches(".lean");
            if file_bytes(&root.join(format!(".lake/build/ir/{base}.setup.json"))).is_err() {
                return Ok(Receipt::new(Phase::Setup, None));
            }
        }
        let lean_path = std::env::join_paths(
            std::iter::once(root.join(tooling_suites::BUILD_DIR).join("lib/lean")).chain(std::env::split_paths(&paths)),
        )
        .map_err(io::Error::other)?;
        let mode = match self.mode {
            Mode::Check => "--check",
            Mode::Explain => "--explain",
        };
        let mut receipt = Receipt::new(Phase::Analysis, Some(0));
        receipt.selected_files = files.len();
        for batch in files.chunks(BATCH_SIZE) {
            authorize()?;
            let mut run = Command::new(sysroot.join("bin/lean"));
            run.current_dir(root)
                .env("LEAN_SYSROOT", sysroot)
                .env("LEAN_PATH", &lean_path)
                .env_remove("LEAN_SRC_PATH")
                .args(["--run", CLI, mode, "--require-setup"])
                .args(batch);
            let outcome = tooling_suites::capture(run, "regroup")?;
            receipt.append(&outcome);
            match outcome.exit_code {
                0 => {}
                1 => receipt.exit_code = Some(1),
                code => {
                    receipt.exit_code = Some(code);
                    return Ok(receipt);
                }
            }
            receipt.completed_files = receipt.completed_files.saturating_add(batch.len());
            receipt.completed_batches = receipt.completed_batches.saturating_add(1);
        }
        authorize()?;
        if self.check_files(root).is_err()
            || self
                .library
                .as_ref()
                .is_some_and(|library| library.verify_sources(root, &library_files).is_err())
        {
            return Ok(Receipt::new(Phase::Inputs, None));
        }
        Ok(receipt)
    }
}
