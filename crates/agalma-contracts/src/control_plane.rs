//! `ControlPlaneRead/v1` typed read contracts for the local browser dashboard
//! (`docs/web-dashboard-design.md`, "Implementation contract for decomposition").
//!
//! The hub-owned read surface (`ControlPlaneReadApi/v1`, behind the
//! `control-plane.read/v1` capability) authorizes observation and
//! report-referenced artifact reads only. These records are the pure, versioned
//! wire projection the browser adapter returns; they carry no runtime policy,
//! storage, listener, or rendering, and grant no action authority. Viewing a
//! component never implies mutation.
//!
//! Exactness and bound rules (design §"Implementation contract"):
//!
//! - identity-bearing `u64` values (generations, sequences, revisions,
//!   watermarks, offsets) travel as [`ExactU64`] decimal strings, validated on
//!   both construction and deserialization so a malformed value is never
//!   accepted or misclassified;
//! - money and usage totals travel as [`ExactAmount`] decimal strings, likewise
//!   validated on deserialization; an unknown value is `null` (`None`), never a
//!   numeric zero;
//! - a page holds at most [`READ_PAGE_MAX_ENTRIES`] entries and at most
//!   [`READ_PAGE_MAX_BYTES`] bytes of its [`BrowserWire`] JSON; the limit
//!   defaults to [`READ_PAGE_DEFAULT_LIMIT`] and accepts `1..=100`;
//! - artifact chunks are at most [`ARTIFACT_CHUNK_MAX_BYTES`] and a whole
//!   artifact is bounded by [`ARTIFACT_TOTAL_MAX_BYTES`];
//! - opaque cursors pin the collection, filter, and projection revision; an
//!   invalid cursor is [`FailureCode::InvalidRecord`] and an expired revision
//!   cursor is [`FailureCode::RevisionConflict`] so the client restarts a fresh
//!   collection.
//!
//! The response version is independent of the ledger schema version. The
//! projection reuses the canonical HP.1 records — [`HealthState`], [`View`],
//! [`ReportEnvelope`], [`BindingPin`], [`PackageArtifactRef`] — and the shared
//! [`ContractError`] taxonomy.
//! Plugin-private report payloads stay namespaced, versioned JSON; this module
//! never introduces plugin-specific hub types.
//!
//! # Browser wire projection
//!
//! The browser reads these records through the [`BrowserWire`] projection, not
//! through generic `serde_json::to_value`. Identity-bearing nested canonical
//! `u64`s that the frozen HP.1 types ([`BindingPin`], [`ReportEnvelope`]) carry
//! as JSON numbers — `BindingPin.generation` and `ReportEnvelope.sequence`,
//! `reported_at_unix_ms`, `observed_at_unix_ms` — become exact decimal strings on
//! the browser wire so a JSON parser never rounds them through an IEEE-754
//! double (values above 2^53 would otherwise lose precision). The read records'
//! own [`ExactU64`]/[`ExactAmount`] fields are already decimal strings and stay
//! untouched. Plugin-private `payload` values are copied verbatim: numbers stay
//! numbers, floats stay floats.
//!
//! This module is pure: no I/O, no vendor types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::digest::{canonical_json, validate_digest, FailureCode};
use crate::error::ContractError;
use crate::execution::{ExecutionPhase, ExecutionState};
use crate::harness::Completeness;
use crate::plugin::{HealthState, ReportEnvelope, View};
use crate::task_package::{BindingPin, PackageArtifactRef};

/// `ControlPlaneRead/v1` wire format. Independent of the ledger schema version.
pub const CONTROL_PLANE_READ_FORMAT: &str = "ControlPlaneRead/v1";
/// Basename of the read response's format family.
pub const CONTROL_PLANE_READ_FAMILY: &str = "ControlPlaneRead";
/// The scoped capability the dashboard holds for this read surface.
pub const CONTROL_PLANE_READ_CAPABILITY: &str = "control-plane.read/v1";

/// Default page size when a request omits `limit`.
pub const READ_PAGE_DEFAULT_LIMIT: u32 = 50;
/// Smallest accepted page size.
pub const READ_PAGE_MIN_LIMIT: u32 = 1;
/// Largest accepted page size.
pub const READ_PAGE_MAX_LIMIT: u32 = 100;
/// Hard cap on entries in one page (mirrors [`READ_PAGE_MAX_LIMIT`]).
pub const READ_PAGE_MAX_ENTRIES: usize = 100;
/// Hard cap on one page's [`BrowserWire`] JSON size (256 KiB).
///
/// Measured on the exact bytes the browser receives: identity-bearing nested
/// `u64`s count their decimal-string quotes, and plugin payload floats are
/// permitted. `canonical-json/v1` is unsuitable here because it rejects floats
/// and does not reflect those quotes.
pub const READ_PAGE_MAX_BYTES: u64 = 256 * 1024;
/// Maximum bytes returned by one [`ReadArtifactChunk`] (64 KiB).
pub const ARTIFACT_CHUNK_MAX_BYTES: u64 = 64 * 1024;
/// Maximum total bytes of one immutable artifact reference (16 MiB).
pub const ARTIFACT_TOTAL_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Prefix marking an opaque [`ReadCursor`] token.
pub const READ_CURSOR_PREFIX: &str = "cp1:";

