//! `HarnessClient` (decisions D2, IPCX.2): a typed [`HarnessApi`] over the
//! harness boundary, on either wire transport.
//!
//! Method names are the trait method snake-case; each call carries one JSON
//! params object and decodes a typed result. `describe` is the identity call.
//! Transport failures are mapped to [`ContractError`] for the frozen trait
//! surface: a mid-call connection loss is already classified by the transport as
//! `UnknownOutcome`; a pre-send loss becomes `KnownFailure` (retry-safe).
//!
//! Since IPCX.2 the client can speak either [`Wire::Framed`] (B1 length-prefixed
//! JSON) or [`Wire::Connect`] (ConnectRPC over HTTP/2 on a Unix socket). The
//! envelope semantics are identical on both; only the byte transport differs.

use std::cell::RefCell;
use std::path::Path;

use agalma_contracts::harness::{
    AttemptHandle, CancelAck, CreateSessionRequest, Describe, Event, HarnessApi, OperationHandle,
    OperationState, RunTurnRequest, SessionHandle, StartAttemptRequest, StopEvidence,
};
use agalma_contracts::ContractError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::Client;
use crate::connect::{ConnectClient, Wire};
use crate::error::IpcError;

/// Params for `run_turn`: the session handle plus the request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunTurnCall {
    pub session: SessionHandle,
    pub request: RunTurnRequest,
}

/// Params for the operation-scoped methods (`inspect_operation`, `read_events`,
/// `cancel_operation`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationCall {
    pub operation: OperationHandle,
}

/// Params for `close_session`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionCall {
    pub session: SessionHandle,
}

/// Params for `stop_attempt`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptCall {
    pub attempt: AttemptHandle,
}

/// The transport backing a [`HarnessClient`].
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

/// A [`HarnessApi`] implementation backed by a component server process.
pub struct HarnessClient {
    backend: RefCell<Backend>,
    /// Identity cached at construction (from a `describe` call) so the
    /// infallible `describe()` trait method always has an answer.
    describe: Describe,
}

impl HarnessClient {
    /// Connect to `socket` over the framed transport and fetch the server
    /// identity with a `describe` call.
    pub fn connect(socket: &Path) -> Result<HarnessClient, IpcError> {
        HarnessClient::connect_wire(socket, Wire::Framed)
    }

    /// Connect to `socket` over `wire` and fetch the server identity with a
    /// `describe` call. Both wires preserve the envelope semantics.
    pub fn connect_wire(socket: &Path, wire: Wire) -> Result<HarnessClient, IpcError> {
        match wire {
            Wire::Framed => {
                let mut client = Client::connect(socket)?;
                let describe = describe_framed(&mut client)?;
                Ok(HarnessClient {
                    backend: RefCell::new(Backend::Framed(client)),
                    describe,
                })
            }
            Wire::Connect => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| IpcError::unavailable(format!("tokio runtime: {e}")))?;
                let mut client = runtime.block_on(ConnectClient::connect(socket))?;
                let describe = runtime.block_on(describe_connect(&mut client))?;
                Ok(HarnessClient {
                    backend: RefCell::new(Backend::Connect(ConnectBackend { runtime, client })),
                    describe,
                })
            }
        }
    }

    /// Build a client from an existing framed connection and a known identity
    /// (used by the process supervisor after it runs the bind gate on the READY
    /// line).
    pub fn from_client(client: Client, describe: Describe) -> HarnessClient {
        HarnessClient {
            backend: RefCell::new(Backend::Framed(client)),
            describe,
        }
    }

    /// The identity cached at construction.
    pub fn cached_describe(&self) -> &Describe {
        &self.describe
    }

    /// Issue a `describe` request live; on transport failure, the cached
    /// identity is returned so the infallible trait method still works.
    pub fn describe_live(&self) -> Result<Describe, IpcError> {
        match &mut *self.backend.borrow_mut() {
            Backend::Framed(client) => describe_framed(client),
            Backend::Connect(backend) => backend
                .runtime
                .block_on(describe_connect(&mut backend.client)),
        }
    }

    fn call<R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &impl Serialize,
    ) -> Result<R, ContractError> {
        match &mut *self.backend.borrow_mut() {
            Backend::Framed(client) => client.call_typed(method, params).map_err(map_ipc_error),
            Backend::Connect(backend) => backend
                .runtime
                .block_on(backend.client.call_typed(method, params))
                .map_err(map_ipc_error),
        }
    }
}

/// Map a transport error onto the frozen [`ContractError`] surface.
///
/// `IpcError::Contract` passes the contract error through unchanged
/// (`UnknownOutcome` included). Transport-only faults have no contract variant,
/// so `Unavailable`/`Protocol` map to `KnownFailure`: they are retry-safe or
/// caller-visible framing faults, never a silently-unknown effect.
pub fn map_ipc_error(err: IpcError) -> ContractError {
    match err {
        IpcError::Contract(error) => error,
        IpcError::Unavailable(message) => {
            ContractError::KnownFailure(format!("harness transport unavailable: {message}"))
        }
        IpcError::Protocol(message) => {
            ContractError::KnownFailure(format!("harness protocol violation: {message}"))
        }
    }
}

fn describe_framed(client: &mut Client) -> Result<Describe, IpcError> {
    client.call_typed("describe", &json!({}))
}

async fn describe_connect(client: &mut ConnectClient) -> Result<Describe, IpcError> {
    client.call_typed("describe", &json!({})).await
}

/// Decode a params object into `T`, reporting a contract failure when malformed.
pub fn decode_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, ContractError> {
    serde_json::from_value(params)
        .map_err(|e| ContractError::KnownFailure(format!("invalid params: {e}")))
}

/// Encode a result value, reporting a contract failure when it cannot serialize.
pub fn encode_result<T: Serialize>(value: T) -> Result<Value, ContractError> {
    serde_json::to_value(value)
        .map_err(|e| ContractError::KnownFailure(format!("encode result: {e}")))
}

impl HarnessApi for HarnessClient {
    fn describe(&self) -> Describe {
        match self.describe_live() {
            Ok(describe) => describe,
            Err(_) => self.describe.clone(),
        }
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, ContractError> {
        self.call("start_attempt", &req)
    }

    fn create_session(
        &mut self,
        req: CreateSessionRequest,
    ) -> Result<SessionHandle, ContractError> {
        self.call("create_session", &req)
    }

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError> {
        self.call(
            "run_turn",
            &RunTurnCall {
                session: session.clone(),
                request: req,
            },
        )
    }

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, ContractError> {
        self.call(
            "inspect_operation",
            &OperationCall {
                operation: op.clone(),
            },
        )
    }

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
        self.call(
            "read_events",
            &OperationCall {
                operation: op.clone(),
            },
        )
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, ContractError> {
        self.call(
            "cancel_operation",
            &OperationCall {
                operation: op.clone(),
            },
        )
    }

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), ContractError> {
        self.call(
            "close_session",
            &SessionCall {
                session: session.clone(),
            },
        )
    }

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, ContractError> {
        self.call(
            "stop_attempt",
            &AttemptCall {
                attempt: attempt.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::IpcError;

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

    #[test]
    fn decode_params_reports_malformed_input() {
        let err = decode_params::<AttemptCall>(serde_json::json!({"nope": 1})).unwrap_err();
        assert!(matches!(err, ContractError::KnownFailure(_)));
    }
}
