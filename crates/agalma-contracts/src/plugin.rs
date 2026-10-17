//! Common plugin contracts: manifest, installation, health, snapshot, reports,
//! actions and cancellation (`docs/self-mutation-contracts.md` §"HP",
//! revision 1).
//!
//! These records are the frozen seam between the hub and independently
//! installed plugins: package identity ([`PluginManifest`]), the operator's
//! immutable configuration and grants ([`PluginInstallation`]), observed health
//! ([`PluginHealth`]), snapshot recovery ([`PluginSnapshot`]), producer
//! publications versus hub-observed report envelopes ([`ReportPublication`] /
//! [`ReportEnvelope`]), scoped action intents ([`ActionIntent`]) and stable
//! cancellation identities ([`PluginCancelRequest`]).
//!
//! Plugin-private report data is namespaced and versioned JSON; the common
//! contract never interprets it. Every integrity digest uses the canonical
//! [`crate::digest`] encoding (`canonical-json/v1` + SHA-256).
//!
//! This module is pure: no I/O, no vendor types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::digest::{canonical_digest, canonical_json, validate_digest, FailureCode};
use crate::error::ContractError;
use crate::ledger::{ComponentReceipt, OperationState};
use crate::task_package::{BindingPin, PackageArtifactRef};

/// `PluginManifest/v1` wire format.
pub const PLUGIN_MANIFEST_FORMAT: &str = "PluginManifest/v1";
/// Basename of the manifest's format family.
pub const PLUGIN_MANIFEST_FAMILY: &str = "PluginManifest";
/// `PluginInstallation/v1` wire format.
pub const PLUGIN_INSTALLATION_FORMAT: &str = "PluginInstallation/v1";
/// Basename of the installation's format family.
pub const PLUGIN_INSTALLATION_FAMILY: &str = "PluginInstallation";
/// `PluginHealth/v1` wire format.
pub const PLUGIN_HEALTH_FORMAT: &str = "PluginHealth/v1";
/// Basename of the health record's format family.
pub const PLUGIN_HEALTH_FAMILY: &str = "PluginHealth";
/// `PluginSnapshot/v1` wire format.
pub const PLUGIN_SNAPSHOT_FORMAT: &str = "PluginSnapshot/v1";
/// Basename of the snapshot's format family.
pub const PLUGIN_SNAPSHOT_FAMILY: &str = "PluginSnapshot";
/// `ReportEnvelope/v1` wire format.
pub const REPORT_ENVELOPE_FORMAT: &str = "ReportEnvelope/v1";
/// Basename of the envelope's format family.
pub const REPORT_ENVELOPE_FAMILY: &str = "ReportEnvelope";
/// `ReportPublication/v1` wire format.
pub const REPORT_PUBLICATION_FORMAT: &str = "ReportPublication/v1";
/// Basename of the publication's format family.
pub const REPORT_PUBLICATION_FAMILY: &str = "ReportPublication";
/// `PluginOperationStatus/v1` wire format.
pub const PLUGIN_OPERATION_STATUS_FORMAT: &str = "PluginOperationStatus/v1";
/// Basename of the operation-status format family.
pub const PLUGIN_OPERATION_STATUS_FAMILY: &str = "PluginOperationStatus";
/// `PluginCancelRequest/v1` wire format.
pub const PLUGIN_CANCEL_REQUEST_FORMAT: &str = "PluginCancelRequest/v1";
/// Basename of the cancel-request format family.
pub const PLUGIN_CANCEL_REQUEST_FAMILY: &str = "PluginCancelRequest";
/// `ActionIntent/v1` wire format.
pub const ACTION_INTENT_FORMAT: &str = "ActionIntent/v1";
/// Basename of the action-intent format family.
pub const ACTION_INTENT_FAMILY: &str = "ActionIntent";

