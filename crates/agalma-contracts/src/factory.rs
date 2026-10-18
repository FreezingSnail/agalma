//! `FactoryRuntime/v1` observation and `FactoryControl/v1` pause/resume/lookup
//! contracts (WEB.9, `agalma-rwn.9`).
//!
//! These are additive, pure wire contracts for the factory runtime/control
//! surface the local browser dashboard consumes:
//!
//! - [`FactoryRuntime`] is the read-only runtime observation the trusted hub
//!   composer builds from bound domain APIs (`factory.read/v1`).
//! - [`FactoryControlRequest`]/[`FactoryControlResponse`] are the typed
//!   pause/resume/lookup control seam (`factory.control/v1`).
//! - [`FactoryQueueCursor`] is the opaque, canonical-JSON queue page cursor.
//!
//! The contracts own no runtime policy, storage, listener, ledger access, or
//! control effect. They reuse the frozen `ExactU64`, [`SourceState`],
//! [`TaskPriority`]/[`TaskFamily`], and [`ExecutionPhase`]/[`ExecutionState`]
//! records and the shared [`FailureCode`] taxonomy. No new dependencies,
//! runtime imports, or failure-code variants are added.
//!
//! Exactness and bounds:
//!
//! - every identity-bearing `u64` (generation, observed time) travels as
//!   [`ExactU64`] decimal text, validated on construction and deserialization;
//! - an unknown total/revision/current execution is `null`, never `0`;
//! - snapshot limits default to [`FACTORY_SNAPSHOT_DEFAULT_LIMIT`] and accept
//!   [`FACTORY_SNAPSHOT_MIN_LIMIT`]`..=`[`FACTORY_SNAPSHOT_MAX_LIMIT`];
//! - a queue page holds at most [`FACTORY_PAGE_MAX_ENTRIES`] entries and the
//!   whole response is bounded by [`FACTORY_RESPONSE_MAX_BYTES`];
//! - the action identity is a stable `sha256:` digest of the exact validated
//!   action object, so a mutated request is [`FailureCode::IdentityConflict`];
//! - a malformed request is [`FailureCode::InvalidRecord`]; a wrong-family or
//!   wrong-version format is [`FailureCode::UnsupportedSchema`] /
//!   [`FailureCode::UnsupportedVersion`].
//!
//! WEB.11 is the sole trusted composer allowed to enable the
//! [`FactoryRuntimePermissions`] flags, using source health, the authenticated
//! caller's control-binding status, exclusive ownership, and the stop
//! observation. Transport and browser may only narrow the composed result.
//!
//! # Golden fixtures
//!
//! Independent golden fixtures live under `fixtures/factory-runtime-v1/` and
//! `fixtures/factory-control-v1/` and are consumed by the accepting test
//! `tests/factory_contracts.rs`. They are hand-written, not serialized from
//! these types. `runtime-statuses.json` covers every status and source state;
//! `runtime-unavailable.json`, `boundaries.json`, `permissions.json`, and
//! `eligible-queue.json` cover the null-identity, boundary, permission, and
//! empty/partial/paged cases; `cursor.json` freezes the exact cursor encoding;
//! `requests.json`/`responses.json` cover the control requests and the four
//! result variants; and `runtime-invalid.json`/`control-invalid.json` carry
//! malformed/overlimit/conflict cases with their expected failure codes. The
//! `request_digest` literals are `sha256:` of the canonical JSON of the exact
//! validated action object, computed independently of this implementation.
//! Every nullable response key must be present (it may be JSON `null`).
//!
//! This module is pure: no I/O, no vendor types.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::control_plane::{ExactU64, SourceState};
use crate::digest::{canonical_digest, canonical_json, is_sha256, FailureCode};
use crate::error::ContractError;
use crate::execution::{ExecutionPhase, ExecutionState};
use crate::taskqueue::{TaskFamily, TaskPriority};

/// `FactoryRuntime/v1` observation wire format.
pub const FACTORY_RUNTIME_FORMAT: &str = "FactoryRuntime/v1";
/// Basename of the runtime observation format family.
pub const FACTORY_RUNTIME_FAMILY: &str = "FactoryRuntime";
/// `FactoryControl/v1` pause/resume/lookup wire format.
pub const FACTORY_CONTROL_FORMAT: &str = "FactoryControl/v1";
/// Basename of the control format family.
pub const FACTORY_CONTROL_FAMILY: &str = "FactoryControl";
/// `FactoryQueueCursor/v1` opaque queue cursor wire format.
pub const FACTORY_QUEUE_CURSOR_FORMAT: &str = "FactoryQueueCursor/v1";
/// Basename of the queue cursor format family.
pub const FACTORY_QUEUE_CURSOR_FAMILY: &str = "FactoryQueueCursor";

/// Scoped capability authorizing the runtime snapshot and receipt lookup.
pub const FACTORY_READ_CAPABILITY: &str = "factory.read/v1";
/// Scoped capability authorizing pause and resume only.
pub const FACTORY_CONTROL_CAPABILITY: &str = "factory.control/v1";

