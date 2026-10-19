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
pub mod control_plane_client;
pub mod envelope;
pub mod error;
pub mod execution_client;
pub mod execution_handler;
pub mod execution_server;
pub mod factory_client;
pub mod frame;
pub mod harness_client;
pub mod harness_server;
pub mod ledger_client;
pub mod ledger_server;
pub mod plugin_client;
pub mod plugin_server;
pub mod server;
pub mod taskqueue_client;
pub mod taskqueue_server;
pub mod web_http;
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

/// Drive `future` to completion on `runtime`, tolerating a caller that is
/// already running inside a Tokio runtime.
///
/// The Connect client backends own a dedicated runtime and block on it per call
/// (the framed client is synchronous). An implementation server that both
/// *serves* over Connect and talks to a peer over Connect — the kernel server
/// hosts its `LedgerClient` this way (IPCX.7) — invokes that blocking call from
/// inside its own async handler. A raw [`Runtime::block_on`] would panic
/// ("Cannot start a runtime from within a runtime"); [`tokio::task::block_in_place`]
/// steps the current worker aside so the peer runtime's reactor can progress.
pub(crate) fn block_on_nested<F>(runtime: &tokio::runtime::Runtime, future: F) -> F::Output
where
    F: std::future::Future,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| runtime.block_on(future))
    } else {
        runtime.block_on(future)
    }
}

pub use client::Client;
pub use command::CommandSpec;
pub use connect::{serve_wire, ConnectClient, IpcDispatcher, Wire, PARENT_DEATH_STDIN_ENV, WIRE_ENV};
pub use control_plane_client::{
    check_component_id, ControlPlaneReadClient, CONNECT_TIMEOUT, CONTROL_PLANE_READ_API,
};
pub use factory_client::{
    capability_bindings_from_env, parse_capability_bindings, register_factory_routes,
    CapabilityBinding, CapabilityBindings, FactoryRuntimeClient, SharedFactoryClient,
    BINDING_GRANT_HEX_CHARS, BINDING_SOCKET_MAX_BYTES, PLUGIN_CAPABILITY_BINDINGS_ENV,
};
pub use envelope::{Request, Response, ResponseError, PROTOCOL_VERSION};
pub use error::{IpcError, CODE_PROTOCOL, CODE_UNAVAILABLE};
pub use execution_client::{
    BeginStepCall, CompleteStepCall, DirectiveExecCall, ExecIdCall, ExecutionClient, ExecutionOps,
    KernelCall, LeaseExecCall, NowCall, PendingStepCall, ReceiptCall, ReclaimCall,
    SnapshotPathCall, StartCall, TimerExecParams,
};
pub use execution_handler::{
    EffectContext, EffectDispatch, EffectMode, EffectProbe, EffectReport, HandlerError, HandlerKey,
    StepBegin,
};
pub use execution_server::{register_execution, serve_execution, EXECUTION_METHODS};
pub use frame::{FrameError, MAX_FRAME};
pub use harness_client::{AttemptCall, HarnessClient, OperationCall, RunTurnCall, SessionCall};
pub use harness_server::{
    ensure_all_handlers, register_harness, serve_harness, server_info_from, HARNESS_METHODS,
};
pub use ledger_client::{
    BindingParams, CommitParams, DecisionParams, DigestParams, DirectiveIdCall, DirectiveParams,
    DueTimersParams, ExecutionCall, KeyCall, LatchParams, LeaseCall, LedgerClient, LedgerOps,
    MetaParams, OperationIdCall, OutcomeParams, PathParams, TimerIdCall, TimerParams,
    SNAPSHOT_EXPORT_FILE, SNAPSHOT_IMPORT_FILE,
};
pub use ledger_server::{register_ledger, serve_ledger, LEDGER_METHODS};
pub use plugin_client::{HubCallbackClient, PluginClient, PluginIdentity};
pub use plugin_server::{
    is_hub_method, is_spoke_method, plugin_server_info, register_hub, register_plugin, HubHandler,
    PluginHandler, HUB_METHODS, METHOD_CANCEL, METHOD_DESCRIBE, METHOD_HEALTH, METHOD_INVOKE,
    METHOD_INVOKE_CAPABILITY, METHOD_PUBLISH_REPORT, METHOD_RECONCILE, METHOD_RECONCILE_CAPABILITY,
    METHOD_SNAPSHOT, PLUGIN_API, PLUGIN_PROTOCOL_MAJOR, SPOKE_METHODS,
};
pub use server::{socket_from_args, Handler, Server, ServerInfo};
pub use taskqueue_client::{
    CloseParams, CommentParams, IssueProjection, LabelParams, StatusParams, TaskCall,
    TaskQueueClient, TaskQueueOps,
};
pub use taskqueue_server::{register_taskqueue, TASKQUEUE_METHODS};
pub use web_http::{
    decode_query_value, Asset, AssetInventory, ControlAuthority, ListenerConfig, LoopbackListener,
    OperatorSession, RevocableControl, Router, WebApp, WebAppBuilder, WebHandler, WebHttpError,
    WebRequest, WebResponse, CSRF_HEADER, DEFAULT_HOST, MAX_OUTSTANDING_READS, OPERATOR_COOKIE,
    POST_BODY_MAX_BYTES, READ_DEADLINE,
};
pub use workspace_client::{
    CheckoutCall, IntegrateOriginParams, IntegrateParams, OriginReceiptParams, PrepareParams,
    PrepareRepoParams, PrepareVerifyParams, RevertOriginParams, RevertParams, TagParams,
    WorkspaceClient, WorkspaceOps,
};
pub use workspace_server::{register_workspace, WORKSPACE_METHODS};