/// The common plugin protocol major this build understands.
pub const PROTOCOL_MAJOR: u64 = 1;
/// The negotiated capability a ledger binding must hold to serve component
/// storage writes.
pub const COMPONENT_STORE_CAPABILITY: &str = "component-store/v1";
/// Maximum canonical byte length of one report payload (64 KiB).
pub const REPORT_PAYLOAD_LIMIT_BYTES: u64 = 64 * 1024;
/// Maximum total canonical byte length of every entry payload in one plugin
/// snapshot (256 KiB); the snapshot is published at a global watermark and is
/// bounded as a whole, not only per entry.
pub const PLUGIN_SNAPSHOT_PAYLOAD_LIMIT_BYTES: u64 = 256 * 1024;
/// Maximum canonical byte length of a manifest/installation document (256 KiB).
pub const MANIFEST_DOCUMENT_LIMIT_BYTES: u64 = 256 * 1024;
/// Maximum canonical byte length of one component checkpoint (256 KiB).
pub const COMPONENT_CHECKPOINT_LIMIT_BYTES: u64 = 256 * 1024;
/// Maximum rows of each kind in one atomic component commit.
pub const COMPONENT_COMMIT_MAX_ROWS: usize = 100;

/// Parse `<family>/vN` into `N`; the error string explains a mismatch.
fn parse_format_version(format: &str, family: &str) -> Result<u64, String> {
    let (actual_family, version) = format
        .split_once('/')
        .ok_or_else(|| format!("{format:?} has no `/version`"))?;
    if actual_family != family {
        return Err(format!("unknown family {actual_family:?}"));
    }
    let digits = version
        .strip_prefix('v')
        .ok_or_else(|| format!("version {version:?} is not `vN`"))?;
    digits
        .parse::<u64>()
        .map_err(|_| format!("version {version:?} is not `vN`"))
}

/// Gate a single-version family: right family wrong version is
/// `UnsupportedVersion`; an unrecognized family is `UnsupportedSchema`.
fn gate_single_version(
    format: &str,
    family: &str,
    label: &str,
    subject: &str,
) -> Result<(), ContractError> {
    match parse_format_version(format, family) {
        Ok(1) => Ok(()),
        Ok(other) => Err(FailureCode::UnsupportedVersion.failure(
            subject,
            format!("{label} version {other} is unsupported (expected 1)"),
        )),
        Err(reason) => Err(FailureCode::UnsupportedSchema.failure(
            subject,
            format!("unrecognized {label} format: {reason}"),
        )),
    }
}

/// How a plugin's executable is launched inside the sealed package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntrypoint {
    /// Package-relative executable path.
    pub path: String,
    /// Argument vector (never a shell string).
    pub argv: Vec<String>,
    /// Named launch profile the hub uses to sanitize the environment.
    pub launch_profile: String,
}

/// One offered domain API and its major-versioned capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainApi {
    /// Domain API name (e.g. `harness`).
    pub api: String,
    /// Offered API major version.
    pub major: u64,
    /// Capabilities advertised under this API.
    pub capabilities: Vec<String>,
}

/// A namespaced report schema reference carried by the package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportSchemaRef {
    /// Report namespace this schema describes.
    pub namespace: String,
    /// Monotonic schema version.
    pub version: u64,
    /// Stored schema artifact.
    pub schema: PackageArtifactRef,
}

/// The declarative view building blocks a report may render as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ViewKind {
    /// A single status value.
    Status,
    /// Key/value metrics.
    Metrics,
    /// A bounded table.
    Table,
    /// A timeline of observations.
    Timeline,
    /// An alert.
    Alert,
    /// A link to an artifact.
    ArtifactLink,
}

/// One declarative view description (no executable UI code).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    /// Stable view identifier.
    pub view_id: String,
    /// Report namespace this view reads.
    pub schema_namespace: String,
    /// Building block the hub renders.
    pub kind: ViewKind,
    /// Human-readable view title.
    pub title: String,
    /// Report schema version the view expects.
    pub schema_version: u64,
    /// Field names the view consumes.
    pub fields: Vec<String>,
}