/// Snapshot page default when the request omits `limit`.
pub const FACTORY_SNAPSHOT_DEFAULT_LIMIT: u32 = 50;
/// Smallest accepted snapshot page size.
pub const FACTORY_SNAPSHOT_MIN_LIMIT: u32 = 1;
/// Largest accepted snapshot page size.
pub const FACTORY_SNAPSHOT_MAX_LIMIT: u32 = 100;
/// Hard cap on entries in one eligible page.
pub const FACTORY_PAGE_MAX_ENTRIES: usize = 100;
/// Hard cap on one whole serialized response (256 KiB).
pub const FACTORY_RESPONSE_MAX_BYTES: u64 = 256 * 1024;
/// Hard cap on one queue cursor token (4096 bytes).
pub const FACTORY_CURSOR_MAX_BYTES: usize = 4096;
/// Smallest action id (1 ASCII byte).
pub const FACTORY_ACTION_ID_MIN_BYTES: usize = 1;
/// Largest action id (128 ASCII bytes).
pub const FACTORY_ACTION_ID_MAX_BYTES: usize = 128;
/// Largest confirmation text (64 UTF-8 bytes).
pub const FACTORY_CONFIRMATION_MAX_BYTES: usize = 64;
/// Largest reason text (2048 UTF-8 bytes).
pub const FACTORY_REASON_MAX_BYTES: usize = 2048;
/// Largest opaque configured id (`repository_id`/`state_id`, 256 bytes).
pub const FACTORY_OPAQUE_ID_MAX_BYTES: usize = 256;
/// Largest nullable runtime instance id (128 bytes).
pub const FACTORY_RUNTIME_INSTANCE_ID_MAX_BYTES: usize = 128;
/// Largest opaque revision string (128 bytes).
pub const FACTORY_REVISION_MAX_BYTES: usize = 128;
/// Largest eligible entry title (1024 bytes).
pub const FACTORY_TITLE_MAX_BYTES: usize = 1024;
/// Largest execution/task id (256 bytes).
pub const FACTORY_ID_MAX_BYTES: usize = 256;

// --- format gating ----------------------------------------------------------

/// Parse a canonical `<family>/vN` into `N`; the error string explains a
/// mismatch. Only the exact canonical spelling is accepted: a non-`vN` token,
/// a leading-zero alias such as `v01`, or an overflow fails.
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
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("version {version:?} is not `vN`"));
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(format!(
            "version {version:?} is not the canonical `vN` spelling"
        ));
    }
    digits
        .parse::<u64>()
        .map_err(|_| format!("version {version:?} is not `vN`"))
}

/// Gate a single-version family: right family/wrong version is
/// [`FailureCode::UnsupportedVersion`]; an unrecognized family is
/// [`FailureCode::UnsupportedSchema`].
fn gate_format(
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
        Err(reason) => Err(FailureCode::UnsupportedSchema
            .failure(subject, format!("unrecognized {label} format: {reason}"))),
    }
}

/// Gate the `FactoryRuntime/v1` format.
fn gate_runtime_format(format: &str) -> Result<(), ContractError> {
    gate_format(
        format,
        FACTORY_RUNTIME_FAMILY,
        "FactoryRuntime",
        FACTORY_RUNTIME_FORMAT,
    )
}

/// Gate the `FactoryControl/v1` format.
fn gate_control_format(format: &str) -> Result<(), ContractError> {
    gate_format(
        format,
        FACTORY_CONTROL_FAMILY,
        "FactoryControl",
        FACTORY_CONTROL_FORMAT,
    )
}

/// Gate the `FactoryQueueCursor/v1` format.
fn gate_cursor_format(format: &str) -> Result<(), ContractError> {
    gate_format(
        format,
        FACTORY_QUEUE_CURSOR_FAMILY,
        "FactoryQueueCursor",
        FACTORY_QUEUE_CURSOR_FORMAT,
    )
}

// --- shared helpers ---------------------------------------------------------

fn invalid(subject: &str, message: impl AsRef<str>) -> ContractError {
    FailureCode::InvalidRecord.failure(subject, message)
}

/// Deserialize `value`, mapping every serde error to `InvalidRecord`.
fn decode<T: serde::de::DeserializeOwned>(value: Value, subject: &str) -> Result<T, ContractError> {
    serde_json::from_value(value).map_err(|e| invalid(subject, format!("cannot decode: {e}")))
}

/// A required-nullable field: the key must be present and its value may be JSON
/// `null`. A missing key is a deserialization error ("an omitted required
/// nullable field is malformed"), so every nullable response key must exist.
fn present_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Require a nonempty string of at most `max` UTF-8 bytes.
fn check_text(raw: &str, subject: &str, max: usize) -> Result<(), ContractError> {
    if raw.is_empty() {
        return Err(invalid(subject, "must not be empty"));
    }
    if raw.len() > max {
        return Err(invalid(
            subject,
            format!("is {} bytes, exceeding the {max}-byte bound", raw.len()),
        ));
    }
    Ok(())
}

/// Require a nullable string of at most `max` UTF-8 bytes when present.
fn check_optional_text(
    raw: &Option<String>,
    subject: &str,
    max: usize,
) -> Result<(), ContractError> {
    if let Some(text) = raw {
        if text.len() > max {
            return Err(invalid(
                subject,
                format!("is {} bytes, exceeding the {max}-byte bound", text.len()),
            ));
        }
    }
    Ok(())
}

