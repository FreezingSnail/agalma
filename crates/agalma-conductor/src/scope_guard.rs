//! SF.6 — candidate file scope and immutable protected verification inputs.
//!
//! Per `docs/sf6-protected-inputs.md` (human decision, 2026-10-15):
//!
//! - The **admission source** is the operator-supplied
//!   `$XDG_CONFIG_HOME/agalma/protected-inputs.json` (`ProtectedInputs/v1`),
//!   falling back to `$HOME/.config/agalma/protected-inputs.json` when
//!   `XDG_CONFIG_HOME` is unset, empty, or relative. It stays outside candidate
//!   writable roots. A repository file, task text, absent manifest, or empty
//!   scope never authorizes an unprotected repository execution; missing
//!   required inputs stop repository work. There is deliberately **no** fallback
//!   to a state-directory manifest and no policy merging. M0 fixture mode
//!   (`repo == None`) is a separate path.
//! - Each entry binds `task_id`, `repository_root` (canonical origin path),
//!   `base_sha`, `allowed_paths`, `protected_paths`, and a `baseline`. The
//!   **complete selected entry** is frozen as the package's `scope.oracle`
//!   artifact; its allowed/protected fields populate the package scope and
//!   agreement is validated on load. Builders cannot read the artifact.
//! - The `ProtectedBaseline/v1` has nonempty `files` and `commands`. Files are
//!   restored into a **separate read-only root outside every child writable
//!   root**; commands are immutable strings run through the existing sandbox.
//!
//! This module is pure policy plus bounded file inspection and materialization;
//! it opens no database and discovers no peer sockets. It reuses the canonical
//! SHA-256 helper and `FailureDetail/v1` codes.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use agalma_contracts::digest::{sha256_digest, FailureCode};
use agalma_contracts::{ChangeKind, ContractError, FileChange, Scope};
use serde::{Deserialize, Serialize};

/// The protected baseline manifest format.
pub const PROTECTED_BASELINE_FORMAT: &str = "ProtectedBaseline/v1";
/// The operator protected-inputs manifest format.
pub const PROTECTED_INPUTS_FORMAT: &str = "ProtectedInputs/v1";
/// The operator manifest file under the XDG config base.
pub const PROTECTED_INPUTS_FILE: &str = "protected-inputs.json";
/// The application directory under the XDG config base.
pub const PROTECTED_CONFIG_DIR: &str = "agalma";

/// A path segment that marks a nested repository (never an authorized change).
const NESTED_REPO_SEGMENT: &str = ".git";

/// The only regular-file modes the bootstrap baseline representation supports.
const SUPPORTED_MODES: [&str; 2] = ["100644", "100755"];

/// Why a policy entry was refused (no silent discard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError {
    pub entry: String,
    pub detail: String,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid scope entry {:?}: {}", self.entry, self.detail)
    }
}

/// The frozen allow/deny policy derived from a package [`Scope`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopePolicy {
    allowed: Vec<String>,
    protected: Vec<String>,
}

/// Why one candidate path was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeniedChange {
    pub path: String,
    pub code: String,
    pub detail: String,
}

/// The outcome of authorizing a candidate change set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeReport {
    pub enforced: bool,
    pub authorized: usize,
    pub denied: Vec<DeniedChange>,
}

impl ScopeReport {
    pub fn is_denied(&self) -> bool {
        !self.denied.is_empty()
    }
}

impl ScopePolicy {
    /// Derive the policy from a frozen [`Scope`]. Every recorded entry is
    /// validated; an invalid entry is refused, never dropped.
    pub fn from_scope(scope: &Scope) -> Result<Self, PolicyError> {
        Ok(ScopePolicy {
            allowed: normalize_entries(&scope.allowed_paths)?,
            protected: normalize_entries(&scope.protected_paths)?,
        })
    }

