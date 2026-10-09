//! Contract error taxonomy.
//!
//! Every component API returns this taxonomy. A retryable error never by itself
//! authorizes repeating an effect: `UnknownOutcome` in particular means the
//! caller must reconcile the external effect before retrying (documented in
//! `docs/component-contracts.md`, "Operations and cancellation").

use serde::{Deserialize, Serialize};

/// Errors shared across Agalma component contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ContractError {
    /// The operation failed for a known reason.
    #[error("known failure: {0}")]
    KnownFailure(String),
    /// The requested capability is not advertised by the bound implementation.
    #[error("unsupported capability: {0}")]
    UnsupportedCapability(String),
    /// The request conflicts with current state (e.g. a changed input under an
    /// existing operation ID, or a failed compare-and-update revision check).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The external effect may or may not have happened; reconcile before retry.
    #[error("unknown outcome: {0}")]
    UnknownOutcome(String),
}