/// Require a string of at most `max` UTF-8 bytes (an empty value is bounded but
/// permitted; only the length is constrained).
fn check_bounded_text(raw: &str, subject: &str, max: usize) -> Result<(), ContractError> {
    if raw.len() > max {
        return Err(invalid(
            subject,
            format!("is {} bytes, exceeding the {max}-byte bound", raw.len()),
        ));
    }
    Ok(())
}

/// Require an opaque configured id: nonempty, at most 256 bytes, and not a
/// private absolute filesystem path.
fn check_opaque_id(raw: &str, subject: &str) -> Result<(), ContractError> {
    check_text(raw, subject, FACTORY_OPAQUE_ID_MAX_BYTES)?;
    if looks_like_private_path(raw) {
        return Err(invalid(
            subject,
            "must be an opaque identifier, not a private absolute path",
        ));
    }
    Ok(())
}

/// Whether `raw` looks like an absolute filesystem path or a traversal.
fn looks_like_private_path(raw: &str) -> bool {
    if raw.starts_with('/') || raw.starts_with('\\') {
        return true;
    }
    // A Windows drive root (`C:\` / `C:/`).
    let bytes = raw.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        if bytes[2] == b'\\' || bytes[2] == b'/' {
            return true;
        }
    }
    // A parent-directory segment.
    raw.split(['/', '\\']).any(|segment| segment == "..")
}

/// Require an action id: 1..=128 ASCII bytes matching `[A-Za-z0-9._-]+`.
fn check_action_id(raw: &str) -> Result<(), ContractError> {
    if raw.len() < FACTORY_ACTION_ID_MIN_BYTES || raw.len() > FACTORY_ACTION_ID_MAX_BYTES {
        return Err(invalid(
            "action_id",
            format!(
                "is {} bytes, outside {FACTORY_ACTION_ID_MIN_BYTES}..={FACTORY_ACTION_ID_MAX_BYTES}",
                raw.len()
            ),
        ));
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid(
            "action_id",
            "must match [A-Za-z0-9._-]+ (ASCII only)",
        ));
    }
    Ok(())
}

/// Serialized JSON byte length (compact), refusing a serialization failure.
fn response_len(value: &impl Serialize) -> Result<u64, ContractError> {
    serde_json::to_string(value)
        .map(|text| text.len() as u64)
        .map_err(|e| invalid("response", format!("cannot serialize response: {e}")))
}

/// Refuse a response larger than [`FACTORY_RESPONSE_MAX_BYTES`].
fn check_response_bound(value: &impl Serialize, subject: &str) -> Result<(), ContractError> {
    let bytes = response_len(value)?;
    if bytes > FACTORY_RESPONSE_MAX_BYTES {
        return Err(invalid(
            subject,
            format!("serializes to {bytes} bytes, exceeding limit {FACTORY_RESPONSE_MAX_BYTES}"),
        ));
    }
    Ok(())
}

/// Refuse a request digest that is not a `sha256:` identity matching the exact
/// canonical JSON of the validated action request.
fn verify_request_digest(
    request: &FactoryActionRequest,
    request_digest: &str,
    subject: &str,
) -> Result<(), ContractError> {
    if !is_sha256(request_digest) {
        return Err(invalid(
            subject,
            format!("request_digest {request_digest:?} is not `sha256:<64 lower hex digits>`"),
        ));
    }
    let actual = request.request_digest()?;
    if actual != request_digest {
        return Err(FailureCode::IdentityConflict.failure(
            subject,
            format!(
                "request_digest {request_digest} does not match the recorded request ({actual})"
            ),
        ));
    }
    Ok(())
}

/// Refuse a recorded request whose inner action id differs from the receipt's
/// outer action id.
fn check_request_action_id(
    request: &FactoryActionRequest,
    outer_action_id: &str,
) -> Result<(), ContractError> {
    if request.action_id != outer_action_id {
        return Err(invalid(
            outer_action_id,
            format!(
                "recorded request action_id {:?} does not equal the outer action_id {outer_action_id:?}",
                request.action_id
            ),
        ));
    }
    Ok(())
}

// --- runtime observation ----------------------------------------------------

/// Observed factory runtime status (exact set; no alias spellings).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FactoryRuntimeStatus {
    /// Starting up before the first observation.
    Starting,
    /// Recovering durable state.
    Recovering,
    /// Recovered and waiting for work.
    Idle,
    /// Executing work.
    Running,
    /// Paused by operator control.
    Paused,
    /// Failed and not serving.
    Failed,
    /// Not available (no observation).
    Unavailable,
}

/// The five named runtime observation sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactorySourceName {
    Runtime,
    Worker,
    Queue,
    Execution,
    Stop,
}

impl FactorySourceName {
    /// Every required source name.
    pub const ALL: [FactorySourceName; 5] = [
        FactorySourceName::Runtime,
        FactorySourceName::Worker,
        FactorySourceName::Queue,
        FactorySourceName::Execution,
        FactorySourceName::Stop,
    ];

