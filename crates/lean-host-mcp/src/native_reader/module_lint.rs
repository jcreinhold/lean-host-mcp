//! Fixed argv for the approved project's prebuilt module linter.

use std::{ffi::OsString, io, os::unix::fs::PermissionsExt as _, path::Path};

use serde::{Deserialize, Serialize};

use super::{MAX_NAMES, NativeReaderResult, invalid, qualified_name};
use crate::lake_meta::{BUILD_LIB_REL, RUN_LINTER_REL};

pub(crate) const OPERATION: &str = "lean_module_lint_v1";
pub(crate) const CHECKPOINT: &str = "lean_checkpoint_lint_v1";
pub(crate) const ROOT_ARCHITECTURE: &str = "lean_root_architecture_lint_v1";

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Checkpoint {
    Default(CheckpointDefault),
    Proof {
        term_objects: i32,
        branch_score: i32,
        branch_depth: i32,
    },
}

#[derive(Serialize)]
pub(super) struct CheckpointDefault {}

impl<'de> Deserialize<'de> for CheckpointDefault {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = std::collections::BTreeMap::<String, serde::de::IgnoredAny>::deserialize(deserializer)?;
        if fields.is_empty() {
            Ok(Self {})
        } else {
            Err(serde::de::Error::custom("default checkpoint takes no fields"))
        }
    }
}

impl Checkpoint {
    pub(super) fn valid(&self) -> bool {
        match *self {
            Self::Default(_) => true,
            Self::Proof {
                term_objects,
                branch_score,
                branch_depth,
            } => term_objects > 0 && branch_score > 0 && branch_depth > 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Request<'a> {
    pub(super) modules: &'a [String],
    pub(super) checkpoint: Option<&'a Checkpoint>,
}

impl Request<'_> {
    pub(super) fn command(self, root: &Path) -> io::Result<Vec<OsString>> {
        let mut command = vec![
            executable(root)?.into_os_string(),
            "--no-build".into(),
            "--module-only".into(),
        ];
        if self.checkpoint.is_some() {
            command.push("--format=json".into());
        }
        match self.checkpoint {
            Some(Checkpoint::Proof {
                term_objects,
                branch_score,
                branch_depth,
            }) => {
                command.extend([
                    "--only=largeProofTermLinter,proofBranchingLinter".into(),
                    format!("--proof-term-objects={term_objects}").into(),
                    format!("--proof-branch-score={branch_score}").into(),
                    format!("--proof-branch-depth={branch_depth}").into(),
                ]);
            }
            None | Some(Checkpoint::Default(_)) => command.push("--deny=all".into()),
        }
        command.extend(self.modules.iter().map(OsString::from));
        Ok(command)
    }

    pub(super) fn record(self, body: &mut serde_json::Value, outcome: &NativeReaderResult) {
        if let Some(checkpoint) = self.checkpoint {
            let report = serde_json::from_str::<serde_json::Value>(&outcome.stdout);
            if !matches!(outcome.exit_code, Some(0 | 1))
                || !report.is_ok_and(|value| {
                    value
                        .get("findings")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|rows| rows.iter().all(serde_json::Value::is_object))
                })
            {
                *body = serde_json::json!({"version":1,"status":"checkpoint_lint_report_unavailable"});
                return;
            }
            body["checkpoint"] = serde_json::json!(checkpoint);
        }
        record_verdict(body, outcome);
    }
}

pub(super) fn architecture_command(root: &Path) -> io::Result<Vec<OsString>> {
    let executable = executable(root)?;
    let olean = root.join(BUILD_LIB_REL).join("Proofs.olean");
    if !olean.symlink_metadata()?.is_file() || olean.canonicalize()? != olean {
        return Err(invalid("canonical prebuilt Proofs root required"));
    }
    Ok(vec![executable.into_os_string(), "--no-build".into(), "Proofs".into()])
}

pub(super) fn record_architecture(body: &mut serde_json::Value, outcome: &NativeReaderResult) {
    body["output_truncated"] = matches!(
        outcome.diagnostic_preview,
        Some(super::super::DiagnosticPreview::Clipped)
    )
    .into();
    record_verdict(body, outcome);
}

fn record_verdict(body: &mut serde_json::Value, outcome: &NativeReaderResult) {
    body["verdict"] = match outcome.exit_code {
        Some(0) => "pass",
        Some(_) => "fail",
        None => "unavailable",
    }
    .into();
}

