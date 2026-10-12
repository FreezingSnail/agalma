//! `agalma-ipc` — framed JSON transport over a Unix socket (B1.1 foundation).
//!
//! This crate implements decision D1 of `docs/b1-extraction.md`: a length-
//! prefixed JSON protocol used to move Tier-1 component implementations behind a
//! process boundary. It provides:
//!
//! - [`frame`]: the wire codec (4-byte big-endian `u32` length + UTF-8 JSON,
//!   16 MiB cap).
//! - [`envelope`]: request/response envelopes and the protocol version.
//! - [`error`]: [`IpcError`], the transport-level error taxonomy.
//! - [`server`]: the server runtime (bind, `READY` handshake, serial dispatch,
//!   receipt replay, version refusal).
//! - [`client`]: the client core (connect, serial `call`, typed conversion,
//!   connection-loss semantics).
//! - [`sandbox_client`]/[`sandbox_server`]: the typed `SandboxApi` client and
//!   its method-registration helper (the second extracted boundary, B1.3).
//! - [`workspace_client`]/[`workspace_server`]: the typed `WorkspaceApi` +
//!   repo-mode client and its method-registration helper (B1.4).
//! - [`taskqueue_client`]/[`taskqueue_server`]: the typed `TaskQueueApi` +
//!   reconcile-projection client and its method-registration helper (B1.5).
//! - [`ledger_client`]/[`ledger_server`]: the typed durable-state `LedgerApi` +
//!   snapshot file-transfer client and its method-registration helper (B1.6).
//! - [`execution_client`]/[`execution_server`]: the kernel-candidate
//!   `ExecutionApi` + kernel engine surface client and its method-registration
//!   helper (B1.7).
//!
//! `agalma-ipc` is **infrastructure**, not an implementation crate: the
//! architecture lint (`crates/agalma-conductor/tests/architecture.rs`) permits
//! implementation crates to depend on it, while still forbidding impl→impl
//! edges. It depends only on `agalma-contracts`, `serde`, and `serde_json`.

pub mod client;
pub mod command;
pub mod connect;
pub mod envelope;
pub mod error;
pub mod execution_client;
pub mod execution_handler;
pub mod execution_server;
pub mod frame;
pub mod harness_client;
pub mod harness_server;
pub mod ledger_client;
pub mod ledger_server;
pub mod sandbox_client;
pub mod sandbox_server;
pub mod server;
pub mod taskqueue_client;
pub mod taskqueue_server;
pub mod workspace_client;
pub mod workspace_server;

/// Generated ConnectRPC envelope types for `proto/ipc.proto` (design D1/D2).
///
/// `connectrpc-build` runs in `build.rs` and emits the `agalma.ipc.v1` module
/// tree (`IpcRequest`/`IpcResponse`/`IpcError`, the `IpcService` trait, and the
/// `IpcServiceClient`); mounted here as `crate::proto::agalma::ipc::v1`.
pub mod proto {
    connectrpc::include_generated!();
}

pub use client::Client;
pub use command::CommandSpec;
pub use connect::{ConnectClient, IpcDispatcher};
pub use envelope::{Request, Response, ResponseError, PROTOCOL_VERSION};
pub use error::{IpcError, CODE_PROTOCOL, CODE_UNAVAILABLE};
pub use execution_client::{
    BeginStepCall, CompleteStepCall, DirectiveExecCall, ExecIdCall, ExecutionClient, ExecutionOps,
    KernelCall, LeaseExecCall, NowCall, PendingStepCall, ReceiptCall, ReclaimCall, SnapshotPathCall,
    StartCall, TimerExecParams,
};
pub use execution_handler::{
    EffectContext, EffectDispatch, EffectMode, EffectProbe, EffectReport, HandlerError, HandlerKey,
    StepBegin,
};
pub use execution_server::{register_execution, EXECUTION_METHODS};
pub use frame::{FrameError, MAX_FRAME};
pub use harness_client::{
    AttemptCall, HarnessClient, OperationCall, RunTurnCall, SessionCall,
};
pub use harness_server::{ensure_all_handlers, register_harness, server_info_from, HARNESS_METHODS};
pub use ledger_client::{
    BindingParams, CommitParams, DecisionParams, DigestParams, DirectiveIdCall, DirectiveParams,
    DueTimersParams, ExecutionCall, KeyCall, LatchParams, LeaseCall, LedgerClient, LedgerOps,
    MetaParams, OperationIdCall, OutcomeParams, PathParams, TimerIdCall, TimerParams,
    SNAPSHOT_EXPORT_FILE, SNAPSHOT_IMPORT_FILE,
};
pub use ledger_server::{register_ledger, LEDGER_METHODS};
pub use sandbox_client::SandboxClient;
pub use sandbox_server::{register_sandbox, SANDBOX_METHODS};
pub use server::{socket_from_args, Handler, Server, ServerInfo};
pub use taskqueue_client::{
    CloseParams, CommentParams, IssueProjection, LabelParams, StatusParams, TaskCall,
    TaskQueueClient, TaskQueueOps,
};
pub use taskqueue_server::{register_taskqueue, TASKQUEUE_METHODS};
pub use workspace_client::{
    CheckoutCall, IntegrateOriginParams, IntegrateParams, OriginReceiptParams, PrepareParams,
    PrepareRepoParams, PrepareVerifyParams, RevertOriginParams, RevertParams, TagParams,
    WorkspaceClient, WorkspaceOps,
};
pub use workspace_server::{register_workspace, WORKSPACE_METHODS};
