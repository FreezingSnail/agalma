//! Immutable `TaskPackage/v1` construction, persistence, and recovery.
//!
//! Wave 1 of SF.2 (`agalma-5fy.2`): a pure builder from a full
//! [`TaskDefinition`](agalma_taskqueue::TaskDefinition) plus role pins, source
//! SHA, directive/budget/attempt settings, and immutable shared-design/parent/
//! dependency-handoff references; an atomic content-addressed store under the
//! state dir; and a load-and-verify path.
//!
//! The store lives at `<state_dir>/packages/` — deliberately **not** under any
//! reap subdirectory (`runs/`), so a frozen package outlives execution-scratch
//! cleanup. Documents are keyed by their canonical digest:
//! `<state_dir>/packages/sha256/<hex>.json`, with referenced context objects at
//! `<state_dir>/packages/objects/sha256/<hex>`. Every write is a temp file plus
//! `fsync` plus atomic rename.
//!
//! This module introduces **no new database**. The package reference is meant to
//! be committed to the existing ledger; wave 2 wires admission in `work.rs`.
//!
//! Recovery must call [`load_package`] (or [`load_frozen_or_legacy`]) and never
//! refresh the definition from the live backlog. A missing package parks with
//! `LegacyInputsMissing`; a corrupt digest parks with `IntegrityMismatch`; an
//! unsupported version parks with `UnsupportedVersion`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agalma_contracts::digest::{canonical_json, sha256_digest, validate_digest, FailureCode};
use agalma_contracts::{
    Authorization, AuthorizationOrigin, BindingPin, Budget, ContextItem, ContextKind, ContractError,
    Definition, DirectiveRef, PackageArtifactRef, RepositoryRef, Role, RolePin, Scope, TaskId,
    TaskPackage, TextObject, UnknownUsage, TASK_PACKAGE_FORMAT,
};
use agalma_taskqueue::{parse_task_block, BdTaskQueue, TaskDefinition};

/// Ledger event kind carrying the durable package reference (SF.2 admission).
pub const ADMITTED_EVENT_KIND: &str = "task.package_admitted";
/// Repository instruction files frozen into every admitted package.
pub const INSTRUCTION_SOURCES: &[&str] = &["AGENTS.md"];
/// Repository shared-design documents frozen into every admitted package.
pub const SHARED_DESIGN_SOURCES: &[&str] = &["docs/self-mutation-contracts.md"];

/// Store directory name under the state dir.
pub const PACKAGE_STORE_DIR: &str = "packages";
/// Content-addressed objects subdirectory.
pub const PACKAGE_OBJECTS_SUBDIR: &str = "objects";
/// Digest-algorithm subdirectory.
pub const PACKAGE_HASH_SUBDIR: &str = "sha256";
/// Media type of a stored package document.
pub const PACKAGE_MEDIA_TYPE: &str = "application/json";

/// A role pin to embed in a package. The prompt bytes are materialized as a
/// content-addressed object by [`build_package`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleInput {
    pub role: Role,
    pub route_id: String,
    pub model: String,
    pub harness: BindingPin,
    pub prompt_bytes: Vec<u8>,
    pub prompt_media_type: String,
}

impl RoleInput {
    /// Pin `role` to a route/model, harness binding, and immutable prompt bytes.
    pub fn new(
        role: Role,
        route_id: impl Into<String>,
        model: impl Into<String>,
        harness: BindingPin,
        prompt_bytes: impl Into<Vec<u8>>,
        prompt_media_type: impl Into<String>,
    ) -> Self {
        Self {
            role,
            route_id: route_id.into(),
            model: model.into(),
            harness,
            prompt_bytes: prompt_bytes.into(),
            prompt_media_type: prompt_media_type.into(),
        }
    }
}

/// An immutable context object to embed (its bytes are persisted by reference).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextInput {
    pub kind: ContextKind,
    pub source: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl ContextInput {
    /// Capture `bytes` from `source` as a context object of `kind`.
    pub fn new(
        kind: ContextKind,
        source: impl Into<String>,
        media_type: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            kind,
            source: source.into(),
            media_type: media_type.into(),
            bytes: bytes.into(),
        }
    }
}

