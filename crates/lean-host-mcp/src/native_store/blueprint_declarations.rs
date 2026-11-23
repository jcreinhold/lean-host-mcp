//! Fixed declaration checker tail of the admitted root library writer.
use std::{io, path::Path, process::Command};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{pinned_lake_config::file_bytes, tooling_suites::capture};

const CHECKER: &str = "scripts/lean/check-decls.lean";
const LIST: &str = "blueprint/lean_decls";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inputs {
    checker_sha256: [u8; 32],
    declarations_sha256: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    verdict: Verdict,
    exit_code: Option<i32>,
    output: String,
    output_truncated: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Present,
    Missing,
    Unavailable,
}

impl Receipt {
    pub(super) fn valid(&self) -> bool {
        self.output.len() <= 2048
            && match self.verdict {
                Verdict::Present => self.exit_code == Some(0),
                Verdict::Missing => self.exit_code == Some(1),
                Verdict::Unavailable => self.exit_code != Some(0),
            }
    }

    fn unavailable() -> Self {
        Self {
            verdict: Verdict::Unavailable,
            exit_code: None,
            output: String::new(),
            output_truncated: false,
        }
    }
}

impl Inputs {
    pub(super) fn check(&self, root: &Path) -> io::Result<()> {
        let checked = |path, expected: &[u8; 32]| {
            let bytes = file_bytes(&root.join(path))?;
            if <[u8; 32]>::from(Sha256::digest(&bytes)) != *expected {
                return Err(io::Error::other("admitted blueprint input changed"));
            }
            Ok(bytes)
        };
        checked(CHECKER, &self.checker_sha256)?;
        std::str::from_utf8(&checked(LIST, &self.declarations_sha256)?)
            .map(|_| ())
            .map_err(io::Error::other)
    }

    pub(super) fn run(&self, root: &Path, lake: &Path, mut command: Command) -> io::Result<Receipt> {
        if self.check(root).is_err() {
            return Ok(Receipt::unavailable());
        }
        let sysroot = lake
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| io::Error::other("invalid Lake executable"))?;
        let lean = sysroot.join("bin/lean");
        let library = sysroot
            .join("lib/lean")
            .join(format!("libLake_shared{}", std::env::consts::DLL_SUFFIX));
        if !library.is_file() || library.canonicalize()? != library || !lean.is_file() || lean.canonicalize()? != lean {
            return Ok(Receipt::unavailable());
        }
        let mut load = std::ffi::OsString::from("--load-dynlib=");
        load.push(library);
        command
            .args(["--no-build", "env"])
            .arg(lean)
            .arg(load)
            .args(["--run", CHECKER, "--managed", LIST])
            .env("LEAN_SYSROOT", sysroot)
            .env_remove("LEAN_PATH")
            .env_remove("LEAN_SRC_PATH");
        let outcome = capture(command, "blueprint-declarations")?;
        let (verdict, exit_code) = match outcome.exit_code {
            0 => (Verdict::Present, 0),
            // Only the managed checker emits 10. Lake and Lean can fail with
            // status 1 before the checker enters main.
            10 => (Verdict::Missing, 1),
            code => (Verdict::Unavailable, code),
        };
        Ok(Receipt {
            verdict,
            exit_code: Some(exit_code),
            output: outcome.output,
            output_truncated: outcome.output_truncated,
        })
    }
}