    /// Whether any policy entry is recorded.
    pub fn enforced(&self) -> bool {
        !self.allowed.is_empty() || !self.protected.is_empty()
    }

    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }
    pub fn protected(&self) -> &[String] {
        &self.protected
    }

    /// Authorize `changes` by recorded path only (no filesystem access).
    pub fn authorize(&self, changes: &[FileChange]) -> ScopeReport {
        self.authorize_with(changes, None)
    }

    /// Authorize `changes` against the pinned candidate tree `root`, resolving
    /// every affected path through the real filesystem so symlink escapes —
    /// including through an unchanged base symlink or a chain — are refused.
    pub fn authorize_in_tree(&self, changes: &[FileChange], root: &Path) -> ScopeReport {
        self.authorize_with(changes, Some(root))
    }

    fn authorize_with(&self, changes: &[FileChange], root: Option<&Path>) -> ScopeReport {
        // An unenforced (empty) policy authorizes nothing on its own; the
        // caller refuses repository work with no policy. Here it reports the
        // explicit unenforced state with no denials.
        if !self.enforced() {
            return ScopeReport {
                enforced: false,
                authorized: changes.len(),
                denied: Vec::new(),
            };
        }
        let mut authorized = 0usize;
        let mut denied: Vec<DeniedChange> = Vec::new();
        for change in changes {
            match self.authorize_one(change, root) {
                Ok(()) => authorized += 1,
                Err(denial) => denied.push(denial),
            }
        }
        denied.sort_by(|a, b| a.path.cmp(&b.path).then(a.code.cmp(&b.code)));
        ScopeReport {
            enforced: true,
            authorized,
            denied,
        }
    }

    pub fn authorize_one(
        &self,
        change: &FileChange,
        root: Option<&Path>,
    ) -> Result<(), DeniedChange> {
        // A gitlink (mode 160000) is a nested repository/submodule. Git does not
        // materialize a component `.git` for it, so the `.git` segment test can
        // miss it; fail closed on the mode itself.
        if is_nested_repo_mode(change) {
            return Err(DeniedChange {
                path: change.path.clone(),
                code: "nested_repository".to_string(),
                detail: "gitlink (mode 160000) nested repository/submodule is not authorized"
                    .to_string(),
            });
        }
        self.check_path(&change.path, Side::Destination)?;
        if matches!(change.kind, ChangeKind::Renamed | ChangeKind::Deleted) {
            if let Some(old_path) = &change.old_path {
                self.check_path(old_path, Side::Source)?;
            }
        }
        if let Some(root) = root {
            check_symlink_escape(root, &change.path).map_err(|detail| DeniedChange {
                path: change.path.clone(),
                code: "symlink_escape".to_string(),
                detail,
            })?;
        }
        Ok(())
    }

    fn check_path(&self, raw: &str, side: Side) -> Result<(), DeniedChange> {
        let path = match normalize_path(raw) {
            Ok(path) => path,
            Err(detail) => {
                return Err(DeniedChange {
                    path: raw.to_string(),
                    code: "invalid_path".to_string(),
                    detail,
                })
            }
        };
        if path
            .split('/')
            .any(|segment| segment == NESTED_REPO_SEGMENT)
        {
            return Err(DeniedChange {
                path: path.clone(),
                code: "nested_repository".to_string(),
                detail: format!("`{path}` is inside a nested repository/generated tree"),
            });
        }
        if matches_entry(&self.protected, &path) {
            return Err(DeniedChange {
                path: path.clone(),
                code: "protected".to_string(),
                detail: format!("`{path}` is a protected path"),
            });
        }
        if matches_entry(&self.allowed, &path) {
            return Ok(());
        }
        let code = match side {
            Side::Source => "rename_source",
            Side::Destination => "not_allowed",
        };
        Err(DeniedChange {
            path: path.clone(),
            code: code.to_string(),
            detail: format!("`{path}` is outside the admitted scope"),
        })
    }
}

#[derive(Clone, Copy)]
enum Side {
    Source,
    Destination,
}

fn normalize_entries(entries: &[String]) -> Result<Vec<String>, PolicyError> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let normalized = normalize_path(entry).map_err(|detail| PolicyError {
            entry: entry.clone(),
            detail,
        })?;
        out.push(normalized);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn matches_entry(entries: &[String], path: &str) -> bool {
    entries
        .iter()
        .any(|entry| path == entry || path.starts_with(&format!("{entry}/")))
}

