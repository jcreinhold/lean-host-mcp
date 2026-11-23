use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use super::{
    DEFAULT_LIMIT, MAX_LIMIT, MAX_SEARCHES, ProofPositionSelector, ProofSearchMode, SearchForProofRequest,
    plan_searches, project_declaration_search, rank_results, session_imports, source_profile,
};
use crate::error::{Result, ServerError};
use crate::writer_setup_verification::capture;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmittedSearch {
    declaration: String,
    #[serde(default)]
    proof_position: ProofPositionSelector,
    goal: Option<String>,
    type_text: Option<String>,
    #[serde(default)]
    imports: Vec<String>,
    #[serde(default)]
    mode: ProofSearchMode,
    limit: usize,
    pub(crate) runtime_sha256: [u8; 32],
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeSelection {
    pub(crate) manifest: PathBuf,
    pub(crate) image: PathBuf,
    pub(crate) sha256: [u8; 32],
}

impl RuntimeSelection {
    pub(crate) fn configured() -> Result<Self> {
        let raw = std::env::var("LEAN_HOST_MCP_PREBUILT_SEMANTIC_RUNTIME")
            .map_err(|_| ServerError::BadProject("no operator-approved semantic runtime".into()))?;
        if raw.len() > 4096 {
            return Err(ServerError::BadProject("invalid semantic selection size".into()));
        }
        serde_json::from_str(&raw).map_err(|error| ServerError::BadProject(error.to_string()))
    }

    pub(crate) fn package(&self) -> Result<&Path> {
        self.manifest
            .ancestors()
            .find(|path| path.file_name().is_some_and(|name| name == ".lake"))
            .and_then(Path::parent)
            .filter(|root| root.is_absolute() && root.canonicalize().is_ok_and(|actual| actual == *root))
            .ok_or_else(|| ServerError::BadProject("semantic runtime is outside its package".into()))
    }
}

impl AdmittedSearch {
    pub(crate) fn valid(&self) -> bool {
        let text = |value: &str| !value.contains('\0') && value.len() <= 4096;
        !self.declaration.trim().is_empty()
            && self.declaration.len() <= 256
            && text(&self.declaration)
            && (1..=MAX_LIMIT).contains(&self.limit)
            && self.goal.as_deref().is_none_or(text)
            && self.type_text.as_deref().is_none_or(text)
            && self.imports.len() <= 32
            && self
                .imports
                .iter()
                .all(|name| !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control))
            && match &self.proof_position {
                ProofPositionSelector::Default | ProofPositionSelector::Index { .. } => true,
                ProofPositionSelector::AfterText { text: fragment, .. } => !fragment.is_empty() && text(fragment),
            }
    }

    pub(super) fn from_public(req: &SearchForProofRequest, runtime_sha256: [u8; 32]) -> Result<Self> {
        let admitted = Self {
            declaration: req.declaration.clone().unwrap_or_default(),
            proof_position: req.proof_position.clone(),
            goal: req.goal.clone(),
            type_text: req.type_text.clone(),
            imports: req.imports.clone(),
            mode: req.mode.unwrap_or_default(),
            limit: req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            runtime_sha256,
        };
        if !admitted.valid() {
            return Err(ServerError::BadProject(
                "managed search needs a bounded file and declaration".into(),
            ));
        }
        Ok(admitted)
    }

    pub(crate) fn execute(
        &self,
        root: &Path,
        input: &crate::tools::source_input::QueryFile,
        shims: &crate::broker::verify_file::VerifierShims,
        assets: &Path,
        project_domains: &[PathBuf],
        bootstrap: PathBuf,
        check: impl Fn() -> io::Result<()>,
    ) -> io::Result<Vec<u8>> {
        let explicit_text = [self.goal.as_deref(), self.type_text.as_deref()]
            .into_iter()
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let profile = source_profile(
            root,
            input,
            &self.declaration,
            &self.proof_position,
            &self.imports,
            explicit_text,
            self.limit,
        )
        .map_err(io::Error::other)?;
        let semantic = profile
            .semantic
            .as_ref()
            .ok_or_else(|| io::Error::other("missing semantic request"))?;
        let mut roots: Vec<_> = project_domains
            .iter()
            .map(|path| path.join(".lake/build/lib/lean"))
            .collect();
        roots.push(shims.sysroot.join("lib/lean"));
        let semantic_driver = [
            include_str!("../../lake/SemanticFeatures.lean"),
            include_str!("../../lake/SemanticSetup.lean"),
        ]
        .concat();
        let semantic_raw = run_driver(
            assets,
            "Semantic",
            &semantic_driver,
            &semantic.reader_input(&roots).map_err(io::Error::other)?,
            &shims.sysroot,
            &[bootstrap],
            &check,
        )?;
        let semantic_result = semantic.reader_result(&semantic_raw).map_err(io::Error::other)?;
        let searches = plan_searches(&profile, self.mode);
        let planned = searches.iter().take(MAX_SEARCHES).collect::<Vec<_>>();
        let lexical = if planned.is_empty() {
            Vec::new()
        } else {
            let query = serde_json::to_vec(&serde_json::json!({
                "roots": roots,
                "sourceRoots": project_domains,
                "imports": session_imports(profile.imports.clone()),
                "queries": planned.iter().map(|query| &query.request).collect::<Vec<_>>(),
            }))?;
            let bytes = run_driver(
                assets,
                "Lexical",
                include_str!("../../lake/SemanticLexical.lean"),
                &query,
                &shims.sysroot,
                &shims.paths,
                &check,
            )?;
            let rows: Vec<lean_rs_worker_parent::LeanWorkerDeclarationSearchResult> = serde_json::from_slice(&bytes)?;
            if rows.len() != planned.len() {
                return Err(io::Error::other("incomplete staged lexical result"));
            }
            planned
                .iter()
                .map(|query| query.label)
                .zip(rows.into_iter().map(project_declaration_search))
                .collect()
        };
        let search_count = lexical.len().saturating_add(1);
        let outcome = rank_results(
            &profile,
            self.mode,
            self.limit,
            search_count,
            Some(semantic_result),
            lexical,
            profile.warnings.clone(),
            true,
        );
        Ok(serde_json::to_vec(&outcome)?)
    }
}

fn run_driver(
    assets: &Path,
    name: &str,
    program: &str,
    input: &[u8],
    sysroot: &Path,
    paths: &[PathBuf],
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    let driver = assets.join(format!("{name}.lean"));
    let request = assets.join(format!("{name}.json"));
    fs::write(&driver, program)?;
    fs::write(&request, input)?;
    check()?;
    capture(
        Command::new(sysroot.join("bin/lean"))
            .current_dir(assets)
            .args(["-j1", "--run"])
            .arg(driver)
            .arg(request)
            .env("LEAN_SYSROOT", sysroot)
            .env("LEAN_PATH", std::env::join_paths(paths).map_err(io::Error::other)?),
        1024 * 1024,
    )
}