// --- request-format gating --------------------------------------------------

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

/// Gate a single-version `ControlPlaneRead` family: right family wrong version
/// is [`FailureCode::UnsupportedVersion`]; an unrecognized family is
/// [`FailureCode::UnsupportedSchema`].
fn gate_read_format(format: &str) -> Result<(), ContractError> {
    match parse_format_version(format, CONTROL_PLANE_READ_FAMILY) {
        Ok(1) => Ok(()),
        Ok(other) => Err(FailureCode::UnsupportedVersion.failure(
            CONTROL_PLANE_READ_FORMAT,
            format!("ControlPlaneRead version {other} is unsupported (expected 1)"),
        )),
        Err(reason) => Err(FailureCode::UnsupportedSchema.failure(
            CONTROL_PLANE_READ_FORMAT,
            format!("unrecognized read format: {reason}"),
        )),
    }
}

// --- exact scalar representations ------------------------------------------

/// An exact unsigned 64-bit value carried as a decimal string.
///
/// Generations, sequences, revisions, watermarks, offsets, and counts use this
/// representation so large values survive JSON without floating-point loss.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExactU64(String);

impl ExactU64 {
    /// Validate and wrap a decimal string.
    pub fn parse(raw: &str) -> Result<Self, ContractError> {
        raw.parse::<u64>().map_err(|_| {
            FailureCode::InvalidRecord.failure(raw, "expected an unsigned decimal integer string")
        })?;
        Ok(Self(raw.to_string()))
    }

    /// Parse back to the native `u64`.
    pub fn as_u64(&self) -> Result<u64, ContractError> {
        self.0.parse::<u64>().map_err(|_| {
            FailureCode::InvalidRecord.failure(
                self.0.as_str(),
                "expected an unsigned decimal integer string",
            )
        })
    }

    /// Borrow the decimal text exactly as it was recorded.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<u64> for ExactU64 {
    fn from(value: u64) -> Self {
        Self(value.to_string())
    }
}

/// Validate on deserialization too, so a malformed decimal string is refused
/// where it is decoded rather than silently carried into a later comparison.
impl TryFrom<String> for ExactU64 {
    type Error = ContractError;

    fn try_from(raw: String) -> Result<Self, ContractError> {
        Self::parse(&raw)
    }
}

impl From<ExactU64> for String {
    fn from(value: ExactU64) -> Self {
        value.0
    }
}

impl std::fmt::Display for ExactU64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An exact, non-negative decimal amount (money or usage) carried as text.
///
/// The grammar is `^[0-9]+(\.[0-9]+)?$`; there is no sign, exponent, or
/// thousands separator. Money is never a JSON number and never silently rounded.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExactAmount(String);

impl ExactAmount {
    /// Validate and wrap a decimal amount string.
    pub fn parse(raw: &str) -> Result<Self, ContractError> {
        if valid_amount(raw) {
            Ok(Self(raw.to_string()))
        } else {
            Err(FailureCode::InvalidRecord
                .failure(raw, "expected a non-negative decimal amount string"))
        }
    }

    /// Borrow the decimal text exactly as it was recorded.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExactAmount {
    type Error = ContractError;

    fn try_from(raw: String) -> Result<Self, ContractError> {
        Self::parse(&raw)
    }
}

impl From<ExactAmount> for String {
    fn from(value: ExactAmount) -> Self {
        value.0
    }
}

impl std::fmt::Display for ExactAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `raw` is `^[0-9]+(\.[0-9]+)?$`.
fn valid_amount(raw: &str) -> bool {
    let mut parts = raw.split('.');
    let integer = parts.next().unwrap_or("");
    if integer.is_empty() || !integer.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(fraction) => {
            !fraction.is_empty()
                && fraction.bytes().all(|b| b.is_ascii_digit())
                && parts.next().is_none()
        }
    }
}

// --- response metadata and source availability -----------------------------

/// The reported availability of one read source.
///
/// A missing source, an unreadable source, and a successful-but-empty source are
/// distinct states so the browser never turns absent data into healthy or empty
/// state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceState {
    /// Responded successfully with entries.
    Available,
    /// Responded successfully but holds no entries.
    Empty,
    /// The source does not exist (for example, no ledger is initialized).
    Missing,
    /// The source exists but could not be read.
    Unreadable,
    /// The source's state is not known.
    Unknown,
}

/// One observation of a named source's availability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAvailability {
    /// Stable source name (`ledger`, `registry`, `reports`, `budget`).
    pub source: String,
    pub state: SourceState,
    /// Human-readable detail; the empty string when self-explanatory.
    pub detail: String,
    pub observed_at_unix_ms: u64,
}