/// One advertised action and its authority/confirmation requirements.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDescriptor {
    /// Stable action identifier.
    pub action_id: String,
    /// Namespaced input schema the action validates against.
    pub input_schema: String,
    /// Capability a caller must hold to invoke the action.
    pub required_capability: String,
    /// Whether the action produces a durable external effect.
    pub effectful: bool,
    /// Declared human-confirmation requirement, when any.
    pub human_confirmation: Option<String>,
    /// Default deadline applied when a caller supplies none.
    pub default_deadline_ms: u64,
}

/// The durable state namespaces a plugin reads and writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateCompatibility {
    /// Namespaces the plugin reads.
    pub reads: Vec<String>,
    /// Namespace the plugin writes.
    pub writes: String,
}

/// A frozen `PluginManifest/v1`: the sealed package's identity and declarations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Wire format; gated by [`PluginManifest::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub package_version: String,
    /// Sealed-inventory digest (not recomputable from the manifest alone).
    pub package_digest: String,
    /// Common plugin protocol major this package targets.
    pub protocol_major: u64,
    pub entrypoint: ManifestEntrypoint,
    pub domain_apis: Vec<DomainApi>,
    pub requested_capabilities: Vec<String>,
    pub required_services: Vec<String>,
    pub optional_services: Vec<String>,
    pub config_schema: PackageArtifactRef,
    pub default_configuration: PackageArtifactRef,
    pub report_schemas: Vec<ReportSchemaRef>,
    pub views: Vec<View>,
    pub actions: Vec<ActionDescriptor>,
    pub state_compatibility: StateCompatibility,
}

impl PluginManifest {
    /// Gate the `format` family/version and the `protocol_major`.
    ///
    /// A `PluginManifest/vN` with `N != 1` is `UnsupportedVersion`; an
    /// unrecognized family is `UnsupportedSchema`; a `protocol_major` other than
    /// [`PROTOCOL_MAJOR`] is `UnsupportedVersion`.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        match parse_format_version(&self.format, PLUGIN_MANIFEST_FAMILY) {
            Ok(1) => {}
            Ok(other) => {
                return Err(FailureCode::UnsupportedVersion.failure(
                    &self.plugin_id,
                    format!("PluginManifest version {other} is unsupported (expected 1)"),
                ))
            }
            Err(reason) => {
                return Err(FailureCode::UnsupportedSchema.failure(
                    &self.plugin_id,
                    format!("unrecognized manifest format: {reason}"),
                ))
            }
        }
        if self.protocol_major != PROTOCOL_MAJOR {
            return Err(FailureCode::UnsupportedVersion.failure(
                &self.plugin_id,
                format!(
                    "plugin protocol major {} is unsupported (expected {PROTOCOL_MAJOR})",
                    self.protocol_major
                ),
            ));
        }
        Ok(())
    }

    /// Validate the `package_digest` shape (it cannot be recomputed here, since
    /// it covers the sealed file inventory).
    pub fn validate_package_digest_shape(&self) -> Result<(), ContractError> {
        validate_digest(&self.package_digest, &self.plugin_id)
    }

    /// The offered domain API at exactly `api`/`major`, if any.
    pub fn offered_api(&self, api: &str, major: u64) -> Option<&DomainApi> {
        self.domain_apis
            .iter()
            .find(|domain| domain.api == api && domain.major == major)
    }

    /// Requested capabilities absent from `approved`, preserving request order.
    pub fn ungranted_capabilities(&self, approved: &[String]) -> Vec<String> {
        self.requested_capabilities
            .iter()
            .filter(|requested| !approved.iter().any(|a| a == *requested))
            .cloned()
            .collect()
    }

    /// Refuse a manifest document whose canonical encoding exceeds
    /// [`MANIFEST_DOCUMENT_LIMIT_BYTES`].
    pub fn check_document_bound(&self) -> Result<(), ContractError> {
        check_document_bound(&self.plugin_id, self, MANIFEST_DOCUMENT_LIMIT_BYTES)
    }
}