/// Explicit authorization input: origin, policy digest, and receipt bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationInput {
    pub origin: AuthorizationOrigin,
    pub policy_digest: String,
    pub receipt_source: String,
    pub receipt_media_type: String,
    pub receipt_bytes: Vec<u8>,
}

/// Everything a package build needs besides the issue definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageInputs {
    pub execution_id: String,
    pub generation: u64,
    pub lease_generation: u64,
    pub repository_identity: String,
    pub base_sha: String,
    pub directive: DirectiveRef,
    pub authorization: AuthorizationInput,
    pub roles: Vec<RoleInput>,
    pub instructions: Vec<ContextInput>,
    pub shared_design: Vec<ContextInput>,
    pub parent_design: Vec<ContextInput>,
    pub dependency_handoffs: Vec<ContextInput>,
    pub allowed_paths: Vec<String>,
    pub protected_paths: Vec<String>,
    pub readable_artifacts: Vec<String>,
    pub oracle_bytes: Vec<u8>,
    pub oracle_media_type: String,
    pub max_attempts: u64,
    pub allowance_nusd: u64,
    pub quota_policy_digest: String,
    pub recorded_at_unix_ms: u64,
}

/// A built package plus the immutable object bytes it references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltPackage {
    pub package: TaskPackage,
    /// `(digest, bytes)` for every object that must be persisted.
    pub objects: Vec<(String, Vec<u8>)>,
}

/// The digest and state-dir-relative path of a persisted package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRef {
    pub digest: String,
    pub path: String,
}

