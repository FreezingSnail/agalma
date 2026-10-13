//! Canonical encoding, SHA-256 identities, and failure-detail codes (revision 1).
//!
//! `canonical-json/v1` recursively sorts object keys by UTF-8 byte order and
//! writes compact JSON with raw UTF-8 strings (no `ensure_ascii` escaping, no
//! whitespace). Array order and string bytes are significant. Parsed integer
//! representations are preserved exactly; floating-point and non-finite numbers
//! are rejected, because integrity-bearing v1 records use integer JSON numbers
//! (decimal metrics travel as strings plus units).
//!
//! New package, configuration, evidence, and input identities use
//! `sha256:<64 lower hex digits>`, computed with the locked `ring = 0.17.14`
//! SHA-256 implementation. FNV remains readable for legacy operation conflict
//! detection; it is never accepted as an artifact integrity digest.
//!
//! Revision-1 failure semantics: new protocols return structured
//! `FailureDetail/v1` JSON inside the existing string payload of a
//! [`ContractError`] variant. [`FailureCode`] names the stable codes and maps
//! each onto its variant.
//!
//! This module is pure: no I/O, no vendor types.

use serde_json::Value;

use crate::error::ContractError;

/// Canonical encoding identifier carried by revision-1 contracts.
pub const CANONICAL_JSON_FORMAT: &str = "canonical-json/v1";
/// Digest algorithm prefix for revision-1 identities.
pub const SHA256_PREFIX: &str = "sha256:";
/// Length of a hex-encoded SHA-256 digest.
pub const SHA256_HEX_LEN: usize = 64;
/// The `FailureDetail/v1` wire format identifier.
pub const FAILURE_DETAIL_FORMAT: &str = "FailureDetail/v1";

/// Encode `value` as `canonical-json/v1`.
///
/// Object keys are sorted by UTF-8 byte order; output uses compact separators
/// (`{`, `}`, `[`, `]`, `,`, `:`) and raw UTF-8 string bytes. A non-integer JSON
/// number (any floating-point representation, including non-finite) is refused
/// with [`FailureCode::InvalidRecord`].
pub fn canonical_json(value: &Value) -> Result<String, ContractError> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), ContractError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                out.push_str(&int.to_string());
            } else if let Some(uint) = number.as_u64() {
                out.push_str(&uint.to_string());
            } else {
                return Err(FailureCode::InvalidRecord.failure(
                    "canonical-json/v1",
                    "floating-point and non-finite numbers are not valid integrity inputs",
                ));
            }
        }
        Value::String(text) => {
            let encoded = serde_json::to_string(text).map_err(|e| {
                FailureCode::InvalidRecord.failure(
                    "canonical-json/v1",
                    format!("cannot encode string: {e}"),
                )
            })?;
            out.push_str(&encoded);
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // UTF-8 byte order, not locale or insertion order.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                let encoded = serde_json::to_string(key.as_str()).map_err(|e| {
                    FailureCode::InvalidRecord.failure(
                        "canonical-json/v1",
                        format!("cannot encode object key: {e}"),
                    )
                })?;
                out.push_str(&encoded);
                out.push(':');
                write_canonical(&map[*key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Lowercase hex encoding of the SHA-256 of `bytes` (no algorithm prefix).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut hex = String::with_capacity(SHA256_HEX_LEN);
    for byte in digest.as_ref() {
        hex.push(hex_digit(byte >> 4));
        hex.push(hex_digit(byte & 0x0f));
    }
    hex
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + (nibble - 10)) as char,
    }
}

/// `sha256:<hex>` identity of the exact bytes `bytes`.
pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("{SHA256_PREFIX}{}", sha256_hex(bytes))
}

/// `sha256:<hex>` identity of the canonical-json/v1 encoding of `value`.
///
/// Serialization failure is an error, never the hash of an empty buffer.
pub fn canonical_digest(value: &Value) -> Result<String, ContractError> {
    let text = canonical_json(value)?;
    Ok(sha256_digest(text.as_bytes()))
}

