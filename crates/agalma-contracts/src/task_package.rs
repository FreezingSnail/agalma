//! `TaskPackage/v1` — the immutable task definition frozen before first dispatch.
//!
//! Revision 1 (`docs/self-mutation-contracts.md`, `agalma-fjq`). A package is a
//! manifest plus immutable referenced objects: the full issue
//! `definition` (title/body/design/notes, machine block, acceptance), the
//! `context` objects (instructions, parent/shared design, closed-dependency
//! handoffs), `scope`, role pins, budget, and the source identity it was frozen
//! from. Recovery loads the frozen package; it never refreshes `definition` from
//! a live backlog.
//!
//! The canonical [`digest`](crate::digest) of a package covers its canonical
//! document **excluding** its own `package_digest` field. [`TaskPackage::verify`]
//! gates the version and re-derives that digest.
//!
//! The schema `ArtifactRef` (`{path, digest, bytes, media_type}`) is modelled here
//! as [`PackageArtifactRef`] to stay distinct from the opaque
//! [`crate::ids::ArtifactRef`] newtype.
//!
//! This module is pure: no I/O, no vendor types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::digest::{canonical_digest, sha256_digest, validate_digest, FailureCode};
use crate::error::ContractError;

/// The revision-1 package format identifier.
pub const TASK_PACKAGE_FORMAT: &str = "TaskPackage/v1";
/// Basename of the format's family (`TaskPackage/`).
pub const TASK_PACKAGE_FAMILY: &str = "TaskPackage";

/// An immutable stored object: repository/store-relative path, digest, byte
/// count, and media type (schema `ArtifactRef`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageArtifactRef {
    pub path: String,
    pub digest: String,
    pub bytes: u64,
    pub media_type: String,
}

impl PackageArtifactRef {
    /// Describe `bytes` at `path` (digest computed with SHA-256).
    pub fn new(path: impl Into<String>, bytes: &[u8], media_type: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            digest: sha256_digest(bytes),
            bytes: bytes.len() as u64,
            media_type: media_type.into(),
        }
    }

    /// Verify the digest shape, then that `bytes` match the recorded digest.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), ContractError> {
        validate_digest(&self.digest, &self.path)?;
        if sha256_digest(bytes) != self.digest || bytes.len() as u64 != self.bytes {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.path,
                "artifact bytes do not match the recorded digest/size",
            ));
        }
        Ok(())
    }
}

/// Inline text with provenance. `source` is provenance, not a recovery lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextObject {
    pub source: String,
    pub text: String,
    pub digest: String,
}

impl TextObject {
    /// Capture `text` from `source`, recording its exact-byte SHA-256.
    pub fn new(source: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            digest: sha256_digest(text.as_bytes()),
            source: source.into(),
            text,
        }
    }

    /// Verify the recorded digest matches the text bytes.
    pub fn verify(&self) -> Result<(), ContractError> {
        validate_digest(&self.digest, &self.source)?;
        if sha256_digest(self.text.as_bytes()) != self.digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.source,
                "text does not match the recorded digest",
            ));
        }
        Ok(())
    }
}

/// A pinned component binding (identity, API, and generation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingPin {
    pub binding_id: String,
    pub api: String,
    pub api_version: u64,
    pub implementation_version: String,
    pub generation: u64,
    pub config_digest: String,
}

/// A role that may run for a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Builder,
    Verifier,
    Planner,
    Reviewer,
    Evaluator,
}

/// A selected route/model plus its harness binding and immutable prompt ref.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolePin {
    pub role: Role,
    pub route_id: String,
    pub model: String,
    pub harness: BindingPin,
    pub prompt: PackageArtifactRef,
}

/// The directive that authorized the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveRef {
    pub id: String,
    pub version: u64,
    pub digest: String,
}

/// The source repository identity and frozen base commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRef {
    pub identity: String,
    pub base_sha: String,
}

/// Provenance of the authorization: a human bootstrap or directive policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationOrigin {
    HumanBootstrap,
    DirectivePolicy,
}

/// Explicit authorization: origin, policy digest, and the receipt artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authorization {
    pub origin: AuthorizationOrigin,
    pub policy_digest: String,
    pub receipt: PackageArtifactRef,
}

/// The full frozen issue definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub title: String,
    pub description: TextObject,
    pub design: TextObject,
    pub notes: TextObject,
    pub machine_block: String,
    pub acceptance: Vec<String>,
}

/// What a context object is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Instruction,
    ParentDesign,
    SharedDesign,
    DependencyHandoff,
}

/// One immutable context object with its source identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextItem {
    pub kind: ContextKind,
    pub source: String,
    pub artifact: PackageArtifactRef,
}

/// Candidate and protected file scope plus the held-out oracle reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub allowed_paths: Vec<String>,
    pub protected_paths: Vec<String>,
    pub readable_artifacts: Vec<String>,
    pub oracle: PackageArtifactRef,
}

/// Unknown-usage policy. Revision 1 allows only explicit denial.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnknownUsage {
    Deny,
}

/// The frozen budget: allowance, unknown-usage policy, and quota policy digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub allowance_nusd: u64,
    pub unknown_usage: UnknownUsage,
    pub quota_policy_digest: String,
}