/// Metadata every successful read response carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadMetadata {
    /// Wire format; gated by [`ReadMetadata::version_gate`].
    pub format: String,
    pub collected_at_unix_ms: u64,
    /// Registry revision the collection was taken at, when applicable.
    pub registry_revision: Option<ExactU64>,
    /// Report watermark the projection is consistent through, when applicable.
    pub report_watermark: Option<ExactU64>,
    /// Per-source availability; no implicit atomicity across sources.
    pub sources: Vec<SourceAvailability>,
}

impl ReadMetadata {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        gate_read_format(&self.format)
    }
}

// --- overview ---------------------------------------------------------------

/// The observed factory stop latch (distinct from any component's health).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryStop {
    /// Whether the factory effect admission is currently stopped.
    pub stopped: bool,
    /// Why the factory is stopped, when recorded.
    pub reason: Option<String>,
    pub observed_at_unix_ms: u64,
}

/// A bounded summary of one execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSummary {
    pub execution_id: String,
    pub task_id: String,
    pub generation: ExactU64,
    pub phase: ExecutionPhase,
    pub state: ExecutionState,
    pub attempt: ExactU64,
    pub revision: ExactU64,
    pub updated_at_unix_ms: u64,
}

/// Known usage, cost, and allowance. `null` means unknown, never zero.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageCost {
    /// A declared completeness for the usage observation.
    pub completeness: Completeness,
    /// Known total usage in `usage_unit`, or `null` when unavailable.
    pub usage: Option<ExactAmount>,
    /// Unit label for `usage` (for example `tokens`).
    pub usage_unit: String,
    /// Known total cost, or `null` when unavailable.
    pub cost: Option<ExactAmount>,
    /// Known allowance, or `null` when unavailable.
    pub allowance: Option<ExactAmount>,
    /// Unit label for `cost`/`allowance` (for example `usd` or `nusd`).
    pub cost_unit: String,
    pub observed_at_unix_ms: u64,
}

/// One entry in the curated factory timeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineEntry {
    pub at_unix_ms: u64,
    pub kind: String,
    pub title: String,
    pub detail: String,
}

/// `ReadOverview` result: factory stop state, executions, economics, and a
/// curated timeline assembled from bound domain APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadOverview {
    pub metadata: ReadMetadata,
    pub factory: FactoryStop,
    pub executions: Vec<ExecutionSummary>,
    pub economics: UsageCost,
    pub timeline: Vec<TimelineEntry>,
}

// --- components -------------------------------------------------------------

/// Report freshness of a component projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Freshness {
    /// Current relative to the report watermark.
    Fresh,
    /// Behind the report watermark.
    Stale,
    /// Freshness cannot be established.
    Unknown,
}

/// One registry-derived component entry for `ListComponents`.
///
/// `available` is false for an installed-but-unavailable component; such an
/// entry is still listed with its identity and declared views.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentSummary {
    pub plugin_id: String,
    pub name: String,
    /// The active binding identity and logical generation.
    pub binding: BindingPin,
    pub generation: ExactU64,
    pub health: HealthState,
    pub freshness: Freshness,
    pub completeness: Completeness,
    /// Whether the component is currently serving.
    pub available: bool,
    /// Declared view descriptors.
    pub views: Vec<View>,
}

/// A bounded page of one projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadPage<T> {
    pub metadata: ReadMetadata,
    pub entries: Vec<T>,
    /// Opaque continuation cursor, or `null` at the final page.
    pub next_cursor: Option<String>,
}

impl<T: BrowserWire> ReadPage<T> {
    /// Refuse a page beyond [`READ_PAGE_MAX_ENTRIES`] or
    /// [`READ_PAGE_MAX_BYTES`], measured on the [`BrowserWire`] JSON the browser
    /// receives.
    pub fn check_bounds(&self) -> Result<(), ContractError> {
        check_wire_page_bound("page", self.entries.len(), self)
    }
}

/// `ListComponents` result: registry identities, generations, health/freshness/
/// completeness, and declared views, including installed-unavailable components.
pub type ListComponents = ReadPage<ComponentSummary>;

/// `ReadComponent` result: a component's declared descriptors and its bounded
/// namespace reports from the authoritative HP.5 projection, retaining every
/// envelope identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadComponent {
    pub metadata: ReadMetadata,
    /// The component's active binding identity and generation.
    pub binding: BindingPin,
    /// Declared view descriptors.
    pub descriptors: Vec<View>,
    /// Hub-observed report envelopes, each retaining its publication identity.
    pub entries: Vec<ReportEnvelope>,
    /// Opaque continuation cursor, or `null` at the final page.
    pub next_cursor: Option<String>,
}

impl ReadComponent {
    /// Refuse a page beyond [`READ_PAGE_MAX_ENTRIES`] or
    /// [`READ_PAGE_MAX_BYTES`], measured on the [`BrowserWire`] JSON the browser
    /// receives (identity quotes counted, plugin payload floats permitted).
    pub fn check_bounds(&self) -> Result<(), ContractError> {
        check_wire_page_bound("page", self.entries.len(), self)
    }
}

