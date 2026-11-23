//! Guarded preparation and read-only `RegroupVariables` analysis in one writer.
use std::{collections::HashSet, io, path::Path, process::Command, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{module_name, pinned_lake_config::file_bytes, run_module, tooling_suites};

pub(crate) const OPERATION: &str = "lean_regroup_v1";
const CLI: &str = "scripts/lean/regroup-variables.lean";
const TOOL: &str = "scripts/lean/RegroupVariables.lean";
const CORE: &str = "scripts/lean/RegroupVariables/Core.lean";
const MAX_FILES: usize = 16;

#[derive(Clone, Deserialize, Serialize)]
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
    files: Vec<Input>,
    lakefile_sha256: [u8; 32],
    manifest_sha256: [u8; 32],
    cli_sha256: [u8; 32],
    library_sha256: [u8; 32],
    core_sha256: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Inputs,
    Environment,
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
        }
    }

    fn observed(phase: Phase, outcome: tooling_suites::Check) -> Self {
        Self {
            output: outcome.output,
            output_truncated: outcome.output_truncated,
            ..Self::new(phase, Some(outcome.exit_code))
        }
    }

    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let value: Self = serde_json::from_slice(bytes)?;
        if value.version != 1
            || value.stage != "regroup"
            || value.output.len() > 2048
            || match value.phase {
                Phase::Analysis => value.exit_code.is_none(),
                Phase::Inputs | Phase::Setup => value.exit_code.is_some(),
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
        let mut seen = HashSet::new();
        !self.files.is_empty()
            && self.files.len() <= MAX_FILES
            && self
                .files
                .iter()
                .all(|file| file.module().is_some() && seen.insert(&file.file))
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
        for file in &self.files {
            check(&file.file, &file.sha256)?;
        }
        Ok(())
    }

    pub(super) fn run(
        &self,
        root: &Path,
        lake: &Path,
        command: impl Fn() -> Command,
        watched: &impl Fn(&[&str]) -> io::Result<i32>,
        authorize: &impl Fn() -> io::Result<Duration>,
        after_deps: &impl Fn() -> io::Result<()>,
    ) -> io::Result<Receipt> {
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
        authorize()?;
        let paths = match tooling_suites::lean_path(tool_command())? {
            Ok(paths) => paths,
            Err(outcome) => return Ok(Receipt::observed(Phase::Environment, outcome)),
        };
        for file in &self.files {
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
        if self.check_files(root).is_err() {
            return Ok(Receipt::new(Phase::Inputs, None));
        }
        for input in &self.files {
            let base = input.file.trim_end_matches(".lean");
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
        authorize()?;
        let mut run = Command::new(sysroot.join("bin/lean"));
        run.current_dir(root)
            .env("LEAN_SYSROOT", sysroot)
            .env("LEAN_PATH", lean_path)
            .env_remove("LEAN_SRC_PATH")
            .args(["--run", CLI, mode, "--require-setup"])
            .args(self.files.iter().map(|file| &file.file));
        tooling_suites::capture(run, "regroup").map(|outcome| Receipt::observed(Phase::Analysis, outcome))
    }
}