/// An operator's immutable installation: pinned package, configuration, grant
/// set and launch profile (`PluginInstallation/v1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginInstallation {
    /// Wire format; gated by [`PluginInstallation::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub package_digest: String,
    /// Immutable reference to the actual configuration object.
    pub configuration: PackageArtifactRef,
    pub launch_profile: String,
    pub approved_capabilities: Vec<String>,
    /// Canonical digest of this record excluding its own field.
    pub installation_digest: String,
}

impl PluginInstallation {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            PLUGIN_INSTALLATION_FAMILY,
            "PluginInstallation",
            &self.plugin_id,
        )
    }

    /// Refuse an installation document whose canonical encoding exceeds
    /// [`MANIFEST_DOCUMENT_LIMIT_BYTES`].
    pub fn check_document_bound(&self) -> Result<(), ContractError> {
        check_document_bound(
            &self.plugin_id,
            self,
            MANIFEST_DOCUMENT_LIMIT_BYTES,
        )
    }

    /// The canonical document to hash: this installation minus its own
    /// `installation_digest` field.
    pub fn canonical_document(&self) -> Result<Value, ContractError> {
        let mut value = serde_json::to_value(self).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                &self.plugin_id,
                format!("cannot serialize installation: {e}"),
            )
        })?;
        if let Some(object) = value.as_object_mut() {
            object.remove("installation_digest");
        }
        Ok(value)
    }

    /// Recompute the installation digest over [`Self::canonical_document`].
    pub fn recomputed_installation_digest(&self) -> Result<String, ContractError> {
        canonical_digest(&self.canonical_document()?)
    }

    /// Verify the recorded `installation_digest` against the recomputed value.
    pub fn verify_installation_digest(&self) -> Result<(), ContractError> {
        validate_digest(&self.installation_digest, &self.plugin_id)?;
        let recomputed = self.recomputed_installation_digest()?;
        if recomputed != self.installation_digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.plugin_id,
                format!(
                    "installation digest {} does not match recomputed {recomputed}",
                    self.installation_digest
                ),
            ));
        }
        Ok(())
    }

    /// Whether this installation approves `capability`.
    pub fn approves(&self, capability: &str) -> bool {
        self.approved_capabilities
            .iter()
            .any(|approved| approved == capability)
    }
}

/// Refuse a plugin ID mismatch or a requested capability the installation does
/// not approve ([`FailureCode::MissingCapability`]). Pure: no I/O.
pub fn missing_approved_capabilities(
    manifest: &PluginManifest,
    installation: &PluginInstallation,
) -> Result<(), ContractError> {
    if manifest.plugin_id != installation.plugin_id {
        return Err(FailureCode::MissingCapability.failure(
            &installation.plugin_id,
            format!(
                "installation plugin_id {} does not match manifest plugin_id {}",
                installation.plugin_id, manifest.plugin_id
            ),
        ));
    }
    for capability in &manifest.requested_capabilities {
        if !installation.approves(capability) {
            return Err(FailureCode::MissingCapability.failure(
                capability,
                format!(
                    "requested capability {capability:?} is not approved by installation {}",
                    installation.plugin_id
                ),
            ));
        }
    }
    Ok(())
}

/// Observed plugin health state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    /// Handshake, Describe compatibility and grants all succeeded.
    Ready,
    /// Serving but impaired.
    Degraded,
    /// Not serving (stale, disconnected or failed).
    Unavailable,
}

/// One health observation (`PluginHealth/v1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHealth {
    /// Wire format; gated by [`PluginHealth::version_gate`].
    pub format: String,
    pub plugin_id: String,
    /// The pinned binding this observation is about.
    pub binding: BindingPin,
    pub state: HealthState,
    /// Human-readable detail.
    pub detail: String,
    pub checked_at_unix_ms: u64,
}