// --- artifacts --------------------------------------------------------------

/// `ReadArtifactChunk` result: at most [`ARTIFACT_CHUNK_MAX_BYTES`] of an
/// immutable, authorized [`PackageArtifactRef`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArtifactChunk {
    pub metadata: ReadMetadata,
    /// The immutable reference this chunk was resolved from.
    pub reference: PackageArtifactRef,
    pub offset: ExactU64,
    /// Lowercase hex of the returned bytes (two hex digits per byte).
    pub data_hex: String,
    /// Offset of the next chunk, or `null` at end of artifact.
    pub next_offset: Option<ExactU64>,
}

impl ReadArtifactChunk {
    /// Decoded byte length of [`Self::data_hex`], refusing malformed hex.
    pub fn data_len(&self) -> Result<u64, ContractError> {
        if self.data_hex.len() % 2 != 0 {
            return Err(FailureCode::InvalidRecord.failure(
                &self.reference.path,
                "artifact chunk data_hex has an odd number of digits",
            ));
        }
        let mut len: u64 = 0;
        for byte in self.data_hex.bytes() {
            if !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte) {
                return Err(FailureCode::InvalidRecord.failure(
                    &self.reference.path,
                    "artifact chunk data_hex is not lowercase hex",
                ));
            }
            len += 1;
        }
        Ok(len / 2)
    }

    /// Refuse a chunk larger than [`ARTIFACT_CHUNK_MAX_BYTES`], an artifact
    /// larger than [`ARTIFACT_TOTAL_MAX_BYTES`], an out-of-range offset, or a
    /// `next_offset` that does not exactly continue the artifact.
    ///
    /// `next_offset` is the delivered byte end (`offset + data_len`). It must be
    /// `null` exactly when the chunk reaches the artifact end, and otherwise
    /// must be present and equal to that end. A truncated chunk with a null
    /// `next_offset`, a redundant `next_offset` at the end, or a skipped
    /// `next_offset` is refused, so a client can never silently lose bytes. A
    /// zero-length chunk before the artifact end is also refused: it advances
    /// nothing and would let a client repeat the same offset forever. An empty
    /// chunk is only valid as the final chunk (`offset == total`).
    pub fn check_bounds(&self) -> Result<(), ContractError> {
        let path = &self.reference.path;
        let data_len = self.data_len()?;
        if data_len > ARTIFACT_CHUNK_MAX_BYTES {
            return Err(FailureCode::InvalidRecord.failure(
                path,
                format!("artifact chunk {data_len} bytes exceeds limit {ARTIFACT_CHUNK_MAX_BYTES}"),
            ));
        }
        let total = self.reference.bytes;
        if total > ARTIFACT_TOTAL_MAX_BYTES {
            return Err(FailureCode::InvalidRecord.failure(
                path,
                format!("artifact {total} bytes exceeds limit {ARTIFACT_TOTAL_MAX_BYTES}"),
            ));
        }
        let offset = self.offset.as_u64()?;
        let end = offset.checked_add(data_len).ok_or_else(|| {
            FailureCode::InvalidRecord.failure(path, "artifact chunk offset overflow")
        })?;
        if end > total {
            return Err(FailureCode::InvalidRecord.failure(
                path,
                format!("artifact chunk covers {end} bytes beyond the artifact's {total}"),
            ));
        }
        if data_len == 0 && end < total {
            return Err(FailureCode::InvalidRecord.failure(
                path,
                format!(
                    "artifact chunk at offset {offset} is empty before the artifact's {total} bytes and would make no progress"
                ),
            ));
        }
        match &self.next_offset {
            None if end < total => Err(FailureCode::InvalidRecord.failure(
                path,
                format!(
                    "artifact chunk ends at {end} before the artifact's {total} bytes but next_offset is null"
                ),
            )),
            None => Ok(()),
            Some(next) => {
                let next = next.as_u64()?;
                if next != end {
                    return Err(FailureCode::InvalidRecord.failure(
                        path,
                        format!(
                            "artifact chunk next_offset {next} does not equal offset+data_len {end}"
                        ),
                    ));
                }
                if next >= total {
                    return Err(FailureCode::InvalidRecord.failure(
                        path,
                        format!(
                            "artifact chunk next_offset {next} must be null at the artifact end ({total})"
                        ),
                    ));
                }
                Ok(())
            }
        }
    }
}

// --- bounded requests -------------------------------------------------------

/// A bounded page request. `limit: null` uses [`READ_PAGE_DEFAULT_LIMIT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReadPageRequest {
    /// Requested page size; `null` uses the default.
    pub limit: Option<u32>,
    /// Opaque continuation cursor from a prior page.
    pub cursor: Option<String>,
}