pub(super) fn valid_modules(modules: &[String]) -> bool {
    !modules.is_empty()
        && modules.len() <= MAX_NAMES
        && modules.iter().all(|module| {
            qualified_name(module)
                && Path::new(module)
                    .extension()
                    .is_none_or(|extension| extension != "lean")
        })
}

pub(super) fn executable(root: &Path) -> io::Result<std::path::PathBuf> {
    let executable = root.join(RUN_LINTER_REL);
    let meta = executable.metadata()?;
    if executable.canonicalize()? != executable || !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(invalid("canonical prebuilt runLinter required"));
    }
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_architecture_requires_prebuilt_and_preserves_default_severities() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        assert_eq!(architecture_command(&root).unwrap_err().kind(), io::ErrorKind::NotFound);
        let executable = root.join(RUN_LINTER_REL);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"fixture executable").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(architecture_command(&root).unwrap_err().kind(), io::ErrorKind::NotFound);
        let olean = root.join(BUILD_LIB_REL).join("Proofs.olean");
        std::fs::create_dir_all(olean.parent().unwrap()).unwrap();
        std::fs::write(olean, b"fixture artifact").unwrap();
        assert_eq!(
            architecture_command(&root).unwrap(),
            [executable.into_os_string(), "--no-build".into(), "Proofs".into()]
        );

        for (code, verdict) in [(0, "pass"), (1, "fail")] {
            let outcome = crate::native_reader::result(
                Some(crate::native_reader::PayloadResult {
                    target_started: true,
                    exit_code: Some(code),
                    exit_status: Some(format!("exit status: {code}")),
                    diagnostic_preview: Some(crate::native_reader::DiagnosticPreview::Clipped),
                    ..crate::native_reader::PayloadResult::default()
                }),
                true,
                None,
            );
            assert_eq!(outcome.state, "completed");
            let mut receipt = serde_json::json!({"exit_code": outcome.exit_code});
            record_architecture(&mut receipt, &outcome);
            assert_eq!(
                receipt,
                serde_json::json!({
                    "exit_code": code, "output_truncated": true, "verdict": verdict
                })
            );
        }
    }

    #[test]
    fn prebuilt_lint_uses_only_fixed_options_and_ordered_modules() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let modules = ["Z.Second".to_owned(), "A.First".to_owned()];
        assert!(valid_modules(&modules));
        assert!(!valid_modules(&["--allow=all".to_owned()]));
        assert!(!valid_modules(&["A.lean".to_owned()]));
        let request = Request {
            modules: &modules,
            checkpoint: None,
        };
        assert!(request.command(&root).is_err());

        let alternative = root.join("custom/bin/runLinter");
        std::fs::create_dir_all(alternative.parent().unwrap()).unwrap();
        std::fs::write(&alternative, b"fixture executable").unwrap();
        std::fs::set_permissions(&alternative, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(request.command(&root).is_err());

        let path = root.join(RUN_LINTER_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"fixture executable").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let args = request.command(&root).unwrap();
        assert_eq!(args[0], path.into_os_string());
        assert_eq!(
            args[1..],
            ["--no-build", "--module-only", "--deny=all", "Z.Second", "A.First"]
        );
        let checkpoint = Checkpoint::Proof {
            term_objects: 10,
            branch_score: 20,
            branch_depth: 30,
        };
        assert!(checkpoint.valid());
        let request = Request {
            modules: &modules[..1],
            checkpoint: Some(&checkpoint),
        };
        assert_eq!(
            request.command(&root).unwrap()[1..],
            [
                "--no-build",
                "--module-only",
                "--format=json",
                "--only=largeProofTermLinter,proofBranchingLinter",
                "--proof-term-objects=10",
                "--proof-branch-score=20",
                "--proof-branch-depth=30",
                "Z.Second"
            ],
        );
        let default = serde_json::from_str::<Checkpoint>(r#"{"kind":"default"}"#).unwrap();
        assert!(default.valid());
        assert_eq!(
            serde_json::to_value(default).unwrap(),
            serde_json::json!({"kind":"default"})
        );
        assert!(serde_json::from_str::<Checkpoint>(r#"{"kind":"default","flag":"--no-build"}"#).is_err());
    }
}