impl PluginHealth {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            PLUGIN_HEALTH_FAMILY,
            "PluginHealth",
            &self.plugin_id,
        )
    }
}

/// One namespaced report snapshot entry with its payload digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSnapshotEntry {
    pub schema_namespace: String,
    pub schema_version: u64,
    pub payload: Value,
    /// Canonical digest of `payload`.
    pub payload_digest: String,
}

/// Snapshot recovery of a plugin's durable reports (`PluginSnapshot/v1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSnapshot {
    /// Wire format; gated by [`PluginSnapshot::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub binding: BindingPin,
    /// Sequence watermark the snapshot is consistent through.
    pub watermark: u64,
    pub snapshots: Vec<PluginSnapshotEntry>,
    /// Canonical digest of this snapshot excluding its own field.
    pub snapshot_digest: String,
}

impl PluginSnapshot {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            PLUGIN_SNAPSHOT_FAMILY,
            "PluginSnapshot",
            &self.plugin_id,
        )
    }

    /// The canonical document to hash: this snapshot minus its own
    /// `snapshot_digest` field.
    pub fn canonical_document(&self) -> Result<Value, ContractError> {
        let mut value = serde_json::to_value(self).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                &self.plugin_id,
                format!("cannot serialize snapshot: {e}"),
            )
        })?;
        if let Some(object) = value.as_object_mut() {
            object.remove("snapshot_digest");
        }
        Ok(value)
    }

    /// Recompute the snapshot digest over [`Self::canonical_document`].
    pub fn recomputed_snapshot_digest(&self) -> Result<String, ContractError> {
        canonical_digest(&self.canonical_document()?)
    }

    /// Verify the recorded `snapshot_digest` against the recomputed value.
    pub fn verify_snapshot_digest(&self) -> Result<(), ContractError> {
        validate_digest(&self.snapshot_digest, &self.plugin_id)?;
        let recomputed = self.recomputed_snapshot_digest()?;
        if recomputed != self.snapshot_digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.plugin_id,
                format!(
                    "snapshot digest {} does not match recomputed {recomputed}",
                    self.snapshot_digest
                ),
            ));
        }
        Ok(())
    }

    /// Verify every entry payload digest (canonical digest of its payload).
    pub fn verify_payload_digests(&self) -> Result<(), ContractError> {
        for entry in &self.snapshots {
            validate_digest(&entry.payload_digest, &entry.schema_namespace)?;
            let recomputed = canonical_digest(&entry.payload)?;
            if recomputed != entry.payload_digest {
                return Err(FailureCode::IntegrityMismatch.failure(
                    &entry.schema_namespace,
                    format!(
                        "snapshot payload digest {} does not match recomputed {recomputed}",
                        entry.payload_digest
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Refuse a snapshot whose entry payloads exceed either bound: each entry
    /// payload's canonical encoding must be within
    /// [`REPORT_PAYLOAD_LIMIT_BYTES`], and the total canonical byte length of
    /// every entry payload must be within [`PLUGIN_SNAPSHOT_PAYLOAD_LIMIT_BYTES`].
    pub fn check_payload_bound(&self) -> Result<(), ContractError> {
        let mut total: u64 = 0;
        for entry in &self.snapshots {
            let text = canonical_json(&entry.payload)?;
            let bytes = text.len() as u64;
            if bytes > REPORT_PAYLOAD_LIMIT_BYTES {
                return Err(FailureCode::InvalidRecord.failure(
                    &entry.schema_namespace,
                    format!(
                        "snapshot payload {bytes} bytes exceeds limit {REPORT_PAYLOAD_LIMIT_BYTES}"
                    ),
                ));
            }
            total += bytes;
        }
        if total > PLUGIN_SNAPSHOT_PAYLOAD_LIMIT_BYTES {
            return Err(FailureCode::InvalidRecord.failure(
                &self.plugin_id,
                format!(
                    "snapshot payloads total {total} bytes exceeds limit {PLUGIN_SNAPSHOT_PAYLOAD_LIMIT_BYTES}"
                ),
            ));
        }
        Ok(())
    }
}

/// Whether a report is a transient heartbeat or a durable observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Durability {
    /// Transient, drop-tolerant health/heartbeat update.
    Transient,
    /// Durable business observation.
    Durable,
}

/// A producer's report publication (`ReportPublication/v1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportPublication {
    /// Wire format; gated by [`ReportPublication::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub binding: BindingPin,
    pub schema_namespace: String,
    pub schema_version: u64,
    /// Monotonic per-binding sequence (>= 1).
    pub sequence: u64,
    pub reported_at_unix_ms: u64,
    /// Optional execution reference.
    pub execution_id: Option<String>,
    pub durability: Durability,
    pub payload: Value,
    /// Canonical digest of `payload`.
    pub payload_digest: String,
}

impl ReportPublication {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            REPORT_PUBLICATION_FAMILY,
            "ReportPublication",
            &self.plugin_id,
        )
    }

    /// Refuse a non-positive sequence (`InvalidRecord`).
    pub fn check_sequence(&self) -> Result<(), ContractError> {
        check_report_sequence(&self.plugin_id, self.sequence)
    }

    /// Verify `payload_digest` against the canonical digest of `payload`.
    pub fn verify_payload_digest(&self) -> Result<(), ContractError> {
        verify_payload_digest(&self.plugin_id, &self.payload, &self.payload_digest)
    }

    /// Refuse a payload whose canonical encoding exceeds
    /// [`REPORT_PAYLOAD_LIMIT_BYTES`].
    pub fn check_payload_bound(&self) -> Result<(), ContractError> {
        check_report_payload_bound(&self.plugin_id, &self.payload)
    }

    /// The deduplication identity `(plugin_id, binding generation, sequence)`.
    pub fn identity(&self) -> (&str, u64, u64) {
        (&self.plugin_id, self.binding.generation, self.sequence)
    }

    /// Gate version, sequence, payload digest and payload size.
    pub fn validate(&self) -> Result<(), ContractError> {
        self.version_gate()?;
        self.check_sequence()?;
        self.verify_payload_digest()?;
        self.check_payload_bound()
    }

    /// Deduplication check against `previous`: the same identity with identical
    /// payload digest/namespace/version is a harmless replay; the same identity
    /// with changed content is [`FailureCode::IdentityConflict`].
    pub fn check_replay(&self, previous: &ReportPublication) -> Result<(), ContractError> {
        if self.identity() == previous.identity() {
            let same = self.payload_digest == previous.payload_digest
                && self.schema_namespace == previous.schema_namespace
                && self.schema_version == previous.schema_version;
            if !same {
                return Err(FailureCode::IdentityConflict.failure(
                    &self.plugin_id,
                    format!(
                        "report identity {:?} reused with changed content",
                        self.identity()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// The hub-observed report envelope (`ReportEnvelope/v1`): a publication plus
/// the hub's observation timestamp.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportEnvelope {
    /// Wire format; gated by [`ReportEnvelope::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub binding: BindingPin,
    pub schema_namespace: String,
    pub schema_version: u64,
    /// Monotonic per-binding sequence (>= 1).
    pub sequence: u64,
    pub reported_at_unix_ms: u64,
    /// Hub observation timestamp, after `reported_at_unix_ms`.
    pub observed_at_unix_ms: u64,
    /// Optional execution reference.
    pub execution_id: Option<String>,
    pub durability: Durability,
    pub payload: Value,
    /// Canonical digest of `payload`.
    pub payload_digest: String,
}

impl ReportEnvelope {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            REPORT_ENVELOPE_FAMILY,
            "ReportEnvelope",
            &self.plugin_id,
        )
    }

    /// Refuse a non-positive sequence (`InvalidRecord`).
    pub fn check_sequence(&self) -> Result<(), ContractError> {
        check_report_sequence(&self.plugin_id, self.sequence)
    }

    /// Verify `payload_digest` against the canonical digest of `payload`.
    pub fn verify_payload_digest(&self) -> Result<(), ContractError> {
        verify_payload_digest(&self.plugin_id, &self.payload, &self.payload_digest)
    }

    /// Refuse a payload whose canonical encoding exceeds
    /// [`REPORT_PAYLOAD_LIMIT_BYTES`].
    pub fn check_payload_bound(&self) -> Result<(), ContractError> {
        check_report_payload_bound(&self.plugin_id, &self.payload)
    }

    /// The deduplication identity `(plugin_id, binding generation, sequence)`.
    pub fn identity(&self) -> (&str, u64, u64) {
        (&self.plugin_id, self.binding.generation, self.sequence)
    }

    /// Gate version, sequence, payload digest and payload size.
    pub fn validate(&self) -> Result<(), ContractError> {
        self.version_gate()?;
        self.check_sequence()?;
        self.verify_payload_digest()?;
        self.check_payload_bound()
    }

    /// Wrap `publication` with the hub's observation timestamp.
    pub fn from_publication(publication: &ReportPublication, observed_at_unix_ms: u64) -> Self {
        Self {
            format: REPORT_ENVELOPE_FORMAT.to_string(),
            plugin_id: publication.plugin_id.clone(),
            binding: publication.binding.clone(),
            schema_namespace: publication.schema_namespace.clone(),
            schema_version: publication.schema_version,
            sequence: publication.sequence,
            reported_at_unix_ms: publication.reported_at_unix_ms,
            observed_at_unix_ms,
            execution_id: publication.execution_id.clone(),
            durability: publication.durability,
            payload: publication.payload.clone(),
            payload_digest: publication.payload_digest.clone(),
        }
    }
}

/// Durable reconciliation status of a plugin operation
/// (`PluginOperationStatus/v1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginOperationStatus {
    /// Wire format; gated by [`PluginOperationStatus::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub binding: BindingPin,
    pub operation_id: String,
    pub input_digest: String,
    pub state: OperationState,
    /// Durable receipt when the operation is terminal.
    pub receipt: Option<ComponentReceipt>,
}

impl PluginOperationStatus {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            PLUGIN_OPERATION_STATUS_FAMILY,
            "PluginOperationStatus",
            &self.plugin_id,
        )
    }

    /// Whether the operation is terminal (completed, failed or cancelled).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            OperationState::Completed | OperationState::Failed | OperationState::Cancelled
        )
    }
}