impl ReadPageRequest {
    /// The effective page size: the default, or a requested `1..=100`.
    pub fn effective_limit(&self) -> Result<u32, ContractError> {
        match self.limit {
            None => Ok(READ_PAGE_DEFAULT_LIMIT),
            Some(limit) if (READ_PAGE_MIN_LIMIT..=READ_PAGE_MAX_LIMIT).contains(&limit) => {
                Ok(limit)
            }
            Some(limit) => Err(FailureCode::InvalidRecord.failure(
                "limit",
                format!(
                    "page limit {limit} is outside {READ_PAGE_MIN_LIMIT}..={READ_PAGE_MAX_LIMIT}"
                ),
            )),
        }
    }
}

/// A bounded artifact-chunk request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReadArtifactChunkRequest {
    pub plugin_id: String,
    /// The `sha256:` digest of the referenced immutable artifact.
    pub digest: String,
    /// Byte offset; `null` starts at zero.
    pub offset: Option<ExactU64>,
    /// Requested chunk size; defaults to and never exceeds
    /// [`ARTIFACT_CHUNK_MAX_BYTES`].
    pub max_bytes: Option<u64>,
}

impl ReadArtifactChunkRequest {
    /// Validate the digest shape and the requested chunk bound.
    pub fn check(&self) -> Result<(), ContractError> {
        validate_digest(&self.digest, &self.plugin_id)?;
        self.effective_max_bytes().map(|_| ())
    }

    /// The effective byte offset (`0` when omitted).
    pub fn effective_offset(&self) -> Result<u64, ContractError> {
        match &self.offset {
            None => Ok(0),
            Some(offset) => offset.as_u64(),
        }
    }

    /// The effective chunk size, defaulting to and never exceeding the cap.
    pub fn effective_max_bytes(&self) -> Result<u64, ContractError> {
        match self.max_bytes {
            None => Ok(ARTIFACT_CHUNK_MAX_BYTES),
            Some(0) => Err(FailureCode::InvalidRecord
                .failure("max_bytes", "artifact chunk max_bytes must be >= 1")),
            Some(max) if max > ARTIFACT_CHUNK_MAX_BYTES => Err(FailureCode::InvalidRecord.failure(
                "max_bytes",
                format!("artifact chunk max_bytes {max} exceeds limit {ARTIFACT_CHUNK_MAX_BYTES}"),
            )),
            Some(max) => Ok(max),
        }
    }
}

// --- opaque cursors ---------------------------------------------------------

/// An opaque continuation cursor.
///
/// Clients carry the token verbatim. It pins the collection, the filter
/// fingerprint, and the projection revision it was issued at, plus the last
/// delivered entry key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadCursor {
    pub collection: String,
    pub filter_digest: String,
    pub revision: ExactU64,
    pub after: String,
}

impl ReadCursor {
    /// Encode this cursor as an opaque `cp1:`-prefixed token.
    pub fn encode(&self) -> Result<String, ContractError> {
        let value = serde_json::to_value(self).map_err(|e| {
            FailureCode::InvalidRecord
                .failure(&self.collection, format!("cannot encode cursor: {e}"))
        })?;
        Ok(format!("{READ_CURSOR_PREFIX}{}", canonical_json(&value)?))
    }

    /// Decode an opaque cursor token, refusing a malformed one.
    pub fn decode(raw: &str) -> Result<Self, ContractError> {
        let body = raw.strip_prefix(READ_CURSOR_PREFIX).ok_or_else(|| {
            FailureCode::InvalidRecord.failure(raw, "cursor is not a `cp1:` token")
        })?;
        let value: Value = serde_json::from_str(body).map_err(|e| {
            FailureCode::InvalidRecord.failure(raw, format!("malformed cursor: {e}"))
        })?;
        serde_json::from_value(value)
            .map_err(|e| FailureCode::InvalidRecord.failure(raw, format!("malformed cursor: {e}")))
    }

    /// Refuse a cursor issued for a different collection or filter.
    pub fn check_binding(
        &self,
        collection: &str,
        filter_digest: &str,
    ) -> Result<(), ContractError> {
        if self.collection != collection || self.filter_digest != filter_digest {
            return Err(FailureCode::InvalidRecord.failure(
                &self.collection,
                "cursor does not match the requested collection/filter",
            ));
        }
        Ok(())
    }

    /// Refuse a cursor whose pinned revision is no longer current.
    pub fn check_revision(&self, current: &ExactU64) -> Result<(), ContractError> {
        if &self.revision != current {
            return Err(FailureCode::RevisionConflict.failure(
                &self.collection,
                format!(
                    "cursor revision {} is no longer current ({})",
                    self.revision.as_str(),
                    current.as_str()
                ),
            ));
        }
        Ok(())
    }
}

/// Validate a supplied page cursor against the current collection, filter, and
/// revision. `None` means "first page". A malformed or mismatched cursor fails
/// with [`FailureCode::InvalidRecord`]; an expired revision cursor fails with
/// [`FailureCode::RevisionConflict`].
pub fn check_cursor(
    raw: Option<&str>,
    collection: &str,
    filter_digest: &str,
    current_revision: &ExactU64,
) -> Result<Option<ReadCursor>, ContractError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let cursor = ReadCursor::decode(raw)?;
    cursor.check_binding(collection, filter_digest)?;
    cursor.check_revision(current_revision)?;
    Ok(Some(cursor))
}