    /// Index into [`Self::ALL`].
    const fn index(self) -> usize {
        match self {
            FactorySourceName::Runtime => 0,
            FactorySourceName::Worker => 1,
            FactorySourceName::Queue => 2,
            FactorySourceName::Execution => 3,
            FactorySourceName::Stop => 4,
        }
    }
}

/// One named runtime source observation.
///
/// A duplicate name or an absent name is malformed; an unknown record always
/// carries an explicit [`SourceState`] rather than being omitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryRuntimeSource {
    pub source: FactorySourceName,
    pub state: SourceState,
    #[serde(deserialize_with = "present_nullable")]
    pub detail: Option<String>,
    #[serde(deserialize_with = "present_nullable")]
    pub observed_at_unix_ms: Option<ExactU64>,
}

impl FactoryRuntimeSource {
    /// Validate the nullable detail bound.
    pub fn validate(&self) -> Result<(), ContractError> {
        check_optional_text(&self.detail, "source.detail", FACTORY_REASON_MAX_BYTES)
    }
}

/// The observed factory stop latch (live metadata, not replayed history).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryStopObservation {
    pub stopped: bool,
    #[serde(deserialize_with = "present_nullable")]
    pub reason: Option<String>,
    pub observed_at_unix_ms: ExactU64,
}

impl FactoryStopObservation {
    /// Validate the nullable reason bound.
    pub fn validate(&self) -> Result<(), ContractError> {
        check_optional_text(&self.reason, "stop.reason", FACTORY_REASON_MAX_BYTES)
    }
}

/// A bounded observation of one current execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryExecutionObservation {
    pub execution_id: String,
    pub task_id: String,
    pub generation: ExactU64,
    pub phase: ExecutionPhase,
    pub state: ExecutionState,
    /// Unknown update time is `null`, never `0`.
    #[serde(deserialize_with = "present_nullable")]
    pub updated_at_unix_ms: Option<ExactU64>,
}

impl FactoryExecutionObservation {
    /// Validate the id bounds.
    pub fn validate(&self) -> Result<(), ContractError> {
        check_text(&self.execution_id, "execution_id", FACTORY_ID_MAX_BYTES)?;
        check_text(&self.task_id, "task_id", FACTORY_ID_MAX_BYTES)
    }
}

/// One eligible queued task entry. `entries` is the authoritative sort order
/// (the live source sorts by `task_id`); it is display order, not dispatch
/// priority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryEligibleEntry {
    pub task_id: String,
    pub title: String,
    pub priority: TaskPriority,
    pub family: TaskFamily,
}

impl FactoryEligibleEntry {
    /// Validate the entry bounds.
    pub fn validate(&self) -> Result<(), ContractError> {
        check_text(&self.task_id, "eligible.task_id", FACTORY_ID_MAX_BYTES)?;
        check_bounded_text(&self.title, "eligible.title", FACTORY_TITLE_MAX_BYTES)
    }
}

/// The bounded eligible-task page. Unknown `revision`/`total` remain `null`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryEligiblePage {
    pub entries: Vec<FactoryEligibleEntry>,
    #[serde(deserialize_with = "present_nullable")]
    pub revision: Option<String>,
    #[serde(deserialize_with = "present_nullable")]
    pub total: Option<ExactU64>,
    #[serde(deserialize_with = "present_nullable")]
    pub next_cursor: Option<String>,
}