/// Build a sealed `TaskPackage/v1` and collect the objects it references.
///
/// Pure: no filesystem, no state dir. Context and receipt paths are
/// store-relative (`packages/objects/sha256/<hex>`); [`persist_package`] writes
/// them.
pub fn build_package(
    definition: &TaskDefinition,
    inputs: PackageInputs,
) -> Result<BuiltPackage, ContractError> {
    let task_id = definition.task_id.as_str().to_string();
    let mut objects: Vec<(String, Vec<u8>)> = Vec::new();

    // The machine block and acceptance come from the fenced task block when the
    // body carries one; otherwise acceptance falls back to the criteria text.
    let (machine_block, acceptance) = machine_block_and_acceptance(definition)?;

    // Collect the immutable context objects and their store-relative refs.
    let mut context: Vec<ContextItem> = Vec::new();
    let mut readable: Vec<String> = Vec::new();
    let mut add = |kind: ContextKind, input: ContextInput| -> Result<(), ContractError> {
        let artifact = store_object(&task_id, &input, &mut objects)?;
        readable.push(artifact.digest.clone());
        context.push(ContextItem {
            kind,
            source: input.source,
            artifact,
        });
        Ok(())
    };
    for input in inputs.instructions {
        add(ContextKind::Instruction, input)?;
    }
    for input in inputs.parent_design {
        add(ContextKind::ParentDesign, input)?;
    }
    for input in inputs.shared_design {
        add(ContextKind::SharedDesign, input)?;
    }
    for input in inputs.dependency_handoffs {
        add(ContextKind::DependencyHandoff, input)?;
    }
    drop(add);

    if context.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            &task_id,
            "context must include at least one instruction/design/handoff object",
        ));
    }

    let receipt = store_object(
        &task_id,
        &ContextInput::new(
            ContextKind::Instruction,
            inputs.authorization.receipt_source.clone(),
            inputs.authorization.receipt_media_type.clone(),
            inputs.authorization.receipt_bytes.clone(),
        ),
        &mut objects,
    )?;

    if inputs.roles.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            &task_id,
            "at least one role pin is required",
        ));
    }
    // Materialize each role prompt and the oracle as content-addressed objects so
    // the referenced bytes are durable and recoverable, not just referenced.
    let mut roles: Vec<RolePin> = Vec::new();
    for role in inputs.roles {
        let prompt = store_object(
            &task_id,
            &ContextInput::new(
                ContextKind::Instruction,
                format!("role:{}/prompt", role.route_id),
                role.prompt_media_type,
                role.prompt_bytes,
            ),
            &mut objects,
        )?;
        readable.push(prompt.digest.clone());
        roles.push(RolePin {
            role: role.role,
            route_id: role.route_id,
            model: role.model,
            harness: role.harness,
            prompt,
        });
    }
    let oracle = store_object(
        &task_id,
        &ContextInput::new(
            ContextKind::Instruction,
            "scope/oracle",
            inputs.oracle_media_type,
            inputs.oracle_bytes,
        ),
        &mut objects,
    )?;
    // The held-out oracle is never a builder-readable artifact: it stays durable
    // and reachable only through its explicit `scope.oracle` reference. A
    // caller-supplied grant of the same oracle is refused.
    if inputs
        .readable_artifacts
        .iter()
        .any(|digest| digest == &oracle.digest)
    {
        return Err(FailureCode::ScopeDenied.failure(
            &task_id,
            "held-out oracle may not be granted as a builder-readable artifact",
        ));
    }
    readable.extend(inputs.readable_artifacts.iter().cloned());
    readable.sort();
    readable.dedup();

    let package = TaskPackage {
        format: TASK_PACKAGE_FORMAT.to_string(),
        task_id,
        execution_id: inputs.execution_id,
        generation: inputs.generation,
        lease_generation: inputs.lease_generation,
        repository: RepositoryRef {
            identity: inputs.repository_identity,
            base_sha: inputs.base_sha,
        },
        directive: inputs.directive,
        authorization: Authorization {
            origin: inputs.authorization.origin,
            policy_digest: inputs.authorization.policy_digest,
            receipt,
        },
        definition: Definition {
            title: definition.title.clone(),
            description: TextObject::new(
                format!("bd:{}/description", definition.task_id.as_str()),
                definition.description.clone(),
            ),
            design: TextObject::new(
                format!("bd:{}/design", definition.task_id.as_str()),
                definition.design.clone().unwrap_or_default(),
            ),
            notes: TextObject::new(
                format!("bd:{}/notes", definition.task_id.as_str()),
                definition.notes.clone().unwrap_or_default(),
            ),
            machine_block,
            acceptance,
        },
        context,
        scope: Scope {
            allowed_paths: inputs.allowed_paths,
            protected_paths: inputs.protected_paths,
            readable_artifacts: readable,
            oracle,
        },
        max_attempts: inputs.max_attempts,
        budget: Budget {
            allowance_nusd: inputs.allowance_nusd,
            unknown_usage: UnknownUsage::Deny,
            quota_policy_digest: inputs.quota_policy_digest,
        },
        roles,
        recorded_at_unix_ms: inputs.recorded_at_unix_ms,
        package_digest: String::new(),
    }
    .seal()?;

    Ok(BuiltPackage { package, objects })
}

fn machine_block_and_acceptance(
    definition: &TaskDefinition,
) -> Result<(String, Vec<String>), ContractError> {
    if let Ok(Some(block)) = parse_task_block(&definition.description) {
        return Ok((block.raw_block, block.acceptance));
    }
    let acceptance: Vec<String> = definition
        .acceptance_criteria
        .as_deref()
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if acceptance.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            definition.task_id.as_str(),
            "no fenced task block and no acceptance criteria to freeze",
        ));
    }
    Ok((String::new(), acceptance))
}

/// The store-relative path of an object with `digest`.
pub fn object_relative_path(digest: &str) -> Result<String, ContractError> {
    validate_digest(digest, "package object")?;
    let hex = digest.trim_start_matches("sha256:");
    Ok(format!(
        "{PACKAGE_STORE_DIR}/{PACKAGE_OBJECTS_SUBDIR}/{PACKAGE_HASH_SUBDIR}/{hex}"
    ))
}