// --- field selectors --------------------------------------------------------

/// Resolution of a `View.fields` selector against a report payload.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldResolution<'a> {
    /// The selector resolved to a value.
    Value(&'a Value),
    /// The selector matched no field: render as missing, never healthy.
    Missing,
}

/// Resolve a `View.fields` selector against `payload`.
///
/// A bare selector is a literal top-level key. A selector beginning with `/` is
/// an object/array path using `~1` for a literal `/` and `~0` for a literal `~`.
/// Selectors are never expressions or executable templates. A malformed pointer,
/// a missing key, a non-numeric array index, or an out-of-range index resolves
/// to [`FieldResolution::Missing`].
pub fn resolve_field_selector<'a>(payload: &'a Value, selector: &str) -> FieldResolution<'a> {
    if !selector.starts_with('/') {
        return match payload.get(selector) {
            Some(value) => FieldResolution::Value(value),
            None => FieldResolution::Missing,
        };
    }
    let mut current = payload;
    for raw in selector[1..].split('/') {
        let Some(token) = unescape_pointer_token(raw) else {
            return FieldResolution::Missing;
        };
        current = match current {
            Value::Object(map) => match map.get(&token) {
                Some(value) => value,
                None => return FieldResolution::Missing,
            },
            Value::Array(items) => {
                let Ok(index) = token.parse::<usize>() else {
                    return FieldResolution::Missing;
                };
                match items.get(index) {
                    Some(value) => value,
                    None => return FieldResolution::Missing,
                }
            }
            _ => return FieldResolution::Missing,
        };
    }
    FieldResolution::Value(current)
}

/// Unescape one JSON-pointer token (`~1` → `/`, `~0` → `~`); `None` when a `~`
/// is not followed by `0` or `1`.
fn unescape_pointer_token(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => return None,
            }
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

// --- shared helpers ---------------------------------------------------------

/// Refuse a page beyond [`READ_PAGE_MAX_ENTRIES`] or [`READ_PAGE_MAX_BYTES`].
///
/// The byte bound is measured on the [`BrowserWire`] JSON the browser actually
/// receives (`record.to_wire_json()`), not on `canonical-json/v1`. The wire form
/// quotes identity-bearing nested `u64`s (adding bytes) and preserves
/// plugin-private payload floats (which `canonical-json/v1` refuses), so a
/// canonical measurement would both under- and over-count real pages.
fn check_wire_page_bound<T: BrowserWire>(
    subject: &str,
    entries: usize,
    record: &T,
) -> Result<(), ContractError> {
    if entries > READ_PAGE_MAX_ENTRIES {
        return Err(FailureCode::InvalidRecord.failure(
            subject,
            format!("page holds {entries} entries, exceeding limit {READ_PAGE_MAX_ENTRIES}"),
        ));
    }
    let bytes = record.to_wire_json()?.len() as u64;
    if bytes > READ_PAGE_MAX_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            subject,
            format!("page serializes to {bytes} bytes, exceeding limit {READ_PAGE_MAX_BYTES}"),
        ));
    }
    Ok(())
}

// --- browser wire projection ------------------------------------------------

/// Browser-facing serialization projection for the `ControlPlaneRead/v1`
/// records.
///
/// Generic [`serde_json::to_value`] serializes the frozen HP.1 [`BindingPin`] and
/// [`ReportEnvelope`] identity `u64` fields — `generation`, `sequence`,
/// `reported_at_unix_ms`, `observed_at_unix_ms` — as JSON numbers. A browser
/// parses JSON numbers as IEEE-754 doubles and loses precision above 2^53, which
/// violates the exact decimal-string identity contract. This projection rewrites
/// exactly those nested canonical identity fields as decimal strings and parses
/// them back, while leaving every plugin-private `payload` value untouched
/// (numbers stay numbers, floats stay floats).
///
/// Implemented for the four read results: [`ReadOverview`],
/// [`ReadPage<ComponentSummary>`](ReadPage) (the `ListComponents` alias),
/// [`ReadComponent`], and [`ReadArtifactChunk`]. The read records' own
/// [`ExactU64`]/[`ExactAmount`] fields are already decimal strings and are not
/// rewritten by the projection.
pub trait BrowserWire: Serialize + serde::de::DeserializeOwned + Sized {
    /// Project to browser wire JSON: nested canonical identity `u64` fields
    /// become exact decimal strings; plugin-private `payload` values are copied
    /// verbatim (JSON numbers stay numbers).
    fn to_wire_value(&self) -> Result<Value, ContractError>;