/// Normalize a repository-relative path, refusing absolutes, `..`, `.`, empty
/// segments, and NUL. Recorded bytes are preserved exactly (meaningful
/// whitespace in a filename is part of the identity).
pub fn normalize_path(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("empty path".to_string());
    }
    if raw.contains('\0') {
        return Err("path contains a NUL byte".to_string());
    }
    if raw.starts_with('/') {
        return Err(format!("`{raw}` is an absolute path"));
    }
    for segment in raw.split('/') {
        match segment {
            "" => return Err(format!("`{raw}` has an empty path segment")),
            "." => return Err(format!("`{raw}` contains a `.` segment")),
            ".." => return Err(format!("`{raw}` escapes the repository via `..`")),
            _ => {}
        }
    }
    Ok(raw.to_string())
}

/// Resolve `rel` against the pinned candidate tree `root`, **component by
/// component**, following symlinks (including dangling and chained targets) and
/// refusing any resolution that leaves the repository root.
///
/// A bare `canonicalize(full)` cannot judge a symlink whose target does not
/// exist (e.g. `src/escape -> /outside/nonexistent`), and falling back to the
/// nearest canonical ancestor would wrongly pass it because `src` is internal.
/// This walks each component instead: a symlink target is resolved relative to
/// its own directory (or from `/` when absolute), its `..`/`.` segments are
/// applied lexically, and the resolution is refused when it is outside the root
/// or the chain is too deep/cyclic.
fn check_symlink_escape(root: &Path, rel: &str) -> Result<(), String> {
    let root_real = std::fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve candidate root {}: {e}", root.display()))?;
    let normalized = normalize_path(rel).map_err(|detail| format!("`{rel}`: {detail}"))?;
    let parts: Vec<&str> = normalized.split('/').collect();
    let resolved = resolve_components(&root_real, &root_real, &parts, 0)?;
    if resolved.starts_with(&root_real) {
        Ok(())
    } else {
        Err(format!(
            "`{normalized}` resolves to {} outside the repository",
            resolved.display()
        ))
    }
}

/// Walk `parts` under `base`, following symlinks, returning the lexical result.
fn resolve_components(
    root_real: &Path,
    base: &Path,
    parts: &[&str],
    depth: u32,
) -> Result<PathBuf, String> {
    if depth > 40 {
        return Err("symlink chain too deep or cyclic".to_string());
    }
    let mut cur = base.to_path_buf();
    let mut i = 0;
    while i < parts.len() {
        let comp = parts[i];
        i += 1;
        match comp {
            "" | "." => continue,
            ".." => {
                cur.pop();
                continue;
            }
            _ => {}
        }
        let next = cur.join(comp);
        if let Ok(meta) = std::fs::symlink_metadata(&next) {
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&next)
                    .map_err(|e| format!("cannot read symlink {}: {e}", next.display()))?;
                let (base2, target_path) = if target.is_absolute() {
                    (PathBuf::from("/"), target)
                } else {
                    (cur.clone(), target)
                };
                let mut chain: Vec<String> = Vec::new();
                for c in target_path.components() {
                    use std::path::Component::{CurDir, Normal, ParentDir, Prefix, RootDir};
                    match c {
                        RootDir | Prefix(_) | CurDir => {}
                        ParentDir => chain.push("..".to_string()),
                        Normal(s) => chain.push(s.to_string_lossy().into_owned()),
                    }
                }
                chain.extend(parts[i..].iter().map(|s| s.to_string()));
                let chain_refs: Vec<&str> = chain.iter().map(String::as_str).collect();
                return resolve_components(root_real, &base2, &chain_refs, depth + 1);
            }
        }
        cur = next;
    }
    Ok(cur)
}

/// One immutable protected baseline file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineFile {
    pub path: String,
    pub mode: String,
    pub digest: String,
    pub bytes: u64,
    pub text: String,
}

impl BaselineFile {
    /// Validate the mode, path, and inline-bytes digest/byte length.
    pub fn validate(&self) -> Result<(), ContractError> {
        if !SUPPORTED_MODES.contains(&self.mode.as_str()) {
            return Err(FailureCode::InvalidRecord.failure(
                &self.path,
                format!("unsupported protected file mode {:?}", self.mode),
            ));
        }
        normalize_path(&self.path).map_err(|detail| {
            FailureCode::InvalidRecord
                .failure(&self.path, format!("invalid protected path: {detail}"))
        })?;
        let actual = sha256_digest(self.text.as_bytes());
        if actual != self.digest || self.text.len() as u64 != self.bytes {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.path,
                format!("protected file bytes do not match {actual}"),
            ));
        }
        Ok(())
    }
}