fn store_object(
    subject: &str,
    input: &ContextInput,
    objects: &mut Vec<(String, Vec<u8>)>,
) -> Result<PackageArtifactRef, ContractError> {
    let digest = sha256_digest(&input.bytes);
    let path = object_relative_path(&digest)?;
    validate_digest(&digest, subject)?;
    objects.push((digest.clone(), input.bytes.clone()));
    Ok(PackageArtifactRef {
        path,
        digest,
        bytes: input.bytes.len() as u64,
        media_type: input.media_type.clone(),
    })
}

/// Atomically persist every referenced object, then the package document.
pub fn persist_package(
    state_dir: &Path,
    built: &BuiltPackage,
) -> Result<PackageRef, ContractError> {
    built.package.verify()?;
    for (digest, bytes) in &built.objects {
        write_atomic(&object_path(state_dir, digest)?, bytes)?;
    }
    let document = canonical_json(&serde_json::to_value(&built.package).map_err(|e| {
        FailureCode::InvalidRecord.failure(&built.package.task_id, format!("serialize: {e}"))
    })?)?;
    let digest = built.package.package_digest.clone();
    let path = package_path(state_dir, &digest)?;
    write_atomic(&path, document.as_bytes())?;
    Ok(PackageRef {
        digest,
        path: relative_to(state_dir, &path),
    })
}

/// Load and verify a frozen package by digest.
///
/// Missing file → `LegacyInputsMissing`; unparsable → `InvalidRecord`; version →
/// `UnsupportedVersion`; a document whose own digest does not match the
/// *requested* digest → `IntegrityMismatch` (a self-consistent rewrite under the
/// requested store path is still refused).
pub fn load_package(state_dir: &Path, digest: &str) -> Result<TaskPackage, ContractError> {
    let path = package_path(state_dir, digest)?;
    let text = fs::read_to_string(&path).map_err(|e| {
        FailureCode::LegacyInputsMissing.failure(
            digest,
            format!("frozen package {} unreadable: {e}", path.display()),
        )
    })?;
    let package: TaskPackage = serde_json::from_str(&text).map_err(|e| {
        FailureCode::InvalidRecord.failure(digest, format!("frozen package is malformed: {e}"))
    })?;
    package.verify()?;
    if package.package_digest != digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            digest,
            format!(
                "stored document digest {} does not match the requested digest {digest}",
                package.package_digest
            ),
        ));
    }
    Ok(package)
}

/// Load a frozen package and verify every referenced immutable input object
/// before any effect. Recovery uses this: a missing or corrupt referenced object
/// parks rather than dispatching.
pub fn load_admitted_package(state_dir: &Path, digest: &str) -> Result<TaskPackage, ContractError> {
    let package = load_package(state_dir, digest)?;
    verify_references(state_dir, &package)?;
    Ok(package)
}

/// Verify every required referenced object (context, authorization receipt, role
/// prompts, oracle, and readable-artifact digests) resolves and matches its
/// recorded digest/size.
///
/// Also refuses a stored admitted package whose builder-readable list grants its
/// own held-out oracle. This is the common bounded validation for both first
/// dispatch and recovery (`load_admitted_package`); it is read-only and never
/// rewrites a stored package.
pub fn verify_references(state_dir: &Path, package: &TaskPackage) -> Result<(), ContractError> {
    if package
        .scope
        .readable_artifacts
        .iter()
        .any(|digest| digest == &package.scope.oracle.digest)
    {
        return Err(FailureCode::ScopeDenied.failure(
            &package.task_id,
            "stored package grants its held-out oracle as a builder-readable artifact",
        ));
    }
    for item in &package.context {
        read_artifact(state_dir, &item.artifact)?;
    }
    read_artifact(state_dir, &package.authorization.receipt)?;
    for role in &package.roles {
        read_artifact(state_dir, &role.prompt)?;
    }
    read_artifact(state_dir, &package.scope.oracle)?;
    for digest in &package.scope.readable_artifacts {
        verify_object_digest(state_dir, digest)?;
    }
    Ok(())
}