impl FactoryEligiblePage {
    /// Validate the page bounds (entries, revision, cursor).
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.entries.len() > FACTORY_PAGE_MAX_ENTRIES {
            return Err(invalid(
                "eligible.entries",
                format!(
                    "holds {} entries, exceeding limit {FACTORY_PAGE_MAX_ENTRIES}",
                    self.entries.len()
                ),
            ));
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        check_optional_text(
            &self.revision,
            "eligible.revision",
            FACTORY_REVISION_MAX_BYTES,
        )?;
        if let Some(cursor) = &self.next_cursor {
            if cursor.len() > FACTORY_CURSOR_MAX_BYTES {
                return Err(invalid(
                    "eligible.next_cursor",
                    format!(
                        "is {} bytes, exceeding the {FACTORY_CURSOR_MAX_BYTES}-byte bound",
                        cursor.len()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Runtime-source permission flags. They default to `false`; only the trusted
/// WEB.11 composer may enable them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryRuntimePermissions {
    pub pause: bool,
    pub resume: bool,
}

impl Default for FactoryRuntimePermissions {
    fn default() -> Self {
        Self {
            pause: false,
            resume: false,
        }
    }
}

/// The `FactoryRuntime/v1` observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryRuntime {
    pub format: String,
    /// Opaque configured repository id (nonempty, not a private path).
    pub repository_id: String,
    /// Opaque configured state id (nonempty, not a private path).
    pub state_id: String,
    /// Nullable runtime instance identity (unknown when unavailable).
    #[serde(deserialize_with = "present_nullable")]
    pub runtime_instance_id: Option<String>,
    pub observed_at_unix_ms: ExactU64,
    pub status: FactoryRuntimeStatus,
    #[serde(deserialize_with = "present_nullable")]
    pub reason: Option<String>,
    /// Exactly the five named unique [`FactorySourceName`] records.
    pub sources: Vec<FactoryRuntimeSource>,
    #[serde(deserialize_with = "present_nullable")]
    pub stop: Option<FactoryStopObservation>,
    #[serde(deserialize_with = "present_nullable")]
    pub current_execution: Option<FactoryExecutionObservation>,
    pub eligible: FactoryEligiblePage,
    pub permissions: FactoryRuntimePermissions,
}

impl FactoryRuntime {
    /// Gate the response format.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_runtime_format(&self.format)
    }

    /// Validate every field bound, the exact five-source invariant, and the
    /// whole-response size bound.
    pub fn validate(&self) -> Result<(), ContractError> {
        self.version_gate()?;
        check_opaque_id(&self.repository_id, "repository_id")?;
        check_opaque_id(&self.state_id, "state_id")?;
        check_optional_text(
            &self.runtime_instance_id,
            "runtime_instance_id",
            FACTORY_RUNTIME_INSTANCE_ID_MAX_BYTES,
        )?;
        check_optional_text(&self.reason, "reason", FACTORY_REASON_MAX_BYTES)?;

        if self.sources.len() != FactorySourceName::ALL.len() {
            return Err(invalid(
                "sources",
                format!(
                    "holds {} records, expected exactly {}",
                    self.sources.len(),
                    FactorySourceName::ALL.len()
                ),
            ));
        }
        let mut seen = [false; 5];
        for source in &self.sources {
            source.validate()?;
            let index = source.source.index();
            if seen[index] {
                return Err(invalid(
                    "sources",
                    format!("duplicate source {:?}", source.source),
                ));
            }
            seen[index] = true;
        }
        if !seen.iter().all(|present| *present) {
            return Err(invalid(
                "sources",
                "must name every required source exactly once",
            ));
        }

        if let Some(stop) = &self.stop {
            stop.validate()?;
        }
        if let Some(execution) = &self.current_execution {
            execution.validate()?;
        }
        self.eligible.validate()?;

        check_response_bound(self, FACTORY_RUNTIME_FORMAT)
    }
}

/// The observational runtime source seam.
///
/// The implementation is observational and owns authoritative sorting, paging,
/// and revision validation; an adapter may not infer queue eligibility. The
/// source sorts eligible entries by `task_id`.
pub trait RuntimeObservationSource: Send + Sync {
    /// Produce one bounded runtime snapshot for `request`.
    fn snapshot(&self, request: &FactoryRuntimeRequest) -> Result<FactoryRuntime, ContractError>;
}

// --- runtime request --------------------------------------------------------

/// A bounded `FactoryRuntime/v1` snapshot request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryRuntimeRequest {
    pub format: String,
    pub operation: String,
    /// `null`/omitted uses [`FACTORY_SNAPSHOT_DEFAULT_LIMIT`].
    #[serde(default)]
    pub limit: Option<u32>,
    /// Opaque continuation cursor (at most [`FACTORY_CURSOR_MAX_BYTES`] bytes).
    #[serde(default)]
    pub cursor: Option<String>,
}

impl FactoryRuntimeRequest {
    /// Gate the format and operation and validate the paging bounds.
    pub fn validate(&self) -> Result<(), ContractError> {
        gate_runtime_format(&self.format)?;
        if self.operation != "snapshot" {
            return Err(invalid(
                "operation",
                format!("unknown FactoryRuntime operation {:?}", self.operation),
            ));
        }
        self.effective_limit()?;
        if let Some(cursor) = &self.cursor {
            if cursor.len() > FACTORY_CURSOR_MAX_BYTES {
                return Err(invalid(
                    "cursor",
                    format!(
                        "is {} bytes, exceeding the {FACTORY_CURSOR_MAX_BYTES}-byte bound",
                        cursor.len()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The effective page size: the default, or a requested `1..=100`.
    pub fn effective_limit(&self) -> Result<u32, ContractError> {
        match self.limit {
            None => Ok(FACTORY_SNAPSHOT_DEFAULT_LIMIT),
            Some(limit)
                if (FACTORY_SNAPSHOT_MIN_LIMIT..=FACTORY_SNAPSHOT_MAX_LIMIT).contains(&limit) =>
            {
                Ok(limit)
            }
            Some(limit) => Err(invalid(
                "limit",
                format!(
                    "snapshot limit {limit} is outside {FACTORY_SNAPSHOT_MIN_LIMIT}..={FACTORY_SNAPSHOT_MAX_LIMIT}"
                ),
            )),
        }
    }
}

// --- control requests -------------------------------------------------------

/// A `FactoryControl/v1` lookup request (`factory.read/v1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryLookupRequest {
    pub format: String,
    pub operation: String,
    pub action_id: String,
}

impl FactoryLookupRequest {
    /// Gate the format, operation, and action id.
    pub fn validate(&self) -> Result<(), ContractError> {
        gate_control_format(&self.format)?;
        if self.operation != "lookup" {
            return Err(invalid(
                "operation",
                format!(
                    "unknown FactoryControl lookup operation {:?}",
                    self.operation
                ),
            ));
        }
        check_action_id(&self.action_id)
    }
}

/// A `FactoryControl/v1` action request (`factory.control/v1`).
///
/// `confirmation` must be present (nullable) in the wire object: an omitted
/// required nullable field is malformed. It is preserved exactly for the
/// request digest; the existing trimmed confirmation guard decides acceptance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryActionRequest {
    pub format: String,
    pub operation: String,
    pub action_id: String,
    /// Required-nullable: `null` or a string of at most 64 UTF-8 bytes.
    #[serde(deserialize_with = "present_nullable")]
    pub confirmation: Option<String>,
}

impl FactoryActionRequest {
    /// Gate the format, operation, action id, and confirmation bound.
    pub fn validate(&self) -> Result<(), ContractError> {
        gate_control_format(&self.format)?;
        if self.operation != "pause" && self.operation != "resume" {
            return Err(invalid(
                "operation",
                format!(
                    "unknown FactoryControl action operation {:?}",
                    self.operation
                ),
            ));
        }
        check_action_id(&self.action_id)?;
        check_optional_text(
            &self.confirmation,
            "confirmation",
            FACTORY_CONFIRMATION_MAX_BYTES,
        )
    }

    /// The exact validated action object.
    /// The exact validated action object. The request is validated first, so a
    /// malformed operation/format/overlimit request cannot be serialized into
    /// an action object.
    pub fn request_value(&self) -> Result<Value, ContractError> {
        self.validate()?;
        serde_json::to_value(self)
            .map_err(|e| invalid(&self.action_id, format!("cannot encode request: {e}")))
    }

    /// The `sha256:` digest of the exact validated action object (never the
    /// outer grant, session, CSRF, operation, or deadline).
    ///
    /// The action request is validated (through [`Self::request_value`]) first,
    /// so the digest helper never produces an identity for a malformed
    /// lookup/cancel/extra request.
    pub fn request_digest(&self) -> Result<String, ContractError> {
        canonical_digest(&self.request_value()?)
    }
}

/// A parsed `FactoryControl/v1` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum FactoryControlRequest {
    Lookup(FactoryLookupRequest),
    Action(FactoryActionRequest),
}

impl FactoryControlRequest {
    /// Parse and validate a control request, dispatching on the operation.
    pub fn parse(text: &str) -> Result<Self, ContractError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| invalid(FACTORY_CONTROL_FORMAT, format!("malformed request: {e}")))?;
        Self::from_value(value)
    }

    /// Parse and validate a control request from an already-decoded value.
    pub fn from_value(value: Value) -> Result<Self, ContractError> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid(FACTORY_CONTROL_FORMAT, "request must be a JSON object"))?;
        let format = object
            .get("format")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("format", "request must carry a string `format`"))?;
        gate_control_format(format)?;
        let operation = object
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("operation", "request must carry a string `operation`"))?;
        match operation {
            "lookup" => {
                let request: FactoryLookupRequest = decode(value, "FactoryControl/v1 lookup")?;
                request.validate()?;
                Ok(FactoryControlRequest::Lookup(request))
            }
            "pause" | "resume" => {
                let request: FactoryActionRequest = decode(value, "FactoryControl/v1 action")?;
                request.validate()?;
                Ok(FactoryControlRequest::Action(request))
            }
            other => Err(invalid(
                "operation",
                format!("unknown FactoryControl operation {other:?}"),
            )),
        }
    }
}

