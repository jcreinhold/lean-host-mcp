//! Fixed formatting and implicit preview under the admitted writer.
use std::{
    collections::BTreeMap,
    io::{self, Read as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::SourceIdentity;
use crate::toolchain::{ToolchainId, WindowVerdict};

const REPORT: &str = "formatter-report.json";
const REPORT_LIMIT: u64 = 64 * 1024 * 1024;
const APPROVAL_ENV: &str = "LEAN_HOST_MCP_MANAGED_FORMATTER_APPROVAL";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Approval {
    pub(super) sha256: [u8; 32],
    #[serde(default)]
    pub(super) preview: bool,
    dependency_revision: String,
    toolchain: String,
}

impl Approval {
    pub(super) fn select(root: &Path) -> io::Result<Self> {
        let encoded = std::env::var(APPROVAL_ENV).map_err(io::Error::other)?;
        if encoded.len() > 1024 {
            return Err(invalid("formatter approval exceeds limit"));
        }
        let approved: Self = serde_json::from_str(&encoded)?;
        let mut manifest = Vec::new();
        std::fs::File::open(root.join("lake-manifest.json"))?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut manifest)?;
        let value: serde_json::Value = serde_json::from_slice(&manifest)?;
        if manifest.len() > 1024 * 1024
            || value["packagesDir"] != ".lake/packages"
            || !value["packages"].as_array().is_some_and(|packages| {
                packages
                    .iter()
                    .filter(|entry| matches!(entry["name"].as_str(), Some("lean-fmt" | "«lean-fmt»")))
                    .count()
                    == 1
                    && packages.iter().any(|entry| {
                        matches!(entry["name"].as_str(), Some("lean-fmt" | "«lean-fmt»"))
                            && entry["type"] == "git"
                            && entry["rev"] == approved.dependency_revision
                            && entry["subDir"].is_null()
                    })
            })
            || approved.dependency_revision.len() != 40
            || !approved
                .dependency_revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(invalid("formatter package differs from its approval"));
        }
        let toolchain = ToolchainId::from_lake_root(root).map_err(io::Error::other)?;
        if toolchain.window_verdict() != WindowVerdict::Supported
            || ToolchainId::parse(&approved.toolchain).map_err(io::Error::other)? != toolchain
        {
            return Err(invalid("formatter toolchain differs from its approval"));
        }
        executable(root)?;
        Ok(approved)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn executable(root: &Path) -> io::Result<PathBuf> {
    let bin = root.join(".lake/packages/lean-fmt/.lake/build/bin/lean-fmt");
    let meta = bin.metadata()?;
    if bin.canonicalize()?.as_os_str() != bin.as_os_str() || !meta.is_file() || meta.mode() & 0o111 == 0 {
        return Err(invalid("approved formatter executable is missing or aliased"));
    }
    Ok(bin)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    formatter_sha256: [u8; 32],
    #[serde(default)]
    targets: Vec<SourceIdentity>,
    preview_input_sha256: Option<[u8; 32]>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    mode: String,
    validation_bypassed: u64,
    infrastructure_failures: Vec<serde_json::Value>,
    rejected: u64,
    written: usize,
    files: Vec<FileResult>,
    findings: u64,
    changed: usize,
    broken: usize,
    unbuilt: usize,
    verbatim_commands: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileResult {
    path: PathBuf,
    status: String,
    written: bool,
    formatted: Option<String>,
    validation_bypassed: u64,
    verbatim_commands: usize,
    findings: Vec<serde::de::IgnoredAny>,
}

impl Request {
    pub(super) fn valid(&self) -> bool {
        let mut unique = std::collections::BTreeSet::new();
        self.preview_input_sha256.is_none()
            && !self.targets.is_empty()
            && self.targets.len() <= 32
            && self.targets.iter().all(|source| {
                source.valid()
                    && source.file.as_os_str().len() <= 512
                    && source.file.starts_with("Proofs")
                    && source.file.to_str().is_some_and(|path| {
                        path.chars()
                            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '\'' | '.' | '/'))
                    })
                    && unique.insert(&source.file)
            })
    }

    pub(super) fn valid_preview(&self) -> bool {
        self.targets.is_empty() && self.preview_input_sha256.is_some()
    }

    pub(super) fn check_sources(&self, root: &Path) -> io::Result<()> {
        self.targets.iter().try_for_each(|source| source.check(root))
    }

    pub(super) fn command(&self, root: &Path, assets: &Path) -> io::Result<Command> {
        let approved = Approval::select(root)?;
        if approved.sha256 != self.formatter_sha256 || (self.valid_preview() && !approved.preview) {
            return Err(invalid("formatter approval differs from original intent"));
        }
        let bin = executable(root)?;
        let mut hash = Sha256::new();
        use rustix::fs::{Mode, OFlags, open};
        let file = std::fs::File::from(open(
            &bin,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let identity = file.metadata()?;
        if !identity.is_file() || identity.len() > 256 * 1024 * 1024 {
            return Err(invalid("formatter image exceeds allowed size"));
        }
        let mut input = file.take(256 * 1024 * 1024 + 1);
        let mut buffer = [0; 8192];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let named = std::fs::symlink_metadata(&bin)?;
        if input.limit() == 0
            || (named.dev(), named.ino()) != (identity.dev(), identity.ino())
            || hash.finalize().as_slice() != approved.sha256
        {
            return Err(invalid("approved formatter executable digest changed"));
        }
        let toolchain = ToolchainId::from_lake_root(root).map_err(io::Error::other)?;
        if toolchain.window_verdict() != WindowVerdict::Supported {
            return Err(invalid("unsupported formatter toolchain"));
        }
        let sysroot = toolchain.elan_dir().map_err(io::Error::other)?.canonicalize()?;
        let mut command = Command::new(bin);
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|value| value.starts_with("LEAN_FMT_")) {
                command.env_remove(name);
            }
        }
        let mode = if self.valid_preview() {
            ["format", "--check"]
        } else {
            ["check", "--fix"]
        };
        command
            .current_dir(root)
            .args(mode)
            .args([
                "--no-build",
                "--inherit-process-group",
                "--no-cache",
                "--workers",
                "2",
                "--json",
                "--output-file",
            ])
            .arg(assets.join(REPORT))
            .env("LEAN_SYSROOT", sysroot)
            .env("LEAN_NUM_THREADS", "2")
            .env("LEAN_FMT_CHILD_THREADS", "4")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("LAKE_CACHE_DIR", assets)
            .env("TMPDIR", assets);
        if let Some(digest) = self.preview_input_sha256 {
            command.arg("--admitted-preview").arg(hex_digest(digest));
        }
        for source in &self.targets {
            command
                .arg("--admitted-source")
                .arg(&source.file)
                .arg(hex_digest(source.sha256));
        }
        Ok(command)
    }

    pub(super) fn receipt(&self, root: &Path, assets: &Path, output: &Output) -> serde_json::Value {
        match self.validated_receipt(root, assets, output) {
            Ok(receipt) => receipt,
            Err(_) => serde_json::json!({
                "version":1,"stage":"formatter-fix","status":"unavailable",
                "reason":if output.status.code()==Some(2)
                    && String::from_utf8_lossy(&output.stderr).contains("source differs from its admitted digest:") {
                    "source_changed"
                } else {
                    "outcome_unknown"
                }
            }),
        }
    }

    pub(super) fn preview_receipt(&self, assets: &Path, output: &Output) -> serde_json::Value {
        let complete = || -> io::Result<serde_json::Value> {
            let (report, _) = read_report(assets)?;
            let selected = report.files.len();
            let mut unique = std::collections::BTreeSet::new();
            let mut changed = 0;
            let mut findings = 0;
            for row in &report.files {
                if row.written
                    || row.validation_bypassed != 0
                    || row.verbatim_commands != 0
                    || !unique.insert(&row.path)
                {
                    return Err(invalid("formatter preview file result is incomplete"));
                }
                match row.status.as_str() {
                    "clean" => (),
                    "would-format" => changed += 1,
                    _ => return Err(invalid("formatter preview file result is incomplete")),
                }
                findings += row.findings.len() as u64;
            }
            let exit = i32::from(changed != 0);
            if report.mode != "format"
                || report.validation_bypassed != 0
                || report.verbatim_commands != 0
                || report.written != 0
                || report.broken != 0
                || report.unbuilt != 0
                || report.rejected != 0
                || !report.infrastructure_failures.is_empty()
                || selected > 16384
                || report.changed != changed
                || report.findings != findings
                || output.status.code() != Some(exit)
            {
                return Err(invalid("formatter preview has no complete result"));
            }
            Ok(serde_json::json!({
                "version":1, "stage":"formatter-preview", "status":"complete",
                "verdict":if changed == 0 {"clean"} else {"would_format"},
                "selected_count":selected, "finding_count":findings,
                "preview_input_sha256":self.preview_input_sha256,
            }))
        };
        complete().unwrap_or_else(|_| {
            let changed = output.status.code() == Some(2)
                && String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .any(|line| line.trim() == "lean-fmt: formatter preview inputs differ from the admitted digest");
            serde_json::json!({
                "version":1, "stage":"formatter-preview", "status":"unavailable",
                "reason":if changed {"inputs_changed"} else {"outcome_incomplete"},
                "preview_input_sha256":self.preview_input_sha256,
            })
        })
    }

    fn validated_receipt(&self, root: &Path, assets: &Path, output: &Output) -> io::Result<serde_json::Value> {
        let Some(exit @ (0 | 1)) = output.status.code() else {
            return Err(invalid("formatter has no complete result"));
        };
        let (report, report_sha256) = read_report(assets)?;
        if report.mode != "fix"
            || report.validation_bypassed != 0
            || report.rejected != 0
            || !report.infrastructure_failures.is_empty()
            || report.files.len() != self.targets.len()
        {
            return Err(invalid("formatter report did not establish exact finite validation"));
        }
        let mut rows = BTreeMap::new();
        for row in report.files {
            if rows.insert(row.path.clone(), row).is_some() {
                return Err(invalid("duplicate formatter report file"));
            }
        }
        let mut files = Vec::with_capacity(self.targets.len());
        let mut written = 0;
        for source in &self.targets {
            let row = rows
                .remove(&source.file)
                .ok_or_else(|| invalid("admitted formatter file missing"))?;
            let after = source.digest(root)?;
            let expected_after: [u8; 32] = match (row.written, row.formatted) {
                (true, Some(bytes)) => Sha256::digest(bytes.as_bytes()).into(),
                (false, None) => source.sha256,
                _ => return Err(invalid("formatter report lacks exact publication bytes")),
            };
            if after != expected_after
                || row.validation_bypassed != 0
                || row.status != if row.written { "fixed" } else { "clean" }
                || row.written != (source.sha256 != after)
            {
                return Err(invalid("formatter report disagrees with source bytes"));
            }
            written += usize::from(row.written);
            files.push(serde_json::json!({
                "path":source.file, "before_sha256":source.sha256, "after_sha256":after,
                "written":row.written, "status":row.status,
            }));
        }
        if written != report.written {
            return Err(invalid("formatter write count differs"));
        }
        Ok(serde_json::json!({
            "version":1,"stage":"formatter-fix","exit_code":exit,"formatter_sha256":self.formatter_sha256,
            "report_sha256":report_sha256,"files":files,
        }))
    }
}

fn hex_digest(digest: [u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn read_report(assets: &Path) -> io::Result<(Report, [u8; 32])> {
    use rustix::fs::{Mode, OFlags, open};
    let file = std::fs::File::from(open(
        assets.join(REPORT),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.len() > REPORT_LIMIT
    {
        return Err(invalid("formatter report is not bounded private data"));
    }
    let mut bytes = Vec::new();
    file.take(REPORT_LIMIT + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(io::Error::other)? > REPORT_LIMIT {
        return Err(invalid("formatter report exceeds limit"));
    }
    let report = serde_json::from_slice(&bytes)?;
    Ok((report, Sha256::digest(&bytes).into()))
}