/// Read a content-addressed object by digest and verify its bytes match.
fn verify_object_digest(state_dir: &Path, digest: &str) -> Result<(), ContractError> {
    let path = object_path(state_dir, digest)?;
    let bytes = fs::read(&path).map_err(|e| {
        FailureCode::LegacyInputsMissing.failure(
            digest,
            format!("referenced object {} unreadable: {e}", path.display()),
        )
    })?;
    if sha256_digest(&bytes) != digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            digest,
            "referenced object bytes do not match the recorded digest",
        ));
    }
    Ok(())
}

/// Recovery entry: load the frozen package, or park an execution that has none.
///
/// A `None` reference is a pre-SF.2 in-flight execution whose inputs cannot be
/// reconstructed: explicit `LegacyInputsMissing`, never a live bd refresh.
pub fn load_frozen_or_legacy(
    state_dir: &Path,
    reference: Option<&str>,
    subject: &str,
) -> Result<TaskPackage, ContractError> {
    match reference {
        Some(digest) => load_admitted_package(state_dir, digest),
        None => Err(legacy_park(
            subject,
            "no frozen TaskPackage reference (legacy in-flight execution)",
        )),
    }
}

/// The explicit park for unreconstructable legacy inputs.
pub fn legacy_park(subject: &str, reason: &str) -> ContractError {
    FailureCode::LegacyInputsMissing.failure(subject, reason)
}

/// The production context bundle frozen into an admitted package.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdmissionContext {
    pub instructions: Vec<ContextInput>,
    pub shared_design: Vec<ContextInput>,
    pub parent_design: Vec<ContextInput>,
    pub dependency_handoffs: Vec<ContextInput>,
}

/// Gather the immutable context for `definition` from the repository and
/// backlog once, at admission (SF.2).
///
/// Repository instruction and shared-design documents are captured verbatim when
/// present; instruction documents referenced by `AGENTS.md` are also frozen. The
/// parent epic design and each declared dependency's handoff are **required**: a
/// declared source that cannot be captured refuses admission rather than
/// silently dropping context. The fenced directive block is frozen as a builder
/// instruction so a task with no repository context still admits (context must be
/// non-empty). Nothing here reads a live backlog during recovery: this is the
/// one-time capture that recovery later reads back from the store.
pub fn gather_admission_context(
    repo: &Path,
    definitions: &BdTaskQueue,
    definition: &TaskDefinition,
) -> Result<AdmissionContext, ContractError> {
    let mut context = AdmissionContext::default();
    let mut instruction_sources: Vec<String> = Vec::new();

    for source in INSTRUCTION_SOURCES {
        if let Some(bytes) = observe_repo_file(repo, source)? {
            instruction_sources.push((*source).to_string());
            let referenced = referenced_markdown(&String::from_utf8_lossy(&bytes));
            context.instructions.push(ContextInput::new(
                ContextKind::Instruction,
                *source,
                "text/plain",
                bytes,
            ));
            // Instruction documents the repository policy references are
            // required: a missing, non-file, or unreadable one refuses admission
            // rather than admitting a package without its governing instructions.
            for referenced_source in referenced {
                if instruction_sources.iter().any(|s| s == &referenced_source) {
                    continue;
                }
                let referenced_bytes = read_required_repo_file(repo, &referenced_source)?;
                instruction_sources.push(referenced_source.clone());
                context.instructions.push(ContextInput::new(
                    ContextKind::Instruction,
                    referenced_source,
                    "text/plain",
                    referenced_bytes,
                ));
            }
        }
    }
    for source in SHARED_DESIGN_SOURCES {
        if let Some(bytes) = observe_repo_file(repo, source)? {
            context.shared_design.push(ContextInput::new(
                ContextKind::SharedDesign,
                *source,
                "text/plain",
                bytes,
            ));
        }
    }
    if let Ok(Some(block)) = parse_task_block(&definition.description) {
        context.instructions.push(ContextInput::new(
            ContextKind::Instruction,
            format!("bd:{}/block", definition.task_id.as_str()),
            "text/plain",
            block.raw_block.into_bytes(),
        ));
    }
    // A declared parent design is required: refusing admission beats dispatching
    // with a missing design.
    if let Some(parent) = &definition.parent {
        let parent_def = definitions
            .task_definition(&TaskId::new(parent.as_str()))
            .map_err(|err| {
                FailureCode::LegacyInputsMissing.failure(
                    format!("bd:{parent}/design"),
                    format!("declared parent design unavailable: {err}"),
                )
            })?;
        let bytes = parent_def
            .design
            .clone()
            .unwrap_or(parent_def.description)
            .into_bytes();
        context.parent_design.push(ContextInput::new(
            ContextKind::ParentDesign,
            format!("bd:{parent}/design"),
            "text/plain",
            bytes,
        ));
    }
    // Every declared dependency handoff is required for the same reason.
    for dep in &definition.dependencies {
        let dep_def = definitions
            .task_definition(&TaskId::new(dep.id.as_str()))
            .map_err(|err| {
                FailureCode::LegacyInputsMissing.failure(
                    format!("bd:{}/handoff", dep.id),
                    format!("declared dependency handoff unavailable: {err}"),
                )
            })?;
        context.dependency_handoffs.push(ContextInput::new(
            ContextKind::DependencyHandoff,
            format!("bd:{}/handoff", dep.id),
            "text/plain",
            render_handoff(&dep_def, dep.kind.as_deref()).into_bytes(),
        ));
    }
    Ok(context)
}