/// A cancel request addressed to a pinned plugin operation
/// (`PluginCancelRequest/v1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCancelRequest {
    /// Wire format; gated by [`PluginCancelRequest::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub binding: BindingPin,
    /// Stable idempotency anchor.
    pub cancel_request_id: String,
    pub operation_id: String,
    pub deadline_unix_ms: u64,
    /// Fingerprint of the grant that authorized the request.
    pub grant_fingerprint: String,
}

impl PluginCancelRequest {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            PLUGIN_CANCEL_REQUEST_FAMILY,
            "PluginCancelRequest",
            &self.plugin_id,
        )
    }

    /// Stable identity check against `previous`: the same `cancel_request_id`
    /// with changed content is [`FailureCode::IdentityConflict`].
    pub fn check_replay(&self, previous: &PluginCancelRequest) -> Result<(), ContractError> {
        if self.cancel_request_id == previous.cancel_request_id {
            let same = self.format == previous.format
                && self.plugin_id == previous.plugin_id
                && self.binding == previous.binding
                && self.operation_id == previous.operation_id
                && self.deadline_unix_ms == previous.deadline_unix_ms
                && self.grant_fingerprint == previous.grant_fingerprint;
            if !same {
                return Err(FailureCode::IdentityConflict.failure(
                    &self.cancel_request_id,
                    "cancel_request_id reused with changed content",
                ));
            }
        }
        Ok(())
    }
}