/// Whether `digest` is exactly `sha256:` + 64 lowercase hex digits.
pub fn is_sha256(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix(SHA256_PREFIX) else {
        return false;
    };
    hex.len() == SHA256_HEX_LEN
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Validate a `sha256:` identity, failing with [`FailureCode::IntegrityMismatch`].
pub fn validate_digest(digest: &str, subject: &str) -> Result<(), ContractError> {
    if is_sha256(digest) {
        Ok(())
    } else {
        Err(FailureCode::IntegrityMismatch.failure(
            subject,
            format!("expected `sha256:<64 lower hex digits>`, got {digest:?}"),
        ))
    }
}

/// Stable revision-1 failure-detail codes and their [`ContractError`] variants.
///
/// The mapping is fixed by `docs/self-mutation-contracts.md` (revision 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureCode {
    /// A malformed or unparsable record.
    InvalidRecord,
    /// A digest or byte-count did not match the referenced content.
    IntegrityMismatch,
    /// A request outside the admitted scope.
    ScopeDenied,
    /// A request without the required authority.
    AuthorityDenied,
    /// A bounded operation exceeded its deadline.
    DeadlineExceeded,
    /// No budget is available for the requested work.
    BudgetUnavailable,
    /// The stop policy halted the operation.
    Stopped,
    /// Required state is unavailable.
    StateUnavailable,
    /// A legacy in-flight input cannot be reconstructed; park explicitly.
    LegacyInputsMissing,
    /// A recognized protocol at an unsupported version.
    UnsupportedVersion,
    /// A record whose control schema is not recognized.
    UnsupportedSchema,
    /// The bound component is missing a required capability.
    MissingCapability,
    /// A provider profile without bounded completions.
    UnboundedProvider,
    /// A compare-and-update revision check failed.
    RevisionConflict,
    /// An identity was reused with different content.
    IdentityConflict,
    /// A pinned generation is no longer current.
    StaleGeneration,
    /// The baseline changed under a candidate.
    BaselineChanged,
    /// An announced effect may or may not have happened.
    EffectUnconfirmed,
    /// Process termination could not be confirmed.
    TerminationUnconfirmed,
    /// Provider usage could not be confirmed.
    UsageUnconfirmed,
}