/// A frozen `TaskPackage/v1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPackage {
    /// Wire format; gated by [`TaskPackage::version_gate`].
    pub format: String,
    pub task_id: String,
    pub execution_id: String,
    pub generation: u64,
    pub lease_generation: u64,
    pub repository: RepositoryRef,
    pub directive: DirectiveRef,
    pub authorization: Authorization,
    pub definition: Definition,
    pub context: Vec<ContextItem>,
    pub scope: Scope,
    pub max_attempts: u64,
    pub budget: Budget,
    pub roles: Vec<RolePin>,
    pub recorded_at_unix_ms: u64,
    pub package_digest: String,
}

impl TaskPackage {
    /// Gate the `format` version. A `TaskPackage/vN` with `N != 1` is
    /// `UnsupportedVersion`; an unrecognized family is `UnsupportedSchema`.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        match parse_format_version(&self.format) {
            Ok(1) => Ok(()),
            Ok(other) => Err(FailureCode::UnsupportedVersion.failure(
                &self.task_id,
                format!("TaskPackage version {other} is unsupported (expected 1)"),
            )),
            Err(reason) => Err(FailureCode::UnsupportedSchema
                .failure(&self.task_id, format!("unrecognized package format: {reason}"))),
        }
    }

    /// The canonical document to hash: this package minus its own
    /// `package_digest` field.
    pub fn canonical_document(&self) -> Result<Value, ContractError> {
        let mut value = serde_json::to_value(self).map_err(|e| {
            FailureCode::InvalidRecord.failure(&self.task_id, format!("cannot serialize: {e}"))
        })?;
        if let Some(object) = value.as_object_mut() {
            object.remove("package_digest");
        }
        Ok(value)
    }

    /// Recompute the package digest over [`Self::canonical_document`].
    pub fn package_digest(&self) -> Result<String, ContractError> {
        canonical_digest(&self.canonical_document()?)
    }

    /// Stamp `package_digest` from the canonical document.
    pub fn seal(mut self) -> Result<Self, ContractError> {
        self.package_digest = self.package_digest()?;
        Ok(self)
    }

    /// Gate the version, then verify the recorded digest.
    pub fn verify(&self) -> Result<(), ContractError> {
        self.version_gate()?;
        validate_digest(&self.package_digest, &self.task_id)?;
        let recomputed = self.package_digest()?;
        if recomputed != self.package_digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.task_id,
                format!(
                    "package digest {} does not match recomputed {recomputed}",
                    self.package_digest
                ),
            ));
        }
        Ok(())
    }

    /// Verify every inline text digest. Separate from [`Self::verify`] because a
    /// load only gates version + package digest.
    pub fn verify_text(&self) -> Result<(), ContractError> {
        self.definition.description.verify()?;
        self.definition.design.verify()?;
        self.definition.notes.verify()?;
        Ok(())
    }
}

/// Parse `TaskPackage/vN` into `N`.
fn parse_format_version(format: &str) -> Result<u64, String> {
    let (family, version) = format
        .split_once('/')
        .ok_or_else(|| format!("{format:?} has no `/version`"))?;
    if family != TASK_PACKAGE_FAMILY {
        return Err(format!("unknown family {family:?}"));
    }
    let digits = version
        .strip_prefix('v')
        .ok_or_else(|| format!("version {version:?} is not `vN`"))?;
    digits
        .parse::<u64>()
        .map_err(|_| format!("version {version:?} is not `vN`"))
}

/// The `FailureDetail/v1` code for a malformed record (helper for callers).
pub fn invalid_record(subject: impl AsRef<str>, message: impl AsRef<str>) -> ContractError {
    FailureCode::InvalidRecord.failure(subject, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/contracts/self-mutation-v1/task-package.json"
    ));

    fn example() -> TaskPackage {
        serde_json::from_str(EXAMPLE).expect("example parses")
    }

    #[test]
    fn example_package_round_trips_and_verifies() {
        let package = example();
        assert_eq!(package.format, TASK_PACKAGE_FORMAT);
        package.version_gate().expect("v1 gate");
        package.verify().expect("fixture digest matches canonical document");
        package.verify_text().expect("text digests match");
    }

    #[test]
    fn package_digest_excludes_its_own_field() {
        let mut package = example();
        package.package_digest = "sha256:".to_string() + &"0".repeat(64);
        let recomputed = package.package_digest().expect("recompute");
        assert_eq!(
            recomputed,
            example().package_digest,
            "the digest field is excluded from its own input"
        );
        assert!(package.verify().is_err(), "a corrupted digest is refused");
    }

    #[test]
    fn unsupported_version_is_gated_before_digest() {
        let mut package = example();
        package.format = "TaskPackage/v99".to_string();
        package.package_digest = "fnv1a64:0000000000000000".to_string();
        let err = package.verify().expect_err("v99 refused");
        assert!(matches!(err, ContractError::UnsupportedCapability(_)));
        assert_eq!(crate::digest::failure_code(&err), Some(FailureCode::UnsupportedVersion));
    }

    #[test]
    fn seal_reproduces_the_fixture_digest() {
        let sealed = example().seal().expect("seal");
        assert_eq!(sealed.package_digest, example().package_digest);
    }
}