/// The immutable protected baseline (`ProtectedBaseline/v1`).
///
/// This is an implementation representation of the accepted protected-baseline
/// design, not an assertion framework: the files are the protected inputs, and
/// the frozen `commands` (run through the sandbox) are the behavioral judge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedBaseline {
    pub format: String,
    pub files: Vec<BaselineFile>,
    pub commands: Vec<String>,
}

impl ProtectedBaseline {
    /// Validate a fully-parsed baseline: format, nonempty files/commands,
    /// supported modes, no duplicate/conflicting paths, verified digests, and
    /// nonempty commands.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.format != PROTECTED_BASELINE_FORMAT {
            return Err(FailureCode::UnsupportedSchema.failure(
                "scope/oracle",
                format!("unsupported protected baseline format {:?}", self.format),
            ));
        }
        if self.files.is_empty() {
            return Err(FailureCode::ScopeDenied.failure(
                "scope/oracle",
                "a protected baseline must list at least one file",
            ));
        }
        if self.commands.is_empty() || self.commands.iter().any(|c| c.trim().is_empty()) {
            return Err(FailureCode::ScopeDenied.failure(
                "scope/oracle",
                "a protected baseline must list at least one non-empty command",
            ));
        }
        // Reject duplicate *normalized* destinations and file/ancestor
        // conflicts (a protected file that is also a directory prefix of
        // another), preserving filename whitespace.
        let mut normalized: Vec<String> = Vec::with_capacity(self.files.len());
        for file in &self.files {
            file.validate()?;
            let path = normalize_path(&file.path)
                .map_err(|detail| FailureCode::InvalidRecord.failure(&file.path, detail))?;
            if normalized.iter().any(|existing| existing == &path) {
                return Err(FailureCode::IdentityConflict.failure(
                    "scope/oracle",
                    format!("duplicate normalized protected file path {:?}", file.path),
                ));
            }
            if normalized
                .iter()
                .any(|existing| is_ancestor(existing, &path) || is_ancestor(&path, existing))
            {
                return Err(FailureCode::IdentityConflict.failure(
                    "scope/oracle",
                    format!(
                        "protected file {:?} conflicts with a directory ancestor/descendant",
                        file.path
                    ),
                ));
            }
            normalized.push(path);
        }
        Ok(())
    }
}

/// Whether a change involves a gitlink (mode `160000`) nested repository.
fn is_nested_repo_mode(change: &FileChange) -> bool {
    change.new_mode.as_deref() == Some("160000") || change.old_mode.as_deref() == Some("160000")
}

/// Whether `a` is a strict directory ancestor of `b`.
fn is_ancestor(a: &str, b: &str) -> bool {
    b.len() > a.len() + 1 && b.starts_with(a) && b.as_bytes()[a.len()] == b'/'
}

/// One operator-supplied protected entry (frozen whole as `scope.oracle`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedEntry {
    pub task_id: String,
    pub repository_root: String,
    pub base_sha: String,
    pub allowed_paths: Vec<String>,
    pub protected_paths: Vec<String>,
    pub baseline: ProtectedBaseline,
}

impl ProtectedEntry {
    /// Validate the entry's identity, paths, and baseline.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.task_id.trim().is_empty()
            || self.repository_root.trim().is_empty()
            || self.base_sha.trim().is_empty()
        {
            return Err(FailureCode::InvalidRecord.failure(
                "protected entry",
                "task_id, repository_root and base_sha must be non-empty",
            ));
        }
        let scope = Scope {
            allowed_paths: self.allowed_paths.clone(),
            protected_paths: self.protected_paths.clone(),
            readable_artifacts: Vec::new(),
            oracle: crate::scope_guard::dummy_artifact(),
        };
        ScopePolicy::from_scope(&scope).map_err(|err| {
            FailureCode::ScopeDenied
                .failure("protected entry", format!("invalid scope entry: {err}"))
        })?;
        self.baseline.validate()
    }

    /// The canonical frozen bytes for `scope.oracle`.
    pub fn freeze(&self) -> Result<Vec<u8>, ContractError> {
        serde_json::to_vec(self).map_err(|e| {
            FailureCode::InvalidRecord.failure(&self.task_id, format!("cannot freeze entry: {e}"))
        })
    }

    /// Parse a frozen `scope.oracle` artifact back into an entry.
    pub fn parse(bytes: &[u8]) -> Result<ProtectedEntry, ContractError> {
        let entry: ProtectedEntry = serde_json::from_slice(bytes).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                "scope/oracle",
                format!("frozen protected entry is malformed: {e}"),
            )
        })?;
        entry.validate()?;
        Ok(entry)
    }
}

