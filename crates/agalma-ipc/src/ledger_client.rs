//! `LedgerClient` (decision D2/D3): the durable-state [`LedgerApi`] over either
//! wire transport (B1 framed JSON or IPCX Connect), plus the versioned snapshot
//! file transfer the durable-state boundary requires.
//!
//! Method names are the existing method snake-case. Each call carries one JSON
//! params object and decodes a typed result. Transport failures map onto the
//! frozen [`ContractError`] surface exactly as for the harness/sandbox/
//! workspace/task-queue clients: a mid-call connection loss is already classified
//! by [`Client`] as `UnknownOutcome`; a pre-send loss becomes `KnownFailure`.
//!
//! ### Snapshot files, not frames (D3)
//!
//! The full canonical state snapshot can exceed the 16 MiB frame cap, so
//! `export_state`/`import_state` never travel as frames. The **server** owns the
//! database and writes/reads a snapshot file under the run state dir; the client
//! only transfers the *path*. `LedgerApi::export_state` on the client asks the
//! server to write the snapshot, then reads the file locally (same host) and
//! parses it; `import_state` writes the snapshot locally and asks the server to
//! import it. [`LedgerOps`] exposes the path-based primitives directly so a
//! quiesced rebind can hand the snapshot between servers without a round trip.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use agalma_contracts::{
    BindingRecord, CommitBatch, CommitReceipt, ContractError, ControlReceipt, DecisionOutcome,
    DecisionRequest, DigestRecord, DirectiveRow, DispatchIntent, ExecutionEvent, ExecutionId,
    ExecutionRecord, LedgerApi, OperationId, OperationReceipt, StateSnapshot, TaskId, TimerRecord,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::connect::{ConnectClient, Wire};
use crate::error::IpcError;

/// Snapshot file written by the server for `LedgerApi::export_state`.
pub const SNAPSHOT_EXPORT_FILE: &str = "ledger-export.json";
/// Snapshot file written by the client for `LedgerApi::import_state`.
pub const SNAPSHOT_IMPORT_FILE: &str = "ledger-import.json";

/// Params for `commit`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitParams {
    pub batch: CommitBatch,
}

/// Params for the execution-scoped methods (`execution`, `events`,
/// `digests_for_execution`, `cancel_timers_for_execution`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionCall {
    pub execution_id: ExecutionId,
}

/// Params for the operation-scoped methods (`operation_receipt`,
/// `decision_outcome`, `consume_dispatch_intent`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationIdCall {
    pub operation_id: OperationId,
}

/// Params for `record_decision_outcome`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutcomeParams {
    pub operation_id: OperationId,
    pub outcome: DecisionOutcome,
}

/// Params for the task-scoped lease methods (`lease_generation`, `begin_lease`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseCall {
    pub task: TaskId,
}

/// Params for `set_kill_latch`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatchParams {
    pub latched: bool,
}

/// Params for `upsert_binding`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BindingParams {
    pub binding: BindingRecord,
}

/// Params for `record_decision`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionParams {
    pub request: DecisionRequest,
}

/// Params for `record_digest`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DigestParams {
    pub digest: DigestRecord,
}

/// Params for `kernel_meta_get`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyCall {
    pub key: String,
}

/// Params for `kernel_meta_set`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaParams {
    pub key: String,
    pub value: String,
}

/// Params for `insert_timer`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimerParams {
    pub timer: TimerRecord,
}

/// Params for the timer-id methods (`timer`, `mark_timer_fired`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimerIdCall {
    pub timer_id: String,
}

/// Params for `due_timers`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DueTimersParams {
    pub now_unix_ms: u64,
}

/// Params for `upsert_directive`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectiveParams {
    pub directive: DirectiveRow,
}

/// Params for `directive`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectiveIdCall {
    pub directive_id: String,
}

/// Params for `record_control_receipt`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlReceiptParams {
    pub receipt: ControlReceipt,
}

/// Params for the snapshot file-transfer primitives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathParams {
    pub path: String,
}