    /// Deterministic JSON text of [`Self::to_wire_value`].
    ///
    /// Uses [`serde_json::to_string`], not `canonical_json`, because a plugin
    /// payload may contain floats that `canonical-json/v1` rejects.
    fn to_wire_json(&self) -> Result<String, ContractError> {
        let value = self.to_wire_value()?;
        serde_json::to_string(&value).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                "browser-wire",
                format!("cannot serialize browser wire value: {e}"),
            )
        })
    }

    /// Parse browser wire JSON back into the canonical record, accepting the
    /// decimal-string identities; malformed, overflowing, negative, or float
    /// identities are refused with [`FailureCode::InvalidRecord`].
    fn from_wire_value(value: &Value) -> Result<Self, ContractError>;

    /// Parse a wire JSON string, then [`Self::from_wire_value`].
    fn from_wire_str(text: &str) -> Result<Self, ContractError> {
        let value: Value = serde_json::from_str(text).map_err(|e| {
            FailureCode::InvalidRecord
                .failure("browser-wire", format!("malformed browser wire JSON: {e}"))
        })?;
        Self::from_wire_value(&value)
    }
}

/// Serialize a record to a JSON value, mapping any failure to `InvalidRecord`.
fn serialize_to_wire_value<T: Serialize>(value: &T) -> Result<Value, ContractError> {
    serde_json::to_value(value).map_err(|e| {
        FailureCode::InvalidRecord.failure("browser-wire", format!("cannot project record: {e}"))
    })
}

/// Deserialize a canonical record from a JSON value, mapping every error to
/// `InvalidRecord`.
fn deserialize_from_wire_value<T: serde::de::DeserializeOwned>(
    value: &Value,
) -> Result<T, ContractError> {
    serde_json::from_value(value.clone()).map_err(|e| {
        FailureCode::InvalidRecord.failure("browser-wire", format!("cannot decode wire value: {e}"))
    })
}

/// Rewrite one identity field from a decimal string to a `u64` JSON number,
/// refusing a non-decimal, negative, overflowing, empty, or float value.
fn rewrite_identity_to_u64(field: &mut Value) -> Result<(), ContractError> {
    let text = match field {
        Value::String(text) => text.clone(),
        other => {
            return Err(FailureCode::InvalidRecord.failure(
                "browser-wire-identity",
                format!("expected a decimal identity string, found {other}"),
            ))
        }
    };
    let parsed = text.parse::<u64>().map_err(|_| {
        FailureCode::InvalidRecord.failure(&text, "expected an unsigned decimal identity string")
    })?;
    *field = Value::Number(serde_json::Number::from(parsed));
    Ok(())
}

/// Project a canonical [`BindingPin`] with `generation` as a decimal string.
fn binding_pin_to_wire(pin: &BindingPin) -> Result<Value, ContractError> {
    let mut value = serialize_to_wire_value(pin)?;
    if let Value::Object(map) = &mut value {
        map.insert(
            "generation".to_string(),
            Value::String(pin.generation.to_string()),
        );
    }
    Ok(value)
}

/// Invert [`binding_pin_to_wire`] on an in-place wire value.
fn binding_pin_from_wire(value: &mut Value) -> Result<(), ContractError> {
    let map = value.as_object_mut().ok_or_else(|| {
        FailureCode::InvalidRecord.failure("binding", "expected a binding pin object")
    })?;
    match map.get_mut("generation") {
        Some(field) => rewrite_identity_to_u64(field),
        None => Ok(()),
    }
}

/// Project a canonical [`ReportEnvelope`], rewriting its identity `u64` fields
/// as decimal strings and copying `payload` verbatim.
fn report_envelope_to_wire(envelope: &ReportEnvelope) -> Result<Value, ContractError> {
    let mut value = serialize_to_wire_value(envelope)?;
    if let Value::Object(map) = &mut value {
        map.insert(
            "binding".to_string(),
            binding_pin_to_wire(&envelope.binding)?,
        );
        map.insert(
            "sequence".to_string(),
            Value::String(envelope.sequence.to_string()),
        );
        map.insert(
            "reported_at_unix_ms".to_string(),
            Value::String(envelope.reported_at_unix_ms.to_string()),
        );
        map.insert(
            "observed_at_unix_ms".to_string(),
            Value::String(envelope.observed_at_unix_ms.to_string()),
        );
    }
    Ok(value)
}

/// Invert [`report_envelope_to_wire`] on an in-place wire value.
fn report_envelope_from_wire(value: &mut Value) -> Result<(), ContractError> {
    let map = value.as_object_mut().ok_or_else(|| {
        FailureCode::InvalidRecord.failure("report-envelope", "expected a report envelope object")
    })?;
    if let Some(binding) = map.get_mut("binding") {
        binding_pin_from_wire(binding)?;
    }
    for key in ["sequence", "reported_at_unix_ms", "observed_at_unix_ms"] {
        if let Some(field) = map.get_mut(key) {
            rewrite_identity_to_u64(field)?;
        }
    }
    Ok(())
}

