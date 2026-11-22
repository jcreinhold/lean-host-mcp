//! Build both development shim packages into a private macOS directory.
//!
//! Arguments: SDK checkout, installed Elan toolchain, empty mode-0700 output
//! directory, and a compatible lean-host-mcp or standalone SDK writer binary.
//! Keep the output path stable: the generated native artifacts may embed it.

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context as _, Result, ensure};
    use lean_toolchain::shared_writer::SharedWriter;
    use sha2::{Digest as _, Sha256};
    use std::{
        collections::BTreeMap,
        env, fs,
        os::unix::fs::{MetadataExt as _, symlink},
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };

    const HOST: &str = "lean-rs-host-shims";
    const INTEROP: &str = "lean-rs-interop-shims";
    const TIMEOUT: Duration = Duration::from_secs(60);
    const INPUT_LIMIT: u64 = 1024 * 1024;
    type Identity = (u64, u64, [u8; 32]);
    type Inventory = BTreeMap<PathBuf, Identity>;

    fn canonical_directory(path: &Path) -> Result<fs::Metadata> {
        ensure!(
            path.is_absolute() && path.canonicalize()? == path,
            "noncanonical directory {}",
            path.display()
        );
        let metadata = fs::symlink_metadata(path)?;
        ensure!(metadata.is_dir(), "expected directory {}", path.display());
        Ok(metadata)
    }

    fn input_identity(path: &Path) -> Result<Identity> {
        use std::io::Read as _;
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            meta.is_file() && meta.len() <= INPUT_LIMIT,
            "invalid shim source {}",
            path.display()
        );
        let file = fs::File::open(path)?;
        let opened = file.metadata()?;
        ensure!(
            (meta.dev(), meta.ino()) == (opened.dev(), opened.ino()),
            "source replaced {}",
            path.display()
        );
        let mut bytes = Vec::new();
        file.take(INPUT_LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == meta.len(),
            "source size changed {}",
            path.display()
        );
        Ok((meta.dev(), meta.ino(), Sha256::digest(bytes).into()))
    }

    fn inventory(path: &Path, inputs: &mut Inventory) -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if meta.is_dir() {
            inputs.insert(path.to_owned(), (meta.dev(), meta.ino(), [0; 32]));
            for entry in fs::read_dir(path)? {
                inventory(&entry?.path(), inputs)?;
            }
        } else {
            inputs.insert(path.to_owned(), input_identity(path)?);
        }
        ensure!(inputs.len() <= 128, "unexpectedly large development shim inventory");
        Ok(())
    }

    fn members(module: &str) -> [PathBuf; 6] {
        [
            "lakefile.lean".into(),
            "lake-manifest.json".into(),
            "lean-toolchain".into(),
            "c".into(),
            module.into(),
            format!("{module}.lean").into(),
        ]
    }

    fn snapshot(packages: &[(PathBuf, &str)]) -> Result<Inventory> {
        let mut inputs = Inventory::new();
        for (root, module) in packages {
            let meta = canonical_directory(root)?;
            inputs.insert(root.clone(), (meta.dev(), meta.ino(), [0; 32]));
            for member in members(module) {
                inventory(&root.join(member), &mut inputs)?;
            }
        }
        Ok(inputs)
    }

    fn installed_lake(toolchain: &str) -> Result<PathBuf> {
        let installed = Command::new("elan").args(["toolchain", "list"]).output()?;
        ensure!(installed.status.success(), "cannot enumerate installed Elan toolchains");
        let normalized = toolchain.replace('/', "--").replace(':', "---");
        ensure!(
            std::str::from_utf8(&installed.stdout)?.lines().any(|line| {
                line.split_whitespace()
                    .next()
                    .is_some_and(|name| name == toolchain || name == normalized)
            }),
            "install {toolchain} explicitly before building"
        );
        let selected = Command::new("elan")
            .env("ELAN_TOOLCHAIN", toolchain)
            .args(["which", "lake"])
            .output()?;
        ensure!(selected.status.success(), "Elan could not select Lake");
        let lake = PathBuf::from(std::str::from_utf8(&selected.stdout)?.trim());
        ensure!(
            lake.is_file() && lake.canonicalize()? == lake,
            "noncanonical Elan Lake executable"
        );
        Ok(lake)
    }

    pub(super) fn run() -> Result<()> {
        let arguments: Vec<_> = env::args_os().skip(1).collect();
        let [sdk, toolchain, destination, writer] = arguments.as_slice() else {
            anyhow::bail!("usage: build_development_shims SDK_CHECKOUT TOOLCHAIN EMPTY_OUTPUT_DIR WRITER");
        };
        let toolchain = toolchain.to_str().context("toolchain must be UTF-8")?;
        let version = match toolchain {
            "leanprover/lean4:v4.35.0" => "4.35.0",
            "leanprover/lean4:v4.36.0" => "4.36.0",
            _ => anyhow::bail!("select an installed Lean 4.35.0 or 4.36.0 toolchain"),
        };
        let sdk = Path::new(sdk);
        canonical_directory(sdk)?;
        let destination = Path::new(destination);
        let meta = canonical_directory(destination)?;
        ensure!(
            meta.uid() == rustix::process::geteuid().as_raw()
                && meta.mode() & 0o777 == 0o700
                && fs::read_dir(destination)?.next().is_none(),
            "output must be empty, private, and caller-owned"
        );
        ensure!(
            !destination.starts_with(sdk),
            "output must be outside the SDK source tree"
        );

        let source = sdk.join("crates/lean-rs-host/shims");
        let packages = [
            (source.join(HOST), "LeanRsHostShims"),
            (source.join(INTEROP), "LeanRsInterop"),
        ];
        let lake = installed_lake(toolchain)?;
        let lake_meta = lake.metadata()?;
        let header = lake
            .parent()
            .context("missing Lake bin directory")?
            .parent()
            .context("missing Lean sysroot")?
            .join("include/lean/lean.h");
        let header_identity = input_identity(&header)?;

        let host = destination.join(HOST);
        let interop = destination.join(INTEROP);
        for view in [&host, &interop] {
            fs::create_dir(view)?;
        }
        let scratch = destination.join("tmp");
        fs::create_dir(&scratch)?;
        let workspace = destination.join("driver");
        fs::create_dir(&workspace)?;
        // Preserve the existing EX package domains. The helper also owns the
        // two real output directories and the driver through confirmed exit.
        let mut custody = SharedWriter::open_with_write_roots(
            Path::new(writer),
            &packages[0].0,
            &[packages[1].0.clone(), host.clone(), interop.clone(), workspace.clone()],
            TIMEOUT,
            &|| false,
        )?;
        let before = snapshot(&packages)?;
        for ((root, module), view) in packages.iter().zip([&host, &interop]) {
            for member in members(module) {
                symlink(root.join(&member), view.join(&member))?;
            }
        }
        fs::write(workspace.join("lean-toolchain"), format!("{toolchain}\n"))?;
        fs::write(
            workspace.join("lakefile.toml"),
            format!(
                "name = \"development_shim_driver\"\n\n[[require]]\nname = \"lean_rs_host_shims\"\npath = {}\n",
                serde_json::to_string(&host)?,
            ),
        )?;

        let dependency = |name: &str, dir: &Path, config: &str, inherited: bool| {
            serde_json::json!({
                "type": "path", "scope": "", "name": name,
                "manifestFile": "lake-manifest.json", "inherited": inherited,
                "dir": dir, "configFile": config,
            })
        };
        fs::write(
            workspace.join("lake-manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "1.1.0", "packagesDir": ".lake/packages", "name": "development_shim_driver",
                "lakeDir": ".lake", "packages": [
                    dependency("lean_rs_host_shims", &host, "lakefile.lean", false),
                    dependency("lean_rs_interop_shims", &interop, "lakefile.lean", true),
                ],
            }))?,
        )?;
        let profile = format!(
            "(version 1) (allow default) (deny file-write* (require-not (subpath {}))) (deny network*)",
            serde_json::to_string(destination)?,
        );
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-p")
            .arg(profile)
            .arg(&lake)
            .args(["build", "LeanRsHostShims", "LeanRsInterop", "--no-cache", "--fail-fast"])
            .current_dir(&workspace)
            .env("TMPDIR", &scratch)
            .env("LAKE_CACHE_DIR", "")
            .env("LAKE_NO_CACHE", "true")
            .env("LEAN_NUM_THREADS", "2")
            .env_remove("LEAN_PATH")
            .env_remove("LEAN_SRC_PATH")
            .env_remove("LEAN_SYSROOT");
        let result = custody.run(&command, TIMEOUT)?;
        ensure!(
            result.status.success(),
            "Lake build failed:\n{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        ensure!(
            snapshot(&packages)? == before,
            "development shim source identity changed"
        );
        let selected = lake.metadata()?;
        ensure!(
            (selected.dev(), selected.ino()) == (lake_meta.dev(), lake_meta.ino())
                && input_identity(&header)? == header_identity,
            "Elan toolchain identity changed"
        );
        let build = if version == "4.36.0" {
            ".lake/lean-rs-generated/lean-4.36.0"
        } else {
            ".lake/build"
        };
        for (view, library) in [
            (host, "liblean__rs__host__shims_LeanRsHostShims.dylib"),
            (interop, "liblean__rs__interop__shims_LeanRsInterop.dylib"),
        ] {
            let image = view.join(build).join("lib").join(library);
            ensure!(
                image.is_file() && image.canonicalize()? == image,
                "missing private shim image {}",
                image.display()
            );
        }
        custody.close(TIMEOUT)?;
        print!("{}", String::from_utf8_lossy(&result.stdout));
        println!("Private Lean {version} shim outputs: {}", destination.display());
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    macos::run()
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("private development shim driver currently requires macOS write confinement")
}