// --- control responses ------------------------------------------------------

/// The `FactoryControl/v1` result tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactoryControlResult {
    /// A terminal receipt (applied or denied).
    Receipt,
    /// An effect whose outcome is not confirmed; never a terminal receipt.
    Unconfirmed,
    /// No receipt exists for the action id.
    NotFound,
}

/// Receipt outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactoryControlOutcome {
    Applied,
    Denied,
}

/// The only actor a factory control receipt records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactoryControlActor {
    Human,
}

/// The `EffectUnconfirmed` code carried by the unconfirmed variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FactoryUnconfirmedCode {
    EffectUnconfirmed,
}

/// The terminal receipt body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryControlReceiptBody {
    pub action_id: String,
    /// The exact validated action object whose digest is `request_digest`.
    pub request: FactoryActionRequest,
    pub request_digest: String,
    pub outcome: FactoryControlOutcome,
    #[serde(deserialize_with = "present_nullable")]
    pub reason: Option<String>,
    pub ledger_action_id: String,
    pub actor: FactoryControlActor,
    pub recorded_at_unix_ms: ExactU64,
}

impl FactoryControlReceiptBody {
    /// Validate the bounds, the embedded action request, and that
    /// `request_digest` matches `request`.
    pub fn validate(&self) -> Result<(), ContractError> {
        check_action_id(&self.action_id)?;
        self.request.validate()?;
        check_request_action_id(&self.request, &self.action_id)?;
        check_optional_text(&self.reason, "receipt.reason", FACTORY_REASON_MAX_BYTES)?;
        check_text(
            &self.ledger_action_id,
            "ledger_action_id",
            FACTORY_ID_MAX_BYTES,
        )?;
        verify_request_digest(&self.request, &self.request_digest, &self.action_id)
    }
}

/// A terminal `FactoryControl/v1` receipt response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryControlReceiptResponse {
    pub format: String,
    pub result: FactoryControlResult,
    pub receipt: FactoryControlReceiptBody,
    #[serde(deserialize_with = "present_nullable")]
    pub observed_stop: Option<FactoryStopObservation>,
}