/// The operator manifest (`ProtectedInputs/v1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedInputs {
    pub format: String,
    pub entries: Vec<ProtectedEntry>,
}

impl ProtectedInputs {
    /// Parse and fully validate the manifest: format, duplicate task entries,
    /// invalid paths, and baseline integrity. Any invalidity refuses the whole
    /// policy.
    pub fn parse(bytes: &[u8]) -> Result<ProtectedInputs, ContractError> {
        let inputs: ProtectedInputs = serde_json::from_slice(bytes).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                PROTECTED_INPUTS_FILE,
                format!("malformed protected inputs: {e}"),
            )
        })?;
        if inputs.format != PROTECTED_INPUTS_FORMAT {
            return Err(FailureCode::UnsupportedSchema.failure(
                PROTECTED_INPUTS_FILE,
                format!("unsupported format {:?}", inputs.format),
            ));
        }
        if inputs.entries.is_empty() {
            return Err(FailureCode::ScopeDenied.failure(
                PROTECTED_INPUTS_FILE,
                "at least one protected entry is required",
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for entry in &inputs.entries {
            entry.validate()?;
            if seen.contains(&entry.task_id.as_str()) {
                return Err(FailureCode::IdentityConflict.failure(
                    PROTECTED_INPUTS_FILE,
                    format!("duplicate protected entry for task {:?}", entry.task_id),
                ));
            }
            seen.push(entry.task_id.as_str());
        }
        Ok(inputs)
    }

    /// Load the manifest from an explicit directory. This is the low-level
    /// fixture/file loader; production admission uses [`load_config`] so a
    /// state-directory manifest can never stand in for operator config.
    pub fn load(dir: &Path) -> Result<ProtectedInputs, ContractError> {
        load_config_file(&dir.join(PROTECTED_INPUTS_FILE))
    }

    /// Select the entry for `task_id`, verifying repository/base identity. A
    /// missing entry or a mismatched identity refuses.
    pub fn select(
        &self,
        task_id: &str,
        repository_root: &str,
        base_sha: &str,
    ) -> Result<&ProtectedEntry, ContractError> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.task_id == task_id)
            .ok_or_else(|| {
                FailureCode::ScopeDenied.failure(
                    task_id,
                    "no operator protected entry for this task; refusing repository work",
                )
            })?;
        if entry.repository_root != repository_root {
            return Err(FailureCode::ScopeDenied.failure(
                task_id,
                format!(
                    "protected entry repository_root {:?} does not match {:?}",
                    entry.repository_root, repository_root
                ),
            ));
        }
        if entry.base_sha != base_sha {
            return Err(FailureCode::ScopeDenied.failure(
                task_id,
                format!(
                    "protected entry base_sha {:?} does not match resolved base {:?}",
                    entry.base_sha, base_sha
                ),
            ));
        }
        Ok(entry)
    }
}

