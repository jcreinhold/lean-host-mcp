//! Fixed argv for the approved project's prebuilt module linter.

use std::{ffi::OsString, io, os::unix::fs::PermissionsExt as _, path::Path};

use super::{MAX_NAMES, invalid, qualified_name};
use crate::lake_meta::RUN_LINTER_REL;

pub(crate) const OPERATION: &str = "lean_module_lint_v1";

pub(super) fn valid_modules(modules: &[String]) -> bool {
    !modules.is_empty()
        && modules.len() <= MAX_NAMES
        && modules
            .iter()
            .all(|module| qualified_name(module) && !module.ends_with(".lean"))
}

pub(super) fn executable(root: &Path) -> io::Result<std::path::PathBuf> {
    let executable = root.join(RUN_LINTER_REL);
    let meta = executable.metadata()?;
    if executable.canonicalize()? != executable || !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(invalid("canonical prebuilt runLinter required"));
    }
    Ok(executable)
}

pub(super) fn command(root: &Path, modules: &[String]) -> io::Result<Vec<OsString>> {
    let mut command = vec![
        executable(root)?.into_os_string(),
        "--no-build".into(),
        "--module-only".into(),
        "--deny=all".into(),
    ];
    command.extend(modules.iter().map(OsString::from));
    Ok(command)
}

pub(super) fn record_verdict(body: &mut serde_json::Value, exit_code: Option<i32>) {
    body["verdict"] = match exit_code {
        Some(0) => "pass",
        Some(_) => "fail",
        None => "unavailable",
    }
    .into();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prebuilt_lint_uses_only_fixed_options_and_ordered_modules() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let modules = ["Z.Second".to_owned(), "A.First".to_owned()];
        assert!(valid_modules(&modules));
        assert!(!valid_modules(&["--allow=all".to_owned()]));
        assert!(!valid_modules(&["A.lean".to_owned()]));
        assert!(command(&root, &modules).is_err());

        let alternative = root.join("custom/bin/runLinter");
        std::fs::create_dir_all(alternative.parent().unwrap()).unwrap();
        std::fs::write(&alternative, b"fixture executable").unwrap();
        std::fs::set_permissions(&alternative, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(command(&root, &modules).is_err());

        let path = root.join(RUN_LINTER_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"fixture executable").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let args = command(&root, &modules).unwrap();
        assert_eq!(args[0], path.into_os_string());
        assert_eq!(
            args[1..],
            ["--no-build", "--module-only", "--deny=all", "Z.Second", "A.First"]
        );
    }
}