/// The scope an action runs under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionScope {
    /// Authorizing directive id.
    pub directive_id: String,
    /// Authorizing directive version.
    pub directive_version: u64,
    /// Bound execution reference, when any.
    pub execution_id: Option<String>,
}

/// Who requested an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    /// A human UI request.
    Human,
    /// An autonomous policy request.
    Policy,
}

/// A journaled action intent (`ActionIntent/v1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionIntent {
    /// Wire format; gated by [`ActionIntent::version_gate`].
    pub format: String,
    pub plugin_id: String,
    pub binding: BindingPin,
    pub action_id: String,
    /// Durable operation identity; effect dedup anchor.
    pub operation_id: String,
    pub input: Value,
    /// Canonical digest of `input`.
    pub input_digest: String,
    pub deadline_unix_ms: u64,
    pub scope: ActionScope,
    /// Fingerprint of the authorizing grant.
    pub grant_fingerprint: String,
    pub actor: Actor,
}

impl ActionIntent {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_single_version(
            &self.format,
            ACTION_INTENT_FAMILY,
            "ActionIntent",
            &self.plugin_id,
        )
    }

    /// Verify `input_digest` against the canonical digest of `input`.
    pub fn verify_input_digest(&self) -> Result<(), ContractError> {
        validate_digest(&self.input_digest, &self.operation_id)?;
        let recomputed = canonical_digest(&self.input)?;
        if recomputed != self.input_digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.operation_id,
                format!(
                    "action input digest {} does not match recomputed {recomputed}",
                    self.input_digest
                ),
            ));
        }
        Ok(())
    }

    /// Identity check against `previous`: the same `operation_id` with a changed
    /// `input_digest` is [`FailureCode::IdentityConflict`].
    pub fn check_replay(&self, previous: &ActionIntent) -> Result<(), ContractError> {
        if self.operation_id == previous.operation_id && self.input_digest != previous.input_digest {
            return Err(FailureCode::IdentityConflict.failure(
                &self.operation_id,
                "action operation_id reused with changed input",
            ));
        }
        Ok(())
    }
}

