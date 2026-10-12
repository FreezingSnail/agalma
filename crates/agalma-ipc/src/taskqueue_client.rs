//! `TaskQueueClient` (decisions D2, IPCX.5): a typed [`TaskQueueApi`] over the
//! task-queue boundary, on either wire transport, plus the projection
//! operations the conductor's reconcile drives.
//!
//! Method names are the existing method snake-case
//! (`ready_tasks`/`get`/`claim`/`comment`/`close_task`/`add_label` plus the
//! reconcile-mode `projection`/`set_status`/`remove_label`). Each call carries
//! one JSON params object and decodes a typed result. Transport failures map
//! onto the frozen [`ContractError`] surface exactly as for the
//! harness/sandbox/workspace clients: a mid-call connection loss is already
//! classified by the transport as `UnknownOutcome`; a pre-send loss becomes
//! `KnownFailure`.
//!
//! Since IPCX.5 the client can speak either [`Wire::Framed`] (B1 length-prefixed
//! JSON) or [`Wire::Connect`] (ConnectRPC over HTTP/2 on a Unix socket). The
//! envelope semantics are identical on both; only the byte transport differs.
//!
//! [`TaskQueueOps`] is the full operation surface a task-queue server hosts: the
//! frozen [`TaskQueueApi`] trait plus the projection operations the ledger-vs-bd
//! reconcile uses. It lets a single registration helper and a single conductor
//! handle cover both the in-process `BdTaskQueue` and the process-bound
//! `TaskQueueClient` without changing any work/reconcile logic.

use std::cell::RefCell;
use std::path::Path;

use agalma_contracts::{ClaimEvidence, ContractError, ReadyTasks, Task, TaskId, TaskQueueApi};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::connect::{ConnectClient, Wire};
use crate::error::IpcError;

/// A task's current bd projection (status, assignee, labels, comment bodies),
/// the derived state the reconcile compares against the ledger.
///
/// Defined here (infrastructure) rather than in the adapter so the wire surface
/// and the in-process backend share one type; `agalma-taskqueue` re-exports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueProjection {
    pub status: String,
    pub assignee: Option<String>,
    pub labels: Vec<String>,
    pub comments: Vec<String>,
}

impl IssueProjection {
    /// Whether the projection carries `label`.
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// Whether any comment body contains `needle`.
    pub fn has_comment(&self, needle: &str) -> bool {
        self.comments.iter().any(|c| c.contains(needle))
    }
}

/// Params for the id-scoped methods (`get`, `claim`, `projection`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskCall {
    pub id: TaskId,
}

/// Params for `comment`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommentParams {
    pub id: TaskId,
    pub text: String,
}

/// Params for `close_task`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CloseParams {
    pub id: TaskId,
    pub reason: String,
}

/// Params for `add_label` / `remove_label`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelParams {
    pub id: TaskId,
    pub label: String,
}

/// Params for `set_status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusParams {
    pub id: TaskId,
    pub status: String,
}

/// The full task-queue operation surface: the frozen [`TaskQueueApi`] plus the
/// projection operations introduced by the ledger-vs-bd reconcile (M1.6).
///
/// The conductor drives both the trait methods and the reconcile operations, so
/// extraction must carry all of them. `agalma-taskqueue` implements this for the
/// `bd`-backed `BdTaskQueue`; [`TaskQueueClient`] and the conductor's
/// `TaskQueueHandle` implement it over the transport (or by delegation).
pub trait TaskQueueOps: TaskQueueApi {
    /// Read an issue's current projection (status, assignee, labels, comments).
    fn projection(&self, id: &TaskId) -> Result<IssueProjection, ContractError>;

    /// Set an issue's status (the `closed` status routes through `close_task`).
    fn set_status(&mut self, id: &TaskId, status: &str) -> Result<(), ContractError>;

    /// Remove a label from an issue (no-op when absent).
    fn remove_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError>;
}

/// The transport backing a [`TaskQueueClient`].
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

/// A [`TaskQueueApi`]/[`TaskQueueOps`] implementation backed by a task-queue
/// component server.
pub struct TaskQueueClient {
    backend: RefCell<Backend>,
}

