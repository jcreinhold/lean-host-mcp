//! A small Lake project for build tests: `Chain.C` imports `Chain.B`, which
//! imports `Chain.A` and `Dep` from a path dependency in `dep/`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use super::{TestResult, sysroot_and_pin};

/// A built chain project in a temporary directory under the test target. The
/// directory goes when the value drops.
pub(crate) struct Chain {
    pub(crate) root: PathBuf,
    pub(crate) sysroot: PathBuf,
    _dir: tempfile::TempDir,
}

impl Chain {
    /// Write `text` to `relative` under the root.
    pub(crate) fn write(&self, relative: &str, text: &str) -> TestResult {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
        Ok(())
    }

    /// Run the toolchain's `lake` with `args` in the root, outside the server,
    /// with the environment a build step gets. Fails unless Lake passes.
    pub(crate) fn lake(&self, args: &[&str]) -> TestResult {
        let output = Command::new(self.sysroot.join("bin/lake"))
            .args(args)
            .current_dir(&self.root)
            .env("LEAN_SYSROOT", &self.sysroot)
            .env("LAKE_ARTIFACT_CACHE", "false")
            .env_remove("LEAN_PATH")
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "lake {args:?} failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .into())
        }
    }

    /// The `.olean` Lake builds for module `module` of the root package.
    pub(crate) fn olean(&self, module: &str) -> PathBuf {
        self.root
            .join(".lake/build/lib/lean")
            .join(module.replace('.', "/"))
            .with_extension("olean")
    }
}

const FILES: &[(&str, &str)] = &[
    // A Lean lakefile, as Proofs has: Lake caches its elaborated form under
    // `.lake/config`, which a TOML lakefile never gets.
    (
        "lakefile.lean",
        "import Lake\nopen Lake DSL\n\npackage chain\n\nrequire dep from \"dep\"\n\n@[default_target]\nlean_lib Chain\n",
    ),
    ("Chain.lean", "import Chain.C\n"),
    ("Chain/A.lean", "def a : Nat := 1\n"),
    ("Chain/B.lean", "import Chain.A\nimport Dep\n\ndef b : Nat := a + dep\n"),
    ("Chain/C.lean", "import Chain.B\n\ntheorem b_eq : b = 2 := rfl\n"),
    (
        "dep/lakefile.toml",
        "name = \"dep\"\ndefaultTargets = [\"Dep\"]\n\n[[lean_lib]]\nname = \"Dep\"\n",
    ),
    ("dep/Dep.lean", "def dep : Nat := 1\n"),
    // The dependency's own manifest, which Lake does not write.
    (
        "dep/lake-manifest.json",
        "{\"version\": \"1.3.0\", \"packagesDir\": \".lake/packages\", \"packages\": [], \"name\": \"dep\", \
         \"lakeDir\": \".lake\"}\n",
    ),
];

/// Write a chain project with the fixture's toolchain and build it.
pub(crate) fn chain_project() -> TestResult<Chain> {
    let (sysroot, pin) = sysroot_and_pin()?;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let chain = Chain {
        root: dir.path().canonicalize()?,
        sysroot: sysroot.canonicalize()?,
        _dir: dir,
    };
    chain.write("lean-toolchain", &format!("{pin}\n"))?;
    for (path, text) in FILES {
        chain.write(path, text)?;
    }
    chain.lake(&["build"])?;
    Ok(chain)
}