/// Repository-relative `.md` paths referenced by an instruction document.
///
/// Bounded, deterministic scan: whitespace/quote/paren-delimited tokens ending in
/// `.md`. Every referenced document is required: [`gather_admission_context`]
/// refuses admission when a referenced `.md` is missing, a non-file, or
/// unreadable.
fn referenced_markdown(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for token in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '"' | '\'' | '`' | ',' | '<' | '>')
    }) {
        let token = token.trim_matches(|c: char| matches!(c, ':' | '*' | ';' | '.'));
        let candidate = token
            .strip_prefix("./")
            .unwrap_or(token);
        if candidate.ends_with(".md")
            && !candidate.starts_with('/')
            && !candidate.starts_with("http")
            && !candidate.contains("://")
            && !found.iter().any(|s| s == candidate)
        {
            found.push(candidate.to_string());
        }
    }
    found
}

/// Observe an optional repository source: `Ok(Some(bytes))` for a readable
/// regular file, `Ok(None)` for a genuine absence, and an error for a metadata
/// or read failure (a stat/permission failure must not be treated as absence).
fn observe_repo_file(repo: &Path, source: &str) -> Result<Option<Vec<u8>>, ContractError> {
    let path = repo.join(source);
    match fs::metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() {
                return Ok(None);
            }
            fs::read(&path).map(Some).map_err(|e| {
                FailureCode::StateUnavailable.failure(
                    source,
                    format!("cannot read {}: {e}", path.display()),
                )
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(FailureCode::StateUnavailable.failure(
            source,
            format!("cannot stat {}: {e}", path.display()),
        )),
    }
}

/// Read a **required** repository source: a missing, non-file, or unreadable
/// source refuses admission.
fn read_required_repo_file(repo: &Path, source: &str) -> Result<Vec<u8>, ContractError> {
    let path = repo.join(source);
    let meta = fs::metadata(&path).map_err(|e| {
        FailureCode::LegacyInputsMissing.failure(
            source,
            format!("required instruction {} unavailable: {e}", path.display()),
        )
    })?;
    if !meta.is_file() {
        return Err(FailureCode::LegacyInputsMissing.failure(
            source,
            format!("required instruction {} is not a regular file", path.display()),
        ));
    }
    fs::read(&path).map_err(|e| {
        FailureCode::StateUnavailable.failure(
            source,
            format!("cannot read required instruction {}: {e}", path.display()),
        )
    })
}

/// Deterministic text form of a dependency issue captured as a handoff.
fn render_handoff(def: &TaskDefinition, kind: Option<&str>) -> String {
    let mut out = format!("# {}\n", def.title);
    if let Some(kind) = kind {
        out.push_str(&format!("edge: {kind}\n"));
    }
    out.push('\n');
    out.push_str(&def.description);
    if let Some(design) = &def.design {
        out.push_str("\n\n## design\n");
        out.push_str(design);
    }
    if let Some(notes) = &def.notes {
        out.push_str("\n\n## notes\n");
        out.push_str(notes);
    }
    if !def.comments.is_empty() {
        out.push_str("\n\n## handoff\n");
        for comment in &def.comments {
            out.push_str(comment);
            out.push('\n');
        }
    }
    out
}

/// Read and digest-verify a referenced object.
pub fn read_artifact(
    state_dir: &Path,
    artifact: &PackageArtifactRef,
) -> Result<Vec<u8>, ContractError> {
    let path = resolve_artifact_path(state_dir, &artifact.path)?;
    let bytes = fs::read(&path).map_err(|e| {
        FailureCode::LegacyInputsMissing.failure(
            &artifact.path,
            format!("referenced object unreadable: {e}"),
        )
    })?;
    artifact.verify(&bytes)?;
    Ok(bytes)
}

/// Resolve a stored path within `state_dir`, refusing absolute paths and `..`.
pub fn resolve_artifact_path(state_dir: &Path, path: &str) -> Result<PathBuf, ContractError> {
    let candidate = Path::new(path);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(FailureCode::ScopeDenied.failure(
            path,
            "artifact references must be store-relative and free of `..`",
        ));
    }
    Ok(state_dir.join(candidate))
}