impl FailureCode {
    /// The stable wire code.
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureCode::InvalidRecord => "InvalidRecord",
            FailureCode::IntegrityMismatch => "IntegrityMismatch",
            FailureCode::ScopeDenied => "ScopeDenied",
            FailureCode::AuthorityDenied => "AuthorityDenied",
            FailureCode::DeadlineExceeded => "DeadlineExceeded",
            FailureCode::BudgetUnavailable => "BudgetUnavailable",
            FailureCode::Stopped => "Stopped",
            FailureCode::StateUnavailable => "StateUnavailable",
            FailureCode::LegacyInputsMissing => "LegacyInputsMissing",
            FailureCode::UnsupportedVersion => "UnsupportedVersion",
            FailureCode::UnsupportedSchema => "UnsupportedSchema",
            FailureCode::MissingCapability => "MissingCapability",
            FailureCode::UnboundedProvider => "UnboundedProvider",
            FailureCode::RevisionConflict => "RevisionConflict",
            FailureCode::IdentityConflict => "IdentityConflict",
            FailureCode::StaleGeneration => "StaleGeneration",
            FailureCode::BaselineChanged => "BaselineChanged",
            FailureCode::EffectUnconfirmed => "EffectUnconfirmed",
            FailureCode::TerminationUnconfirmed => "TerminationUnconfirmed",
            FailureCode::UsageUnconfirmed => "UsageUnconfirmed",
        }
    }

    /// The [`ContractError`] variant this code belongs to.
    pub const fn variant(self) -> FailureVariant {
        match self {
            FailureCode::InvalidRecord
            | FailureCode::IntegrityMismatch
            | FailureCode::ScopeDenied
            | FailureCode::AuthorityDenied
            | FailureCode::DeadlineExceeded
            | FailureCode::BudgetUnavailable
            | FailureCode::Stopped
            | FailureCode::StateUnavailable
            | FailureCode::LegacyInputsMissing => FailureVariant::KnownFailure,
            FailureCode::UnsupportedVersion
            | FailureCode::UnsupportedSchema
            | FailureCode::MissingCapability
            | FailureCode::UnboundedProvider => FailureVariant::UnsupportedCapability,
            FailureCode::RevisionConflict
            | FailureCode::IdentityConflict
            | FailureCode::StaleGeneration
            | FailureCode::BaselineChanged => FailureVariant::Conflict,
            FailureCode::EffectUnconfirmed
            | FailureCode::TerminationUnconfirmed
            | FailureCode::UsageUnconfirmed => FailureVariant::UnknownOutcome,
        }
    }

    /// Parse a stable wire code; `None` when unrecognized.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "InvalidRecord" => FailureCode::InvalidRecord,
            "IntegrityMismatch" => FailureCode::IntegrityMismatch,
            "ScopeDenied" => FailureCode::ScopeDenied,
            "AuthorityDenied" => FailureCode::AuthorityDenied,
            "DeadlineExceeded" => FailureCode::DeadlineExceeded,
            "BudgetUnavailable" => FailureCode::BudgetUnavailable,
            "Stopped" => FailureCode::Stopped,
            "StateUnavailable" => FailureCode::StateUnavailable,
            "LegacyInputsMissing" => FailureCode::LegacyInputsMissing,
            "UnsupportedVersion" => FailureCode::UnsupportedVersion,
            "UnsupportedSchema" => FailureCode::UnsupportedSchema,
            "MissingCapability" => FailureCode::MissingCapability,
            "UnboundedProvider" => FailureCode::UnboundedProvider,
            "RevisionConflict" => FailureCode::RevisionConflict,
            "IdentityConflict" => FailureCode::IdentityConflict,
            "StaleGeneration" => FailureCode::StaleGeneration,
            "BaselineChanged" => FailureCode::BaselineChanged,
            "EffectUnconfirmed" => FailureCode::EffectUnconfirmed,
            "TerminationUnconfirmed" => FailureCode::TerminationUnconfirmed,
            "UsageUnconfirmed" => FailureCode::UsageUnconfirmed,
            _ => return None,
        })
    }

    /// Build the [`ContractError`] for this code carrying a `FailureDetail/v1`
    /// payload.
    pub fn failure(self, subject: impl AsRef<str>, message: impl AsRef<str>) -> ContractError {
        let detail = self.detail_json(subject.as_ref(), message.as_ref());
        match self.variant() {
            FailureVariant::KnownFailure => ContractError::KnownFailure(detail),
            FailureVariant::UnsupportedCapability => ContractError::UnsupportedCapability(detail),
            FailureVariant::Conflict => ContractError::Conflict(detail),
            FailureVariant::UnknownOutcome => ContractError::UnknownOutcome(detail),
        }
    }

    fn detail_json(self, subject: &str, message: &str) -> String {
        let value = serde_json::json!({
            "format": FAILURE_DETAIL_FORMAT,
            "code": self.as_str(),
            "subject": subject,
            "message": message,
        });
        canonical_json(&value)
            .unwrap_or_else(|_| format!("{}: {}: {}", self.as_str(), subject, message))
    }
}

/// The four [`ContractError`] variants a [`FailureCode`] maps onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureVariant {
    KnownFailure,
    UnsupportedCapability,
    Conflict,
    UnknownOutcome,
}

/// Extract the [`FailureCode`] carried by a structured error message, if any.
///
/// Legacy plain messages have no `FailureDetail/v1` payload and return `None`;
/// failure-detail parsing never grants retry authority.
pub fn failure_code(error: &ContractError) -> Option<FailureCode> {
    let message = match error {
        ContractError::KnownFailure(message)
        | ContractError::UnsupportedCapability(message)
        | ContractError::Conflict(message)
        | ContractError::UnknownOutcome(message) => message,
    };
    let value: Value = serde_json::from_str(message).ok()?;
    if value.get("format").and_then(Value::as_str) != Some(FAILURE_DETAIL_FORMAT) {
        return None;
    }
    FailureCode::parse(value.get("code").and_then(Value::as_str)?)
}