impl FactoryControlReceiptResponse {
    /// Validate the receipt and reaction metadata.
    pub fn validate(&self) -> Result<(), ContractError> {
        gate_control_format(&self.format)?;
        if self.result != FactoryControlResult::Receipt {
            return Err(invalid(
                "result",
                "receipt variant must carry result `receipt`",
            ));
        }
        self.receipt.validate()?;
        if let Some(stop) = &self.observed_stop {
            stop.validate()?;
        }
        Ok(())
    }
}

/// An `unconfirmed` `FactoryControl/v1` response: no terminal receipt, no
/// auto-reexecution, and no terminal-looking fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryControlUnconfirmedResponse {
    pub format: String,
    pub result: FactoryControlResult,
    pub action_id: String,
    pub request: FactoryActionRequest,
    pub request_digest: String,
    pub code: FactoryUnconfirmedCode,
    pub reason: String,
    #[serde(deserialize_with = "present_nullable")]
    pub observed_stop: Option<FactoryStopObservation>,
}

impl FactoryControlUnconfirmedResponse {
    /// Validate the unconfirmed shape and request digest.
    pub fn validate(&self) -> Result<(), ContractError> {
        gate_control_format(&self.format)?;
        if self.result != FactoryControlResult::Unconfirmed {
            return Err(invalid(
                "result",
                "unconfirmed variant must carry result `unconfirmed`",
            ));
        }
        check_action_id(&self.action_id)?;
        self.request.validate()?;
        check_request_action_id(&self.request, &self.action_id)?;
        if self.reason.is_empty() || self.reason.len() > FACTORY_REASON_MAX_BYTES {
            return Err(invalid(
                "reason",
                format!(
                    "is {} bytes, expected 1..={FACTORY_REASON_MAX_BYTES}",
                    self.reason.len()
                ),
            ));
        }
        verify_request_digest(&self.request, &self.request_digest, &self.action_id)?;
        if let Some(stop) = &self.observed_stop {
            stop.validate()?;
        }
        Ok(())
    }
}

/// A `not_found` `FactoryControl/v1` response: no receipt exists for the
/// action id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryControlNotFoundResponse {
    pub format: String,
    pub result: FactoryControlResult,
    pub action_id: String,
    #[serde(deserialize_with = "present_nullable")]
    pub observed_stop: Option<FactoryStopObservation>,
}

impl FactoryControlNotFoundResponse {
    /// Validate the not-found shape.
    pub fn validate(&self) -> Result<(), ContractError> {
        gate_control_format(&self.format)?;
        if self.result != FactoryControlResult::NotFound {
            return Err(invalid(
                "result",
                "not_found variant must carry result `not_found`",
            ));
        }
        check_action_id(&self.action_id)?;
        if let Some(stop) = &self.observed_stop {
            stop.validate()?;
        }
        Ok(())
    }
}

/// A `FactoryControl/v1` response with exactly one of three distinct results.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FactoryControlResponse {
    /// A terminal receipt (applied or denied).
    Receipt(FactoryControlReceiptResponse),
    /// An unconfirmed effect; never a terminal receipt.
    Unconfirmed(FactoryControlUnconfirmedResponse),
    /// No receipt for the action id.
    NotFound(FactoryControlNotFoundResponse),
}

impl FactoryControlResponse {
    /// Validate the selected variant and the whole-response size bound.
    pub fn validate(&self) -> Result<(), ContractError> {
        // Bound first: a hostile response is refused on size regardless of its
        // shape, before any digest work.
        check_response_bound(self, FACTORY_CONTROL_FORMAT)?;
        match self {
            FactoryControlResponse::Receipt(response) => response.validate()?,
            FactoryControlResponse::Unconfirmed(response) => response.validate()?,
            FactoryControlResponse::NotFound(response) => response.validate()?,
        }
        Ok(())
    }

    /// Parse and validate a control response from an already-decoded value.
    pub fn from_value(value: Value) -> Result<Self, ContractError> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid(FACTORY_CONTROL_FORMAT, "response must be a JSON object"))?;
        let format = object
            .get("format")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("format", "response must carry a string `format`"))?;
        gate_control_format(format)?;
        let result = object
            .get("result")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("result", "response must carry a string `result`"))?;
        let response = match result {
            "receipt" => {
                FactoryControlResponse::Receipt(decode(value, "FactoryControl/v1 receipt")?)
            }
            "unconfirmed" => {
                FactoryControlResponse::Unconfirmed(decode(value, "FactoryControl/v1 unconfirmed")?)
            }
            "not_found" => {
                FactoryControlResponse::NotFound(decode(value, "FactoryControl/v1 not_found")?)
            }
            other => {
                return Err(invalid(
                    "result",
                    format!("unknown FactoryControl result {other:?}"),
                ))
            }
        };
        response.validate()?;
        Ok(response)
    }

    /// Parse a wire JSON string, then [`Self::from_value`].
    pub fn from_wire_str(text: &str) -> Result<Self, ContractError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| invalid(FACTORY_CONTROL_FORMAT, format!("malformed response: {e}")))?;
        Self::from_value(value)
    }
}

