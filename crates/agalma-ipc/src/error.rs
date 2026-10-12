//! Transport-level error taxonomy for `agalma-ipc`.
//!
//! The contract error set (`KnownFailure`, `UnsupportedCapability`, `Conflict`,
//! `UnknownOutcome`) is a **frozen seam** (`docs/spikes/s0d-seam-freeze.md`,
//! `crates/agalma-contracts/src/error.rs`). B1 must not change contract
//! schemas, so this crate does not add a variant to
//! [`agalma_contracts::ContractError`]. Instead it defines [`IpcError`], which
//! reuses `ContractError` for every contract-shaped failure and adds the two
//! transport-only outcomes the design names (D1): `Unavailable` and `Protocol`.
//!
//! Wire error codes are therefore the four `ContractError` variant names plus
//! [`CODE_UNAVAILABLE`] and [`CODE_PROTOCOL`].

use agalma_contracts::ContractError;

/// Wire error code for a transport fault (not a contract variant).
pub const CODE_UNAVAILABLE: &str = "Unavailable";
/// Wire error code for a protocol violation (framing, version, id mismatch).
pub const CODE_PROTOCOL: &str = "Protocol";

/// Transport-level failure observed by an IPC client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    /// The server returned a contract error. Carries `ContractError`, including
    /// `UnknownOutcome` when an in-flight call's effect cannot be known.
    Contract(ContractError),
    /// The transport is (or became) unavailable: cannot connect, peer gone
    /// before the request was sent. Safe to retry: no effect can have occurred.
    Unavailable(String),
    /// The exchange violated the protocol: bad framing, unsupported version,
    /// mismatched response id, or an unrecognized error code.
    Protocol(String),
}

impl IpcError {
    /// Construct an `Unavailable` transport error.
    pub fn unavailable(message: impl Into<String>) -> Self {
        IpcError::Unavailable(message.into())
    }

    /// Construct an `UnknownOutcome` error: the request was sent but its result
    /// could not be observed. Reconcile through inspection APIs before retrying.
    pub fn unknown_outcome(message: impl Into<String>) -> Self {
        IpcError::Contract(ContractError::UnknownOutcome(message.into()))
    }

    /// Construct a `Protocol` error.
    pub fn protocol(message: impl Into<String>) -> Self {
        IpcError::Protocol(message.into())
    }

    /// True for the transport `Unavailable` error.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, IpcError::Unavailable(_))
    }

    /// True when the outcome of an effectful call cannot be known.
    pub fn is_unknown_outcome(&self) -> bool {
        matches!(self, IpcError::Contract(ContractError::UnknownOutcome(_)))
    }

    /// Decode a wire `(code, message)` pair into an [`IpcError`].
    pub fn from_wire(code: &str, message: &str) -> IpcError {
        match code {
            "KnownFailure" => IpcError::Contract(ContractError::KnownFailure(message.to_string())),
            "UnsupportedCapability" => {
                IpcError::Contract(ContractError::UnsupportedCapability(message.to_string()))
            }
            "Conflict" => IpcError::Contract(ContractError::Conflict(message.to_string())),
            "UnknownOutcome" => {
                IpcError::Contract(ContractError::UnknownOutcome(message.to_string()))
            }
            CODE_UNAVAILABLE => IpcError::Unavailable(message.to_string()),
            CODE_PROTOCOL => IpcError::Protocol(message.to_string()),
            other => IpcError::Protocol(format!("unknown error code `{other}`: {message}")),
        }
    }

    /// Encode this error as a wire `(code, message)` pair.
    pub fn to_wire(&self) -> (&'static str, String) {
        match self {
            IpcError::Contract(e) => contract_error_wire(e),
            IpcError::Unavailable(m) => (CODE_UNAVAILABLE, m.clone()),
            IpcError::Protocol(m) => (CODE_PROTOCOL, m.clone()),
        }
    }
}

/// Map a [`ContractError`] to its wire code and message.
pub fn contract_error_wire(error: &ContractError) -> (&'static str, String) {
    match error {
        ContractError::KnownFailure(m) => ("KnownFailure", m.clone()),
        ContractError::UnsupportedCapability(m) => ("UnsupportedCapability", m.clone()),
        ContractError::Conflict(m) => ("Conflict", m.clone()),
        ContractError::UnknownOutcome(m) => ("UnknownOutcome", m.clone()),
    }
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcError::Contract(e) => write!(f, "{e}"),
            IpcError::Unavailable(m) => write!(f, "transport unavailable: {m}"),
            IpcError::Protocol(m) => write!(f, "protocol violation: {m}"),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IpcError::Contract(e) => Some(e),
            IpcError::Unavailable(_) | IpcError::Protocol(_) => None,
        }
    }
}

impl From<ContractError> for IpcError {
    fn from(e: ContractError) -> Self {
        IpcError::Contract(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_variants_round_trip_through_wire() {
        let cases = [
            ContractError::KnownFailure("a".into()),
            ContractError::UnsupportedCapability("b".into()),
            ContractError::Conflict("c".into()),
            ContractError::UnknownOutcome("d".into()),
        ];
        for case in cases {
            let (code, message) = contract_error_wire(&case);
            assert_eq!(
                IpcError::from_wire(code, &message),
                IpcError::Contract(case)
            );
        }
    }

    #[test]
    fn transport_codes_round_trip() {
        let cases = [
            IpcError::unavailable("gone"),
            IpcError::protocol("bad frame"),
        ];
        for case in cases {
            let (code, message) = case.to_wire();
            assert_eq!(IpcError::from_wire(code, &message), case);
        }
    }

    #[test]
    fn unknown_code_is_a_protocol_error() {
        assert!(matches!(
            IpcError::from_wire("Nope", "x"),
            IpcError::Protocol(_)
        ));
    }

    #[test]
    fn predicates_classify_outcomes() {
        assert!(IpcError::unavailable("x").is_unavailable());
        assert!(IpcError::unknown_outcome("x").is_unknown_outcome());
        assert!(!IpcError::protocol("x").is_unavailable());
    }
}