/// `<state_dir>/packages` — the content-addressed store root.
pub fn store_root(state_dir: &Path) -> PathBuf {
    state_dir.join(PACKAGE_STORE_DIR)
}

/// `<state_dir>/packages/sha256/<hex>.json`.
pub fn package_path(state_dir: &Path, digest: &str) -> Result<PathBuf, ContractError> {
    validate_digest(digest, "package")?;
    let hex = digest.trim_start_matches("sha256:");
    Ok(store_root(state_dir)
        .join(PACKAGE_HASH_SUBDIR)
        .join(format!("{hex}.json")))
}

/// `<state_dir>/packages/objects/sha256/<hex>`.
pub fn object_path(state_dir: &Path, digest: &str) -> Result<PathBuf, ContractError> {
    validate_digest(digest, "package object")?;
    let hex = digest.trim_start_matches("sha256:");
    Ok(store_root(state_dir)
        .join(PACKAGE_OBJECTS_SUBDIR)
        .join(PACKAGE_HASH_SUBDIR)
        .join(hex))
}

/// Write `bytes` to `path` atomically: temp file, `fsync`, rename, `fsync` dir.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ContractError> {
    let dir = path.parent().ok_or_else(|| {
        FailureCode::InvalidRecord.failure(path.display().to_string(), "no parent directory")
    })?;
    fs::create_dir_all(dir).map_err(|e| {
        FailureCode::InvalidRecord.failure(dir.display().to_string(), format!("mkdir: {e}"))
    })?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{nonce}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp).map_err(|e| {
            FailureCode::InvalidRecord.failure(tmp.display().to_string(), format!("create: {e}"))
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| {
                FailureCode::InvalidRecord.failure(tmp.display().to_string(), format!("write: {e}"))
            })?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        FailureCode::InvalidRecord.failure(path.display().to_string(), format!("rename: {e}"))
    })?;
    // Directory fsync makes the rename durable; an ignored failure here would
    // let admission report success without a durable reference.
    let dir_file = fs::File::open(dir).map_err(|e| {
        FailureCode::InvalidRecord.failure(dir.display().to_string(), format!("open dir: {e}"))
    })?;
    dir_file.sync_all().map_err(|e| {
        FailureCode::InvalidRecord.failure(dir.display().to_string(), format!("fsync dir: {e}"))
    })?;
    Ok(())
}