/// Resolve the operator protected-inputs path from explicit environment values.
///
/// Precedence follows standard XDG absolute-directory semantics:
///
/// 1. `XDG_CONFIG_HOME` when it is non-empty, valid Unicode, and absolute;
///    a valid absolute value needs no `HOME`.
/// 2. `HOME/.config` when `XDG_CONFIG_HOME` is unset, empty, or relative.
///
/// A non-Unicode `XDG_CONFIG_HOME` or `HOME`, a relative `HOME`, an empty
/// `HOME`, or no usable base at all fails closed instead of guessing a path.
/// The returned path is absolute in every successful case.
pub fn resolve_config_path_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, ContractError> {
    if let Some(xdg) = xdg_config_home.filter(|value| !value.is_empty()) {
        let xdg = xdg.to_str().ok_or_else(|| {
            protected_config_error(
                "XDG_CONFIG_HOME is not valid Unicode; refusing to guess a protected-inputs path",
            )
        })?;
        let base = Path::new(xdg);
        if base.is_absolute() {
            return Ok(base
                .join(PROTECTED_CONFIG_DIR)
                .join(PROTECTED_INPUTS_FILE));
        }
        // A relative XDG base is not usable; standard semantics fall back to
        // HOME/.config rather than interpreting it against the current cwd.
    }

    let home = home.ok_or_else(|| {
        protected_config_error(
            "neither an absolute XDG_CONFIG_HOME nor HOME is set; refusing repository work",
        )
    })?;
    let home = home.to_str().ok_or_else(|| {
        protected_config_error(
            "HOME is not valid Unicode; refusing to guess a protected-inputs path",
        )
    })?;
    if home.is_empty() {
        return Err(protected_config_error(
            "HOME is empty; refusing repository work without a usable config base",
        ));
    }
    let base = Path::new(home);
    if !base.is_absolute() {
        return Err(protected_config_error(
            "HOME is not an absolute path; refusing repository work without a usable config base",
        ));
    }
    Ok(base
        .join(".config")
        .join(PROTECTED_CONFIG_DIR)
        .join(PROTECTED_INPUTS_FILE))
}

/// Resolve the operator protected-inputs path from the process environment.
pub fn resolve_config_path() -> Result<PathBuf, ContractError> {
    resolve_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Production admission loader: read the selected operator config file.
///
/// Exactly one file is chosen by [`resolve_config_path`]; a missing, unreadable,
/// or malformed file refuses even when a legacy state-directory manifest exists.
/// No policy merging and no legacy fallback occur.
pub fn load_config() -> Result<ProtectedInputs, ContractError> {
    load_config_file(&resolve_config_path()?)
}

/// Testable config loader with explicit XDG/HOME inputs (never mutates process
/// environment).
pub fn load_config_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<ProtectedInputs, ContractError> {
    load_config_file(&resolve_config_path_from(xdg_config_home, home)?)
}

/// Read and validate an exact protected-inputs file path.
pub fn load_config_file(path: &Path) -> Result<ProtectedInputs, ContractError> {
    let bytes = std::fs::read(path).map_err(|e| {
        FailureCode::ScopeDenied.failure(
            PROTECTED_INPUTS_FILE,
            format!(
                "no readable operator protected inputs at {}; refusing repository work: {e}",
                path.display()
            ),
        )
    })?;
    ProtectedInputs::parse(&bytes)
}

fn protected_config_error(detail: impl AsRef<str>) -> ContractError {
    FailureCode::ScopeDenied.failure(PROTECTED_INPUTS_FILE, detail)
}

/// A dummy artifact used only to build a [`Scope`] for entry validation.
pub fn dummy_artifact() -> agalma_contracts::PackageArtifactRef {
    agalma_contracts::PackageArtifactRef {
        path: String::new(),
        digest: String::new(),
        bytes: 0,
        media_type: String::new(),
    }
}

/// Read a frozen protected entry from a package `scope.oracle` artifact.
pub fn entry_from_artifact(
    state_dir: &Path,
    artifact: &agalma_contracts::PackageArtifactRef,
) -> Result<(Vec<u8>, ProtectedEntry), ContractError> {
    let bytes = crate::task_package::read_artifact(state_dir, artifact)?;
    let entry = ProtectedEntry::parse(&bytes)?;
    Ok((bytes, entry))
}

/// Restore the protected baseline files into `target` (a root that must be
/// outside every child writable root). Digests are re-verified.
pub fn materialize_baseline(
    target: &Path,
    baseline: &ProtectedBaseline,
) -> Result<PathBuf, ContractError> {
    baseline.validate()?;
    std::fs::create_dir_all(target).map_err(|e| {
        FailureCode::StateUnavailable.failure(
            target.display().to_string(),
            format!("cannot create protected root: {e}"),
        )
    })?;
    for file in &baseline.files {
        let path = target.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                FailureCode::StateUnavailable
                    .failure(&file.path, format!("cannot create protected dir: {e}"))
            })?;
        }
        std::fs::write(&path, file.text.as_bytes()).map_err(|e| {
            FailureCode::StateUnavailable
                .failure(&file.path, format!("cannot restore protected file: {e}"))
        })?;
        // Honor the recorded supported regular-file mode so a declared
        // executable protected runner (100755) can be launched directly.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = match file.mode.as_str() {
                "100755" => 0o755,
                _ => 0o644,
            };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).map_err(
                |e| {
                    FailureCode::StateUnavailable
                        .failure(&file.path, format!("cannot set protected file mode: {e}"))
                },
            )?;
        }
    }
    Ok(target.to_path_buf())
}

