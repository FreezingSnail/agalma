//! `SandboxClient` (decisions D2, IPCX.3): a typed [`SandboxApi`] over the
//! sandbox boundary, on either wire transport.
//!
//! Method names are the trait method snake-case (`launch`/`alive`/`terminate`);
//! each call carries one JSON params object and decodes a typed result.
//! Transport failures map onto the frozen [`ContractError`] surface exactly as
//! for the harness client: a mid-call connection loss is already classified by
//! the transport as `UnknownOutcome`; a pre-send loss becomes `KnownFailure`.
//!
//! Since IPCX.3 the client can speak either [`Wire::Framed`] (B1 length-prefixed
//! JSON) or [`Wire::Connect`] (ConnectRPC over HTTP/2 on a Unix socket). The
//! envelope semantics are identical on both; only the byte transport differs.

use std::cell::RefCell;
use std::path::Path;

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};
use agalma_contracts::ContractError;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::client::Client;
use crate::connect::{ConnectClient, Wire};
use crate::error::IpcError;

/// The transport backing a [`SandboxClient`].
enum Backend {
    /// B1 framed-JSON client ([`Client`]).
    Framed(Client),
    /// IPCX Connect client plus the runtime it is driven on.
    Connect(ConnectBackend),
}

/// Connect transport state: the async client and the runtime that drives it.
struct ConnectBackend {
    runtime: tokio::runtime::Runtime,
    client: ConnectClient,
}

/// A [`SandboxApi`] implementation backed by a sandbox component server.
pub struct SandboxClient {
    backend: RefCell<Backend>,
}

impl SandboxClient {
    /// Connect to `socket` over the framed transport.
    pub fn connect(socket: &Path) -> Result<SandboxClient, IpcError> {
        SandboxClient::connect_wire(socket, Wire::Framed)
    }

    /// Connect to the sandbox server listening on `socket` over `wire`. Both
    /// wires preserve the envelope semantics.
    pub fn connect_wire(socket: &Path, wire: Wire) -> Result<SandboxClient, IpcError> {
        match wire {
            Wire::Framed => Ok(SandboxClient {
                backend: RefCell::new(Backend::Framed(Client::connect(socket)?)),
            }),
            Wire::Connect => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| IpcError::unavailable(format!("tokio runtime: {e}")))?;
                let client = crate::block_on_nested(&runtime, ConnectClient::connect(socket))?;
                Ok(SandboxClient {
                    backend: RefCell::new(Backend::Connect(ConnectBackend { runtime, client })),
                })
            }
        }
    }

    /// Build a client from an already-connected transport (used by the process
    /// supervisor after the `READY` handshake).
    pub fn from_client(client: Client) -> SandboxClient {
        SandboxClient {
            backend: RefCell::new(Backend::Framed(client)),
        }
    }

    fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: &impl Serialize,
    ) -> Result<R, ContractError> {
        match &mut *self.backend.borrow_mut() {
            Backend::Framed(client) => client.call_typed(method, params).map_err(map_ipc_error),
            // The OpenCode harness server both *serves* over Connect and drives
            // this outbound sandbox client from inside its async handler, so the
            // per-call block must tolerate an enclosing runtime (`agalma-3ej`).
            Backend::Connect(backend) => crate::block_on_nested(
                &backend.runtime,
                backend.client.call_typed(method, params),
            )
            .map_err(map_ipc_error),
        }
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