/// Shared helper: refuse a document whose canonical encoding exceeds `limit`.
fn check_document_bound<T: Serialize>(
    subject: &str,
    record: &T,
    limit: u64,
) -> Result<(), ContractError> {
    let value = serde_json::to_value(record).map_err(|e| {
        FailureCode::InvalidRecord.failure(subject, format!("cannot serialize document: {e}"))
    })?;
    let text = canonical_json(&value)?;
    let bytes = text.len() as u64;
    if bytes > limit {
        return Err(FailureCode::InvalidRecord.failure(
            subject,
            format!("document {bytes} bytes exceeds limit {limit}"),
        ));
    }
    Ok(())
}

/// Shared helper: refuse a report sequence below 1.
fn check_report_sequence(subject: &str, sequence: u64) -> Result<(), ContractError> {
    if sequence < 1 {
        return Err(FailureCode::InvalidRecord.failure(
            subject,
            "report sequence must be >= 1",
        ));
    }
    Ok(())
}

/// Shared helper: verify a payload digest against the canonical digest.
fn verify_payload_digest(
    subject: &str,
    payload: &Value,
    digest: &str,
) -> Result<(), ContractError> {
    validate_digest(digest, subject)?;
    let recomputed = canonical_digest(payload)?;
    if recomputed != digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            subject,
            format!("payload digest {digest} does not match recomputed {recomputed}"),
        ));
    }
    Ok(())
}

/// Shared helper: refuse a report payload over [`REPORT_PAYLOAD_LIMIT_BYTES`].
fn check_report_payload_bound(subject: &str, payload: &Value) -> Result<(), ContractError> {
    let text = canonical_json(payload)?;
    let bytes = text.len() as u64;
    if bytes > REPORT_PAYLOAD_LIMIT_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            subject,
            format!("report payload {bytes} bytes exceeds limit {REPORT_PAYLOAD_LIMIT_BYTES}"),
        ));
    }
    Ok(())
}