/// The immutable identity pinned at verification (SF.6 amendment).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePin {
    /// The execution identity.
    pub execution_id: String,
    /// The execution generation (lease generation).
    pub generation: u32,
    /// The frozen package digest.
    pub package_digest: String,
    /// The successful verification attempt.
    pub attempt: u32,
    /// The verified candidate SHA.
    pub candidate_sha: String,
    /// The held-out oracle digest.
    pub oracle_digest: String,
}

/// The current identity observed at the integration boundary.
pub struct PinIdentity<'a> {
    pub execution_id: &'a str,
    pub generation: u32,
    pub package_digest: &'a str,
    pub candidate_sha: &'a str,
    pub oracle_digest: &'a str,
}

impl CandidatePin {
    /// Revalidate every identity field at the integration boundary. Any
    /// mismatch is refused (`Conflict`).
    pub fn revalidate(&self, now: &PinIdentity<'_>) -> Result<(), ContractError> {
        if self.execution_id != now.execution_id {
            return Err(FailureCode::IdentityConflict.failure(
                &self.execution_id,
                format!(
                    "pin execution {} != integrating {}",
                    self.execution_id, now.execution_id
                ),
            ));
        }
        if self.generation != now.generation {
            return Err(FailureCode::StaleGeneration.failure(
                &self.execution_id,
                format!(
                    "pin generation {} != current {}",
                    self.generation, now.generation
                ),
            ));
        }
        if self.package_digest != now.package_digest {
            return Err(FailureCode::IdentityConflict.failure(
                &self.package_digest,
                format!(
                    "verification pin package {} does not match the integrating frozen package {}",
                    self.package_digest, now.package_digest
                ),
            ));
        }
        if self.candidate_sha != now.candidate_sha {
            return Err(FailureCode::BaselineChanged.failure(
                &self.candidate_sha,
                format!(
                    "candidate changed between verify and integrate: verified {} but \
                     integration would ship {}",
                    self.candidate_sha, now.candidate_sha
                ),
            ));
        }
        if self.oracle_digest != now.oracle_digest {
            return Err(FailureCode::IdentityConflict.failure(
                &self.oracle_digest,
                format!(
                    "verification pin oracle {} does not match the frozen oracle {}",
                    self.oracle_digest, now.oracle_digest
                ),
            ));
        }
        Ok(())
    }
}

/// Immutable evidence emitted by the scope/protected-verification seam.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeEvidence {
    pub candidate_sha: String,
    pub package_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_digest: Option<String>,
    #[serde(default)]
    pub baseline_files: usize,
    #[serde(default)]
    pub denied_paths: Vec<DeniedChange>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(kind: ChangeKind, path: &str) -> FileChange {
        FileChange {
            kind,
            path: path.to_string(),
            old_path: None,
            old_mode: None,
            new_mode: None,
            symlink_target: None,
        }
    }

    #[test]
    fn normalize_strict_and_whitespace_preserved() {
        assert!(normalize_path("/etc/passwd").is_err());
        assert!(normalize_path("a/../../b").is_err());
        assert!(normalize_path("a//b").is_err());
        assert_eq!(normalize_path("a/b ").unwrap(), "a/b ");
    }

    #[test]
    fn deny_wins_over_allow() {
        let policy = ScopePolicy {
            allowed: vec!["crates".to_string()],
            protected: vec!["crates/secret".to_string()],
        };
        let report = policy.authorize(&[change(ChangeKind::Modified, "crates/secret/key.rs")]);
        assert!(report.is_denied());
        assert_eq!(report.denied[0].code, "protected");
    }
}