/// The full durable-state operation surface a ledger server hosts: the frozen
/// [`LedgerApi`] plus the versioned snapshot **file** transfer required by D3
/// (the server writes/reads the file, the client transfers only the path).
///
/// `agalma-ledger` implements this for the SQLite-backed ledger; [`LedgerClient`]
/// and the conductor's `LedgerHandle` implement it over the transport (or by
/// delegation).
pub trait LedgerOps: LedgerApi {
    /// Write the complete canonical state snapshot to `path` (server-side).
    fn export_snapshot_to(&self, path: &Path) -> Result<(), ContractError>;

    /// Replace all durable state from the snapshot at `path` (server-side),
    /// refusing an incompatible format/schema rather than applying it partially.
    fn import_snapshot_from(&mut self, path: &Path) -> Result<(), ContractError>;
}

/// The transport backing a [`LedgerClient`] (IPCX.6).
enum Backend {
    /// B1 framed-JSON client ([`Client`]).
    Framed(Client),
    /// IPCX Connect client plus the runtime it is driven on.
    Connect(ConnectBackend),
}

/// Connect transport state: the async client and the runtime that drives it.
struct ConnectBackend {
    /// The runtime driving `client`; `None` after [`Drop`] tears it down.
    runtime: Option<tokio::runtime::Runtime>,
    client: ConnectClient,
}

impl Drop for ConnectBackend {
    fn drop(&mut self) {
        // The conductor builds and drops these clients from within its own Tokio
        // runtime (the async `agalma` binary). A blocking runtime drop is
        // forbidden in an async context, so shut the runtime down without
        // blocking; its transport tasks are abandoned with it.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// A [`LedgerApi`]/[`LedgerOps`] implementation backed by a ledger component
/// server.
///
/// Since IPCX.6 the client can speak either [`Wire::Framed`] (B1 length-prefixed
/// JSON) or [`Wire::Connect`] (ConnectRPC over HTTP/2 on a Unix socket). The
/// envelope semantics are identical on both; only the byte transport differs.
/// Snapshot transfer stays file-based on both wires (D3, unchanged).
pub struct LedgerClient {
    backend: RefCell<Backend>,
    /// Directory the snapshot files are written to/read from (the run state
    /// dir shared with the server).
    snapshot_dir: PathBuf,
}

impl LedgerClient {
    /// Connect to the ledger server listening on `socket` over the
    /// environment-selected wire ([`Wire::from_env`], `Connect` default since
    /// IPCX.6) — the same wire the supervised server serves by default. Snapshot
    /// files live under `snapshot_dir` (the run state dir shared with the
    /// server). Use [`LedgerClient::connect_wire`] to pin a wire explicitly.
    pub fn connect(socket: &Path, snapshot_dir: &Path) -> Result<LedgerClient, IpcError> {
        LedgerClient::connect_wire(socket, snapshot_dir, Wire::from_env(Wire::Connect))
    }

    /// Connect to the ledger server listening on `socket` over `wire`; snapshot
    /// files live under `snapshot_dir` (the run state dir shared with the
    /// server). Both wires preserve the envelope semantics.
    pub fn connect_wire(
        socket: &Path,
        snapshot_dir: &Path,
        wire: Wire,
    ) -> Result<LedgerClient, IpcError> {
        let backend = match wire {
            Wire::Framed => Backend::Framed(Client::connect(socket)?),
            Wire::Connect => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| IpcError::unavailable(format!("tokio runtime: {e}")))?;
                let client = crate::block_on_nested(&runtime, ConnectClient::connect(socket))?;
                Backend::Connect(ConnectBackend {
                    runtime: Some(runtime),
                    client,
                })
            }
        };
        Ok(LedgerClient {
            backend: RefCell::new(backend),
            snapshot_dir: snapshot_dir.to_path_buf(),
        })
    }

    /// Build a framed client from an already-connected transport.
    pub fn from_client(client: Client, snapshot_dir: &Path) -> LedgerClient {
        LedgerClient {
            backend: RefCell::new(Backend::Framed(client)),
            snapshot_dir: snapshot_dir.to_path_buf(),
        }
    }

    /// The snapshot directory shared with the server.
    pub fn snapshot_dir(&self) -> &Path {
        &self.snapshot_dir
    }

    fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: &impl Serialize,
    ) -> Result<R, ContractError> {
        match &mut *self.backend.borrow_mut() {
            Backend::Framed(client) => client.call_typed(method, params).map_err(map_ipc_error),
            Backend::Connect(backend) => {
                crate::block_on_nested(
                    backend.runtime.as_ref().expect("runtime present until drop"),
                    backend.client.call_typed(method, params),
                )
                    .map_err(map_ipc_error)
            }
        }
    }

    fn read_snapshot(&self, file: &str) -> Result<StateSnapshot, ContractError> {
        let path = self.snapshot_dir.join(file);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            ContractError::KnownFailure(format!("read snapshot {}: {e}", path.display()))
        })?;
        StateSnapshot::from_json(&text)
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
            ContractError::KnownFailure(format!("ledger transport unavailable: {message}"))
        }
        IpcError::Protocol(message) => {
            ContractError::KnownFailure(format!("ledger protocol violation: {message}"))
        }
    }
}

impl LedgerApi for LedgerClient {
    fn commit(&mut self, batch: CommitBatch) -> Result<CommitReceipt, ContractError> {
        self.call("commit", &CommitParams { batch })
    }

    fn execution(&self, id: &ExecutionId) -> Result<Option<ExecutionRecord>, ContractError> {
        self.call(
            "execution",
            &ExecutionCall {
                execution_id: id.clone(),
            },
        )
    }

    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError> {
        self.call("executions", &serde_json::json!({}))
    }

    fn pending_intents(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        self.call("pending_intents", &serde_json::json!({}))
    }

    fn events(&self, id: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError> {
        self.call(
            "events",
            &ExecutionCall {
                execution_id: id.clone(),
            },
        )
    }

    fn operation_receipt(
        &self,
        id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        self.call(
            "operation_receipt",
            &OperationIdCall {
                operation_id: id.clone(),
            },
        )
    }

    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        self.call("lease_generation", &LeaseCall { task: task.clone() })
    }

    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError> {
        self.call("begin_lease", &LeaseCall { task: task.clone() })
    }

    fn kill_latch(&self) -> Result<bool, ContractError> {
        self.call("kill_latch", &serde_json::json!({}))
    }

    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError> {
        self.call("set_kill_latch", &LatchParams { latched })
    }

    fn upsert_binding(&mut self, binding: &BindingRecord) -> Result<(), ContractError> {
        self.call(
            "upsert_binding",
            &BindingParams {
                binding: binding.clone(),
            },
        )
    }

    fn bindings(&self) -> Result<Vec<BindingRecord>, ContractError> {
        self.call("bindings", &serde_json::json!({}))
    }

    fn schema_version(&self) -> Result<u32, ContractError> {
        self.call("schema_version", &serde_json::json!({}))
    }

    fn record_decision(&mut self, request: &DecisionRequest) -> Result<(), ContractError> {
        self.call(
            "record_decision",
            &DecisionParams {
                request: request.clone(),
            },
        )
    }

    fn record_decision_outcome(
        &mut self,
        operation_id: &OperationId,
        outcome: &DecisionOutcome,
    ) -> Result<(), ContractError> {
        self.call(
            "record_decision_outcome",
            &OutcomeParams {
                operation_id: operation_id.clone(),
                outcome: outcome.clone(),
            },
        )
    }