/// Best-effort state-dir-relative path for the persist receipt.
fn relative_to(state_dir: &Path, path: &Path) -> String {
    path.strip_prefix(state_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::digest::failure_code;
    use agalma_contracts::TaskId;

    fn definition() -> TaskDefinition {
        TaskDefinition {
            task_id: TaskId::new("fixture.task"),
            title: "Fixture immutable context task".to_string(),
            description: "Body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\npriority: high\nfamily: feature\nmax_attempts: 2\n```\n".to_string(),
            design: Some("Design text.\n".to_string()),
            notes: Some("Notes text.\n".to_string()),
            acceptance_criteria: None,
            parent: Some("fixture.epic".to_string()),
            dependencies: Vec::new(),
            comments: vec!["handoff".to_string()],
        }
    }

    fn inputs() -> PackageInputs {
        PackageInputs {
            execution_id: "exec:fixture.task:1".to_string(),
            generation: 1,
            lease_generation: 1,
            repository_identity: "agalma-fixture".to_string(),
            base_sha: "1".repeat(40),
            directive: DirectiveRef {
                id: "standup-prompts".to_string(),
                version: 1,
                digest: sha256_digest(b"directive"),
            },
            authorization: AuthorizationInput {
                origin: AuthorizationOrigin::HumanBootstrap,
                policy_digest: sha256_digest(b"policy"),
                receipt_source: "objects/authorization.json".to_string(),
                receipt_media_type: "application/json".to_string(),
                receipt_bytes: b"{}".to_vec(),
            },
            roles: vec![RoleInput::new(
                Role::Builder,
                "fixture.builder",
                "fixture-model",
                BindingPin {
                    binding_id: "fixture.harness".to_string(),
                    api: "harness".to_string(),
                    api_version: 1,
                    implementation_version: "fixture-1".to_string(),
                    generation: 1,
                    config_digest: sha256_digest(b"{}"),
                },
                b"prompt",
                "text/plain",
            )],
            instructions: Vec::new(),
            shared_design: vec![ContextInput::new(
                ContextKind::SharedDesign,
                "docs/self-mutation-contracts.md",
                "text/plain",
                b"shared design".to_vec(),
            )],
            parent_design: Vec::new(),
            dependency_handoffs: vec![ContextInput::new(
                ContextKind::DependencyHandoff,
                "bd:fixture.dep",
                "text/plain",
                b"handoff bytes".to_vec(),
            )],
            allowed_paths: vec!["crates/fixture".to_string()],
            protected_paths: vec!["fixtures/protected/".to_string()],
            readable_artifacts: Vec::new(),
            oracle_bytes: b"oracle".to_vec(),
            oracle_media_type: "text/plain".to_string(),
            max_attempts: 2,
            allowance_nusd: 2_000_000_000,
            quota_policy_digest: sha256_digest(b"quota"),
            recorded_at_unix_ms: 1_791_820_800_000,
        }
    }

    #[test]
    fn build_verifies_and_freezes_context() {
        let built = build_package(&definition(), inputs()).expect("build");
        built.package.verify().expect("verify");
        built.package.verify_text().expect("text digests");
        assert_eq!(built.package.definition.acceptance, vec!["sh -c 'true'"]);
        assert_eq!(built.package.context.len(), 2);
        assert!(built.package.context.iter().any(|c| c.kind == ContextKind::SharedDesign));
    }

    #[test]
    fn missing_acceptance_is_refused() {
        let mut definition = definition();
        definition.description = "no block".to_string();
        definition.acceptance_criteria = None;
        let err = build_package(&definition, inputs()).expect_err("no acceptance");
        assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));
    }

    #[test]
    fn empty_context_is_refused() {
        let mut inputs = inputs();
        inputs.shared_design.clear();
        inputs.dependency_handoffs.clear();
        let err = build_package(&definition(), inputs).expect_err("no context");
        assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));
    }
}
