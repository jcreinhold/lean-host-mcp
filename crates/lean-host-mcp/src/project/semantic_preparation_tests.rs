//! The semantic runtime cache, and the refusal to serve a runtime that
//! install has not built.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use super::*;

#[test]
fn an_unbuilt_runtime_tells_the_operator_to_rerun_install() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    // A Lake that answers `--no-build` with Lake's "needs a rebuild" code.
    let sysroot = root.join("sysroot");
    fs::create_dir_all(sysroot.join("bin"))?;
    let lake = sysroot.join("bin/lake");
    fs::write(&lake, "#!/bin/sh\nexit 3\n")?;
    fs::set_permissions(&lake, fs::Permissions::from_mode(0o755))?;
    let cache = root.join("cache");
    fs::create_dir(&cache)?;

    let error = open_runtime(cache, "semantic-unbuilt-test", &sysroot).err();

    let message = error.map(|error| error.to_string()).unwrap_or_default();
    assert!(
        message.contains("is not built; rerun `lean-host-mcp install --lake-root <root>`"),
        "{message}"
    );
    assert!(
        !SEMANTIC_RUNTIMES
            .lock()
            .contains_key(&("semantic-unbuilt-test".to_owned(), sysroot)),
        "a failed open is not kept"
    );
    Ok(())
}

#[test]
fn a_missing_semantic_cache_directory_is_created_and_a_bad_one_is_named() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let missing = root.join("semantic").join("cache");
    assert_eq!(env_cache_dir(missing.clone()).ok(), Some(missing.clone()));
    assert!(missing.is_dir());

    let file = root.join("file");
    fs::write(&file, "not a directory")?;
    let error = env_cache_dir(file.join("cache"))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains(&file.display().to_string()), "{error}");

    let relative = env_cache_dir(PathBuf::from("relative-cache"))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(relative.contains("canonical absolute path"), "{relative}");
    Ok(())
}
