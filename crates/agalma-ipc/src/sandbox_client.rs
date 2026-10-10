//! `SandboxClient` (decision D2): a typed [`SandboxApi`] over the framed client.
//!
//! Method names are the trait method snake-case (`launch`/`alive`/`terminate`);
//! each call carries one JSON params object and decodes a typed result.
//! Transport failures map onto the frozen [`ContractError`] surface exactly as
//! for the harness client: a mid-call connection loss is already classified by
//! [`Client`] as `UnknownOutcome`; a pre-send loss becomes `KnownFailure`.

use std::cell::RefCell;
use std::path::Path;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};
use agalma_contracts::ContractError;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::client::Client;
use crate::error::IpcError;

/// A [`SandboxApi`] implementation backed by a sandbox component server.
pub struct SandboxClient {
    client: RefCell<Client>,
}

impl SandboxClient {
    /// Connect to the sandbox server listening on `socket`.
    pub fn connect(socket: &Path) -> Result<SandboxClient, IpcError> {
        Ok(SandboxClient {
            client: RefCell::new(Client::connect(socket)?),
        })
    }

    /// Build a client from an already-connected transport (used by the process
    /// supervisor after the `READY` handshake).
    pub fn from_client(client: Client) -> SandboxClient {
        SandboxClient {
            client: RefCell::new(client),
        }
    }

    fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: &impl Serialize,
    ) -> Result<R, ContractError> {
        let mut client = self.client.borrow_mut();
        client.call_typed(method, params).map_err(map_ipc_error)
    }
}

/// Map a transport error onto the frozen [`ContractError`] surface.
///
/// `IpcError::Contract` passes through unchanged (including `UnknownOutcome`).
/// Transport-only faults have no contract variant: `Unavailable`/`Protocol`
/// become `KnownFailure` (retry-safe or caller-visible framing faults).
pub fn map_ipc_error(err: IpcError) -> ContractError {
    match err {
        IpcError::Contract(error) => error,
        IpcError::Unavailable(message) => {
            ContractError::KnownFailure(format!("sandbox transport unavailable: {message}"))
        }
        IpcError::Protocol(message) => {
            ContractError::KnownFailure(format!("sandbox protocol violation: {message}"))
        }
    }
}

impl SandboxApi for SandboxClient {
    fn launch(&mut self, spec: LaunchSpec) -> Result<SandboxChild, ContractError> {
        self.call("launch", &spec)
    }

    fn alive(&self, child: &SandboxChild) -> Result<bool, ContractError> {
        self.call("alive", child)
    }

    fn terminate(&mut self, child: &SandboxChild) -> Result<StopEvidence, ContractError> {
        self.call("terminate", child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_error_passes_through() {
        let err = map_ipc_error(IpcError::Contract(ContractError::Conflict("x".into())));
        assert!(matches!(err, ContractError::Conflict(_)));
    }

    #[test]
    fn unknown_outcome_passes_through() {
        let err = map_ipc_error(IpcError::unknown_outcome("lost"));
        assert!(matches!(err, ContractError::UnknownOutcome(_)));
    }

    #[test]
    fn unavailable_maps_to_known_failure() {
        let err = map_ipc_error(IpcError::unavailable("gone"));
        assert!(matches!(err, ContractError::KnownFailure(_)));
    }

    /// The client must satisfy the kernel/harness seam `Box<dyn SandboxApi + Send>`.
    #[test]
    fn sandbox_client_is_a_send_sandbox_api() {
        fn assert_seam<T: SandboxApi + Send + 'static>() {}
        assert_seam::<SandboxClient>();
    }
}