impl<'de> Deserialize<'de> for FactoryControlResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        FactoryControlResponse::from_value(value).map_err(serde::de::Error::custom)
    }
}

// --- queue cursor -----------------------------------------------------------

/// The opaque `FactoryQueueCursor/v1` page cursor.
///
/// Every field is bounded and nonempty and pins the collection/runtime. The
/// token is the canonical JSON UTF-8 text of this object. Opaque revisions are
/// not browser-comparable counters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryQueueCursor {
    pub format: String,
    pub repository_id: String,
    pub state_id: String,
    pub runtime_instance_id: String,
    pub revision: String,
    pub after: String,
}

impl FactoryQueueCursor {
    /// Gate the format and validate every bounded/nonempty field.
    pub fn validate(&self) -> Result<(), ContractError> {
        gate_cursor_format(&self.format)?;
        check_opaque_id(&self.repository_id, "repository_id")?;
        check_opaque_id(&self.state_id, "state_id")?;
        check_text(
            &self.runtime_instance_id,
            "runtime_instance_id",
            FACTORY_RUNTIME_INSTANCE_ID_MAX_BYTES,
        )?;
        check_text(&self.revision, "revision", FACTORY_REVISION_MAX_BYTES)?;
        check_text(&self.after, "after", FACTORY_ID_MAX_BYTES)
    }

    /// Encode as opaque canonical JSON UTF-8 text, refusing a token beyond the
    /// byte bound.
    pub fn encode(&self) -> Result<String, ContractError> {
        self.validate()?;
        let value = serde_json::to_value(self)
            .map_err(|e| invalid("cursor", format!("cannot encode cursor: {e}")))?;
        let encoded = canonical_json(&value)?;
        check_cursor_token_len(encoded.len())?;
        Ok(encoded)
    }

    /// Decode and validate an opaque cursor token.
    pub fn decode(raw: &str) -> Result<Self, ContractError> {
        check_cursor_token_len(raw.len())?;
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| invalid("cursor", format!("malformed cursor: {e}")))?;
        let cursor: Self = decode(value, "FactoryQueueCursor/v1")?;
        cursor.validate()?;
        Ok(cursor)
    }

    /// Refuse a cursor issued for a different repository/state scope.
    pub fn check_binding(&self, repository_id: &str, state_id: &str) -> Result<(), ContractError> {
        if self.repository_id != repository_id || self.state_id != state_id {
            return Err(invalid(
                "cursor",
                "cursor does not match the requested repository/state scope",
            ));
        }
        Ok(())
    }

    /// Refuse a cursor whose pinned queue revision or runtime is no longer
    /// current.
    pub fn check_pinned(
        &self,
        revision: &str,
        runtime_instance_id: &str,
    ) -> Result<(), ContractError> {
        if self.revision != revision || self.runtime_instance_id != runtime_instance_id {
            return Err(FailureCode::RevisionConflict.failure(
                "cursor",
                format!(
                    "cursor is pinned to revision {:?}/runtime {:?}, no longer current",
                    self.revision, self.runtime_instance_id
                ),
            ));
        }
        Ok(())
    }
}

/// Validate a supplied queue cursor against the scope and pinned revision/
/// runtime. `None` means "first page". A malformed cursor, a wrong
/// repository/state scope, or a token beyond [`FACTORY_CURSOR_MAX_BYTES`] is
/// [`FailureCode::InvalidRecord`]; an unverifiable or mismatched
/// revision/runtime fails closed with [`FailureCode::RevisionConflict`] so the
/// client starts a fresh collection.
pub fn check_factory_cursor(
    raw: Option<&str>,
    repository_id: &str,
    state_id: &str,
    revision: Option<&str>,
    runtime_instance_id: Option<&str>,
) -> Result<Option<FactoryQueueCursor>, ContractError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let cursor = FactoryQueueCursor::decode(raw)?;
    cursor.check_binding(repository_id, state_id)?;
    let Some(revision) = revision else {
        return Err(FailureCode::RevisionConflict.failure(
            "cursor",
            "cursor cannot be verified: the current queue revision is unknown",
        ));
    };
    if cursor.revision != revision {
        return Err(
            FailureCode::RevisionConflict.failure("cursor", "cursor revision is no longer current")
        );
    }
    let Some(runtime_instance_id) = runtime_instance_id else {
        return Err(FailureCode::RevisionConflict.failure(
            "cursor",
            "cursor cannot be verified: the current runtime identity is unknown",
        ));
    };
    if cursor.runtime_instance_id != runtime_instance_id {
        return Err(
            FailureCode::RevisionConflict.failure("cursor", "cursor runtime is no longer current")
        );
    }
    Ok(Some(cursor))
}

/// Refuse a cursor token beyond [`FACTORY_CURSOR_MAX_BYTES`] UTF-8 bytes.
fn check_cursor_token_len(bytes: usize) -> Result<(), ContractError> {
    if bytes > FACTORY_CURSOR_MAX_BYTES {
        return Err(invalid(
            "cursor",
            format!("is {bytes} bytes, exceeding the {FACTORY_CURSOR_MAX_BYTES}-byte bound"),
        ));
    }
    Ok(())
}