    fn decision_outcome(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<DecisionOutcome>, ContractError> {
        self.call(
            "decision_outcome",
            &OperationIdCall {
                operation_id: operation_id.clone(),
            },
        )
    }

    fn record_digest(&mut self, digest: &DigestRecord) -> Result<(), ContractError> {
        self.call(
            "record_digest",
            &DigestParams {
                digest: digest.clone(),
            },
        )
    }

    fn digests_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<DigestRecord>, ContractError> {
        self.call(
            "digests_for_execution",
            &ExecutionCall {
                execution_id: execution_id.clone(),
            },
        )
    }

    fn kernel_meta_get(&self, key: &str) -> Result<Option<String>, ContractError> {
        self.call(
            "kernel_meta_get",
            &KeyCall {
                key: key.to_string(),
            },
        )
    }

    fn kernel_meta_set(&mut self, key: &str, value: &str) -> Result<(), ContractError> {
        self.call(
            "kernel_meta_set",
            &MetaParams {
                key: key.to_string(),
                value: value.to_string(),
            },
        )
    }

    fn insert_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError> {
        self.call(
            "insert_timer",
            &TimerParams {
                timer: timer.clone(),
            },
        )
    }

    fn timer(&self, timer_id: &str) -> Result<Option<TimerRecord>, ContractError> {
        self.call(
            "timer",
            &TimerIdCall {
                timer_id: timer_id.to_string(),
            },
        )
    }

    fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError> {
        self.call("due_timers", &DueTimersParams { now_unix_ms })
    }

    fn mark_timer_fired(&mut self, timer_id: &str) -> Result<(), ContractError> {
        self.call(
            "mark_timer_fired",
            &TimerIdCall {
                timer_id: timer_id.to_string(),
            },
        )
    }

    fn cancel_timers_for_execution(
        &mut self,
        execution_id: &ExecutionId,
    ) -> Result<u64, ContractError> {
        self.call(
            "cancel_timers_for_execution",
            &ExecutionCall {
                execution_id: execution_id.clone(),
            },
        )
    }

    fn next_deadline(&self) -> Result<Option<u64>, ContractError> {
        self.call("next_deadline", &serde_json::json!({}))
    }

    fn upsert_directive(&mut self, directive: &DirectiveRow) -> Result<bool, ContractError> {
        self.call(
            "upsert_directive",
            &DirectiveParams {
                directive: directive.clone(),
            },
        )
    }

    fn directive(&self, directive_id: &str) -> Result<Option<DirectiveRow>, ContractError> {
        self.call(
            "directive",
            &DirectiveIdCall {
                directive_id: directive_id.to_string(),
            },
        )
    }

    fn consume_dispatch_intent(&mut self, operation_id: &OperationId) -> Result<(), ContractError> {
        self.call(
            "consume_dispatch_intent",
            &OperationIdCall {
                operation_id: operation_id.clone(),
            },
        )
    }

    fn record_control_receipt(&mut self, receipt: &ControlReceipt) -> Result<(), ContractError> {
        self.call(
            "record_control_receipt",
            &ControlReceiptParams {
                receipt: receipt.clone(),
            },
        )
    }

    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        self.call("control_receipts", &serde_json::json!({}))
    }

    fn export_state(&self) -> Result<StateSnapshot, ContractError> {
        let path = self.snapshot_dir.join(SNAPSHOT_EXPORT_FILE);
        self.call::<()>(
            "export_state_to",
            &PathParams {
                path: path.to_string_lossy().into_owned(),
            },
        )?;
        self.read_snapshot(SNAPSHOT_EXPORT_FILE)
    }

    fn import_state(&mut self, snapshot: &StateSnapshot) -> Result<(), ContractError> {
        let path = self.snapshot_dir.join(SNAPSHOT_IMPORT_FILE);
        let text = snapshot.to_canonical_json()?;
        std::fs::write(&path, text).map_err(|e| {
            ContractError::KnownFailure(format!("write snapshot {}: {e}", path.display()))
        })?;
        self.call::<()>(
            "import_state_from",
            &PathParams {
                path: path.to_string_lossy().into_owned(),
            },
        )
    }
}

impl LedgerOps for LedgerClient {
    fn export_snapshot_to(&self, path: &Path) -> Result<(), ContractError> {
        self.call::<()>(
            "export_state_to",
            &PathParams {
                path: path.to_string_lossy().into_owned(),
            },
        )
    }

    fn import_snapshot_from(&mut self, path: &Path) -> Result<(), ContractError> {
        self.call::<()>(
            "import_state_from",
            &PathParams {
                path: path.to_string_lossy().into_owned(),
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

    /// The client must satisfy the conductor seam `LedgerApi + Send`.
    #[test]
    fn ledger_client_is_a_send_ledger_api() {
        fn assert_seam<T: LedgerApi + Send + 'static>() {}
        assert_seam::<LedgerClient>();
    }
}