impl TaskQueueClient {
    /// Connect to the task-queue server listening on `socket` over the framed
    /// transport.
    pub fn connect(socket: &Path) -> Result<TaskQueueClient, IpcError> {
        TaskQueueClient::connect_wire(socket, Wire::Framed)
    }

    /// Connect to the task-queue server listening on `socket` over `wire`. Both
    /// wires preserve the envelope semantics.
    pub fn connect_wire(socket: &Path, wire: Wire) -> Result<TaskQueueClient, IpcError> {
        match wire {
            Wire::Framed => Ok(TaskQueueClient {
                backend: RefCell::new(Backend::Framed(Client::connect(socket)?)),
            }),
            Wire::Connect => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| IpcError::unavailable(format!("tokio runtime: {e}")))?;
                let client = runtime.block_on(ConnectClient::connect(socket))?;
                Ok(TaskQueueClient {
                    backend: RefCell::new(Backend::Connect(ConnectBackend { runtime, client })),
                })
            }
        }
    }

    /// Build a client from an already-connected transport (used by the process
    /// supervisor after the `READY` handshake).
    pub fn from_client(client: Client) -> TaskQueueClient {
        TaskQueueClient {
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
            Backend::Connect(backend) => backend
                .runtime
                .block_on(backend.client.call_typed(method, params))
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
            ContractError::KnownFailure(format!("taskqueue transport unavailable: {message}"))
        }
        IpcError::Protocol(message) => {
            ContractError::KnownFailure(format!("taskqueue protocol violation: {message}"))
        }
    }
}

impl TaskQueueApi for TaskQueueClient {
    fn ready_tasks(&self) -> Result<ReadyTasks, ContractError> {
        self.call("ready_tasks", &serde_json::json!({}))
    }

    fn get(&self, id: &TaskId) -> Result<Task, ContractError> {
        self.call("get", &TaskCall { id: id.clone() })
    }

    fn claim(&mut self, id: &TaskId) -> Result<ClaimEvidence, ContractError> {
        self.call("claim", &TaskCall { id: id.clone() })
    }

    fn comment(&mut self, id: &TaskId, text: &str) -> Result<(), ContractError> {
        self.call(
            "comment",
            &CommentParams {
                id: id.clone(),
                text: text.to_string(),
            },
        )
    }

    fn close_task(&mut self, id: &TaskId, reason: &str) -> Result<(), ContractError> {
        self.call(
            "close_task",
            &CloseParams {
                id: id.clone(),
                reason: reason.to_string(),
            },
        )
    }

    fn add_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        self.call(
            "add_label",
            &LabelParams {
                id: id.clone(),
                label: label.to_string(),
            },
        )
    }
}

impl TaskQueueOps for TaskQueueClient {
    fn projection(&self, id: &TaskId) -> Result<IssueProjection, ContractError> {
        self.call("projection", &TaskCall { id: id.clone() })
    }

    fn set_status(&mut self, id: &TaskId, status: &str) -> Result<(), ContractError> {
        self.call(
            "set_status",
            &StatusParams {
                id: id.clone(),
                status: status.to_string(),
            },
        )
    }

    fn remove_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        self.call(
            "remove_label",
            &LabelParams {
                id: id.clone(),
                label: label.to_string(),
            },
        )
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

    #[test]
    fn projection_predicates() {
        let projection = IssueProjection {
            status: "blocked".to_string(),
            assignee: None,
            labels: vec!["parked".to_string()],
            comments: vec!["phase=parked execution=exec:x:1 lease=1".to_string()],
        };
        assert!(projection.has_label("parked"));
        assert!(!projection.has_label("done"));
        assert!(projection.has_comment("phase=parked"));
        assert!(!projection.has_comment("phase=done"));
    }

    /// The client must satisfy the conductor seam `TaskQueueOps + Send`.
    #[test]
    fn taskqueue_client_is_a_send_taskqueue_ops() {
        fn assert_seam<T: TaskQueueOps + Send + 'static>() {}
        assert_seam::<TaskQueueClient>();
    }
}