/// Project a canonical [`ComponentSummary`] with its `binding` generation as a
/// decimal string.
fn component_summary_to_wire(summary: &ComponentSummary) -> Result<Value, ContractError> {
    let mut value = serialize_to_wire_value(summary)?;
    if let Value::Object(map) = &mut value {
        map.insert(
            "binding".to_string(),
            binding_pin_to_wire(&summary.binding)?,
        );
    }
    Ok(value)
}

/// Invert [`component_summary_to_wire`] on an in-place wire value.
fn component_summary_from_wire(value: &mut Value) -> Result<(), ContractError> {
    let map = value.as_object_mut().ok_or_else(|| {
        FailureCode::InvalidRecord
            .failure("component-summary", "expected a component summary object")
    })?;
    if let Some(binding) = map.get_mut("binding") {
        binding_pin_from_wire(binding)?;
    }
    Ok(())
}

impl BrowserWire for ReadOverview {
    fn to_wire_value(&self) -> Result<Value, ContractError> {
        // No nested canonical identity needs rewriting; the faithful generic
        // projection proves the codec is exact for the overview surface.
        serialize_to_wire_value(self)
    }

    fn from_wire_value(value: &Value) -> Result<Self, ContractError> {
        deserialize_from_wire_value(value)
    }
}

impl BrowserWire for ReadArtifactChunk {
    fn to_wire_value(&self) -> Result<Value, ContractError> {
        // `PackageArtifactRef.bytes` is not identity-bearing and stays a number;
        // the chunk's own `ExactU64` offsets are already decimal strings.
        serialize_to_wire_value(self)
    }

    fn from_wire_value(value: &Value) -> Result<Self, ContractError> {
        deserialize_from_wire_value(value)
    }
}

impl BrowserWire for ReadComponent {
    fn to_wire_value(&self) -> Result<Value, ContractError> {
        let mut value = serialize_to_wire_value(self)?;
        let map = value.as_object_mut().ok_or_else(|| {
            FailureCode::InvalidRecord.failure("read-component", "expected a read component object")
        })?;
        map.insert("binding".to_string(), binding_pin_to_wire(&self.binding)?);
        let entries = self
            .entries
            .iter()
            .map(report_envelope_to_wire)
            .collect::<Result<Vec<_>, _>>()?;
        map.insert("entries".to_string(), Value::Array(entries));
        Ok(value)
    }

    fn from_wire_value(value: &Value) -> Result<Self, ContractError> {
        let mut value = value.clone();
        let map = value.as_object_mut().ok_or_else(|| {
            FailureCode::InvalidRecord.failure("read-component", "expected a read component object")
        })?;
        if let Some(binding) = map.get_mut("binding") {
            binding_pin_from_wire(binding)?;
        }
        if let Some(Value::Array(entries)) = map.get_mut("entries") {
            for entry in entries {
                report_envelope_from_wire(entry)?;
            }
        }
        deserialize_from_wire_value(&value)
    }
}

impl BrowserWire for ComponentSummary {
    fn to_wire_value(&self) -> Result<Value, ContractError> {
        component_summary_to_wire(self)
    }

    fn from_wire_value(value: &Value) -> Result<Self, ContractError> {
        let mut value = value.clone();
        component_summary_from_wire(&mut value)?;
        deserialize_from_wire_value(&value)
    }
}

impl<T: BrowserWire> BrowserWire for ReadPage<T> {
    fn to_wire_value(&self) -> Result<Value, ContractError> {
        let mut value = serialize_to_wire_value(self)?;
        let map = value.as_object_mut().ok_or_else(|| {
            FailureCode::InvalidRecord.failure("read-page", "expected a page object")
        })?;
        let entries = self
            .entries
            .iter()
            .map(BrowserWire::to_wire_value)
            .collect::<Result<Vec<_>, _>>()?;
        map.insert("entries".to_string(), Value::Array(entries));
        Ok(value)
    }

    fn from_wire_value(value: &Value) -> Result<Self, ContractError> {
        let map = value.as_object().ok_or_else(|| {
            FailureCode::InvalidRecord.failure("read-page", "expected a page object")
        })?;
        // Without an `entries` array there is nothing to project; the generic
        // decode reports the shape error.
        let Some(entries) = map.get("entries").and_then(Value::as_array) else {
            return deserialize_from_wire_value(value);
        };
        let parsed = entries
            .iter()
            .map(T::from_wire_value)
            .collect::<Result<Vec<_>, _>>()?;

        // Rebuild a canonical page value whose entries are serialized
        // canonically, so the deserialize step yields exactly the canonical
        // record; then restore the entries parsed from the wire.
        let mut canonical = value.clone();
        let canonical_map = canonical.as_object_mut().expect("object checked above");
        let canonical_entries = parsed
            .iter()
            .map(serialize_to_wire_value)
            .collect::<Result<Vec<_>, _>>()?;
        canonical_map.insert("entries".to_string(), Value::Array(canonical_entries));

        let mut page: Self = deserialize_from_wire_value(&canonical)?;
        page.entries = parsed;
        Ok(page)
    }
}
