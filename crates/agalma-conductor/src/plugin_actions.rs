//! HP.7 authorized plugin actions with durable intents, receipts and recovery
//! (`agalma-jmr.7`).
//!
//! The hub's common action service. Advertised actions (`ActionDescriptor`) are
//! **inert data** until the hub validates a request; a report can never schedule
//! or execute an action. The service authenticates a per-launch grant, validates
//! the advertised descriptor, capability, binding generation, deadline and input
//! digest, applies SF stop admission and human-confirmation policy, then journals
//! the immutable [`ActionIntent`] **before** invoking the pinned generation. The
//! executor runs without any service lock held. A terminal outcome is recorded as
//! a durable receipt; an ambiguous (lost acknowledgement) outcome stays `Unknown`
//! and is reconciled on its **pinned** generation, never a replacement.
//!
//! Durability follows the frozen component-storage rules
//! (`docs/self-mutation-contracts.md` §"HP: durable component storage and
//! reports"). The ledger treats `operations`, `receipts` and event ids as
//! **immutable per identity** (identical content is a no-op, changed content is
//! [`FailureCode::IdentityConflict`]), so the module stores:
//!
//! * the immutable intent exactly once as a [`ComponentOperation`] row whose
//!   `operation_id` is `"{generation}:{operation_id}"` (generation-qualified so a
//!   reused caller operation id in a later generation cannot collide);
//! * every state transition as a [`ComponentEvent`] with a unique `event_id`
//!   (`"{generation}:{operation_id}:{suffix}"`); the current state is derived from
//!   the latest event for the operation;
//! * terminal outcomes as [`ComponentReceipt`] rows (the durable idempotency
//!   anchor) whose payload is also carried by the terminal event so the exact
//!   outcome is reconstructible from readable events.
//!
//! Every commit takes the namespace's current `expected_revision` and a freshly
//! recomputed `input_digest` ([`ComponentCommit::computed_input_digest`]).
//! Namespace is [`action_namespace`] = `"plugin-action/{plugin_id}"`.
//!
//! The production durable backing is the selected ledger via [`ActionStore`]
//! (`Mutex<SqliteLedger>` / `Mutex<LedgerHandle>`). [`InMemoryActionStore`] is a
//! clearly-marked test double, and [`ActionExecutor`] is the pinned-plugin
//! invoker seam (the production RPC wiring is deferred until the HP.6
//! `watch.rs`/UI handoff; see `output.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use agalma_contracts::digest::{canonical_digest, FailureCode};
use agalma_contracts::ledger::OperationState;
use agalma_contracts::{
    sha256_digest, ActionDescriptor, ActionIntent, ActionScope, Actor, BindingPin, ComponentCommit,
    ComponentCommitAnchor, ComponentEvent, ComponentEventRow, ComponentOperation,
    ComponentOperationRow, ComponentOperationState, ComponentReceipt, ComponentStateRow,
    ContractError, LedgerApi, PluginCancelRequest, PluginOperationStatus, ACTION_INTENT_FORMAT,
    COMPONENT_COMMIT_FORMAT, PLUGIN_CANCEL_REQUEST_FORMAT,
};
use agalma_ipc::plugin_server::{METHOD_CANCEL, METHOD_INVOKE, METHOD_INVOKE_CAPABILITY, METHOD_RECONCILE};
use agalma_ledger::SqliteLedger;

use crate::plugin_router::{hex_decode_32, GrantScope, GrantTable, LaunchGrant, StopAdmission};
use crate::stop::StopDecision;

/// Prefix of the per-plugin durable action namespace.
pub const ACTION_NAMESPACE_PREFIX: &str = "plugin-action/";
/// Event kinds (the trailing suffix of an event id).
const KIND_ACCEPTED: &str = "accepted";
const KIND_RUNNING: &str = "running";
const KIND_COMPLETED: &str = "completed";
const KIND_FAILED: &str = "failed";
const KIND_UNKNOWN: &str = "unknown";
const KIND_CANCEL_REPORTED: &str = "cancel_reported";
const KIND_CANCELLED: &str = "cancelled";
/// Cancel-request event ids are `"{generation}:{op}:cancel_requested:{cancel_id}"`.
const CANCEL_REQUESTED_PREFIX: &str = "cancel_requested:";
/// Event page size for rehydration.
const EVENT_PAGE: u32 = 500;

/// The component namespace for one plugin's durable action journal.
pub fn action_namespace(plugin_id: &str) -> String {
    format!("{ACTION_NAMESPACE_PREFIX}{plugin_id}")
}

/// The generation-qualified stored operation id.
fn stored_operation_id(generation: u64, operation_id: &str) -> String {
    format!("{generation}:{operation_id}")
}

/// Split a stored operation id into `(generation, caller operation id)`.
fn parse_stored_operation_id(stored: &str) -> Option<(u64, String)> {
    let (generation, operation_id) = stored.split_once(':')?;
    Some((generation.parse().ok()?, operation_id.to_string()))
}

/// A unique event id for one transition of one operation.
fn event_id(generation: u64, operation_id: &str, suffix: &str) -> String {
    format!("{generation}:{operation_id}:{suffix}")
}

/// Whether `event.event_id` belongs to `(generation, operation_id)`.
///
/// Uses exact known suffixes so an operation id that is a prefix of another
/// (`"a"` vs `"a:b"`) cannot steal the other's events.
fn event_belongs(event_id: &str, generation: u64, operation_id: &str) -> bool {
    let prefix = format!("{generation}:{operation_id}:");
    let Some(rest) = event_id.strip_prefix(&prefix) else {
        return false;
    };
    matches!(
        rest,
        KIND_ACCEPTED
            | KIND_RUNNING
            | KIND_COMPLETED
            | KIND_FAILED
            | KIND_UNKNOWN
            | KIND_CANCEL_REPORTED
            | KIND_CANCELLED
    ) || rest.starts_with(CANCEL_REQUESTED_PREFIX)
}

/// One advertised action and the pinned binding that advertises it.
#[derive(Clone, Debug, PartialEq)]
pub struct AdvertisedAction {
    pub descriptor: ActionDescriptor,
    pub binding: BindingPin,
}

/// The hub's catalog of advertised actions (from the registry/manifest).
///
/// An action id absent for `(plugin_id, generation)` is not advertised by that
/// pinned generation and is refused; the catalog never falls back to another
/// generation.
pub trait ActionCatalog: Send {
    /// The advertised action for `(plugin_id, generation, action_id)`, if any.
    fn action(&self, plugin_id: &str, generation: u64, action_id: &str) -> Option<AdvertisedAction>;
}

/// A `BTreeMap` keyed by `(plugin_id, generation, action_id)`.
impl ActionCatalog for BTreeMap<(String, u64, String), AdvertisedAction> {
    fn action(&self, plugin_id: &str, generation: u64, action_id: &str) -> Option<AdvertisedAction> {
        self.get(&(plugin_id.to_string(), generation, action_id.to_string()))
            .cloned()
    }
}

/// A `Vec` fixture matched by advertised binding identity + action id.
impl ActionCatalog for Vec<AdvertisedAction> {
    fn action(&self, plugin_id: &str, generation: u64, action_id: &str) -> Option<AdvertisedAction> {
        self.iter()
            .find(|a| {
                a.binding.binding_id == plugin_id
                    && a.binding.generation == generation
                    && a.descriptor.action_id == action_id
            })
            .cloned()
    }
}

/// Build a catalog key.
pub fn catalog_key(plugin_id: &str, generation: u64, action_id: &str) -> (String, u64, String) {
    (plugin_id.to_string(), generation, action_id.to_string())
}

/// Send-safe durable backing for the action journal (`component-store/v1`).
///
/// Mirrors [`crate::plugin_reports::ReportStore`] and adds the single-operation
/// read used for effect dedup. The production implementation wraps the selected
/// ledger; tests reuse it, so the exercised path is the production one.
pub trait ActionStore: Send + Sync {
    /// Atomically commit a component-namespace transaction.
    fn component_commit(
        &self,
        commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError>;
    /// Read one component namespace's durable state row.
    fn component_read(&self, namespace: &str) -> Result<Option<ComponentStateRow>, ContractError>;
    /// Component events with sequence `> after`, up to `limit`.
    fn component_events(
        &self,
        namespace: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError>;
    /// One component operation row by namespace and operation id, if present.
    fn component_operation(
        &self,
        namespace: &str,
        operation_id: &str,
    ) -> Result<Option<ComponentOperationRow>, ContractError>;
}

impl ActionStore for std::sync::Mutex<SqliteLedger> {
    fn component_commit(
        &self,
        commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_commit(commit)
    }

    fn component_read(&self, namespace: &str) -> Result<Option<ComponentStateRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_read(namespace)
    }

    fn component_events(
        &self,
        namespace: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_events(namespace, after, limit)
    }

    fn component_operation(
        &self,
        namespace: &str,
        operation_id: &str,
    ) -> Result<Option<ComponentOperationRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_operation(namespace, operation_id)
    }
}

impl ActionStore for std::sync::Mutex<crate::ledger_process::LedgerHandle> {
    fn component_commit(
        &self,
        commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_commit(commit)
    }

    fn component_read(&self, namespace: &str) -> Result<Option<ComponentStateRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_read(namespace)
    }

    fn component_events(
        &self,
        namespace: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_events(namespace, after, limit)
    }

    fn component_operation(
        &self,
        namespace: &str,
        operation_id: &str,
    ) -> Result<Option<ComponentOperationRow>, ContractError> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .component_operation(namespace, operation_id)
    }
}

/// One durable namespace's in-memory mirror (test double only).
#[derive(Default, Clone)]
struct MemNamespace {
    revision: u64,
    event_seq: u64,
    checkpoint: Option<Value>,
    events: Vec<ComponentEventRow>,
    operations: BTreeMap<String, ComponentOperation>,
    receipts: BTreeMap<String, ComponentReceipt>,
    anchors: BTreeMap<String, ComponentCommitAnchor>,
}

/// **Test double.** An in-memory [`ActionStore`] with the same immutability
/// semantics as the real ledger, for co-located unit tests only. It is not
/// durable and must never be presented as production storage.
#[derive(Default)]
pub struct InMemoryActionStore {
    namespaces: std::sync::Mutex<BTreeMap<String, MemNamespace>>,
}

impl InMemoryActionStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl ActionStore for InMemoryActionStore {
    fn component_commit(
        &self,
        commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError> {
        commit.version_gate()?;
        commit.check_bounds()?;
        commit.verify_input_digest()?;
        let mut all = self.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        let ns = all.entry(commit.namespace.clone()).or_default();
        if let Some(existing) = ns.anchors.get(&commit.commit_id) {
            if existing.input_digest == commit.input_digest {
                return Ok(existing.clone());
            }
            return Err(FailureCode::IdentityConflict.failure(
                &commit.commit_id,
                "component commit reused with changed input digest",
            ));
        }
        if commit.expected_revision != ns.revision {
            return Err(FailureCode::RevisionConflict.failure(
                &commit.commit_id,
                format!(
                    "expected revision {} but namespace is at {}",
                    commit.expected_revision, ns.revision
                ),
            ));
        }
        for event in &commit.events {
            if let Some(existing) = ns.events.iter().find(|e| e.event.event_id == event.event_id) {
                if existing.event == *event {
                    continue;
                }
                return Err(FailureCode::IdentityConflict.failure(
                    &event.event_id,
                    "component event reused with changed content",
                ));
            }
            ns.event_seq += 1;
            ns.events.push(ComponentEventRow {
                namespace: commit.namespace.clone(),
                sequence: ns.event_seq,
                event: event.clone(),
            });
        }
        for operation in &commit.operations {
            if let Some(existing) = ns.operations.get(&operation.operation_id) {
                if existing == operation {
                    continue;
                }
                return Err(FailureCode::IdentityConflict.failure(
                    &operation.operation_id,
                    "component operation reused with changed content",
                ));
            }
            ns.operations
                .insert(operation.operation_id.clone(), operation.clone());
        }
        for receipt in &commit.receipts {
            if let Some(existing) = ns.receipts.get(&receipt.operation_id) {
                if existing == receipt {
                    continue;
                }
                return Err(FailureCode::IdentityConflict.failure(
                    &receipt.operation_id,
                    "component receipt reused with changed content",
                ));
            }
            ns.receipts
                .insert(receipt.operation_id.clone(), receipt.clone());
        }
        if let Some(checkpoint) = &commit.checkpoint {
            ns.checkpoint = Some(checkpoint.clone());
        }
        ns.revision += 1;
        let anchor = ComponentCommitAnchor {
            namespace: commit.namespace.clone(),
            commit_id: commit.commit_id.clone(),
            input_digest: commit.input_digest.clone(),
            revision: ns.revision,
            committed_at_unix_ms: 0,
        };
        ns.anchors.insert(commit.commit_id.clone(), anchor.clone());
        Ok(anchor)
    }

    fn component_read(&self, namespace: &str) -> Result<Option<ComponentStateRow>, ContractError> {
        let all = self.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        Ok(all.get(namespace).map(|ns| ComponentStateRow {
            namespace: namespace.to_string(),
            revision: ns.revision,
            watermark: ns.event_seq,
            checkpoint: ns.checkpoint.clone(),
            recorded_at_unix_ms: 0,
        }))
    }

    fn component_events(
        &self,
        namespace: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError> {
        let all = self.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ns) = all.get(namespace) else {
            return Ok(Vec::new());
        };
        Ok(ns
            .events
            .iter()
            .filter(|row| row.sequence > after)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    fn component_operation(
        &self,
        namespace: &str,
        operation_id: &str,
    ) -> Result<Option<ComponentOperationRow>, ContractError> {
        let all = self.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        Ok(all.get(namespace).and_then(|ns| {
            ns.operations
                .get(operation_id)
                .map(|operation| ComponentOperationRow {
                    namespace: namespace.to_string(),
                    operation: operation.clone(),
                })
        }))
    }
}

/// The outcome of invoking the pinned plugin's effect.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionOutcome {
    /// The effect completed with a durable value.
    Completed(Value),
    /// The plugin durably accepted the operation; the effect is not yet
    /// confirmed (the frozen protocol journals + acknowledges, then reconciles).
    Accepted,
    /// The plugin refused before any effect.
    Refused(ContractError),
    /// The acknowledgement was lost; the effect may or may not have happened.
    Lost,
}

/// The outcome of reconciling/cancelling an already-intent operation.
#[derive(Clone, Debug, PartialEq)]
pub enum ReconcileOutcome {
    /// Confirmed completed with a durable value.
    Completed(Value),
    /// Confirmed failed with a message.
    Failed(String),
    /// A cancellation is requested; settlement is still pending.
    CancelRequested,
    /// Cancellation settled (terminal).
    Cancelled,
    /// Still ambiguous; stays parked and must be reconciled again, never
    /// redispatched as a new effect.
    Unknown,
}

/// The pinned-plugin invoker seam.
///
/// `reconcile` and `cancel` MUST resolve the plugin from
/// `intent.binding.generation` (the pinned generation), never a currently-active
/// replacement. Tests inject a fake with an independent durable effect journal.
pub trait ActionExecutor: Send {
    /// Perform the action for a fresh, journaled intent.
    fn invoke(&mut self, intent: &ActionIntent) -> ExecutionOutcome;
    /// Reconcile an interrupted/ambiguous operation on its pinned generation.
    fn reconcile(&mut self, intent: &ActionIntent) -> ReconcileOutcome;
    /// Request cancellation; acknowledgement alone is not settlement evidence.
    fn cancel(&mut self, intent: &ActionIntent, cancel: &PluginCancelRequest) -> ReconcileOutcome;
}

/// A child-RPC failure, carrying whether the request could have been delivered.
struct CallError {
    /// The transport/contract error.
    error: ContractError,
    /// `true` once the request may have reached the child (unconfirmed outcome);
    /// `false` for a connect/handshake failure (no effect can have happened).
    delivered: bool,
}

/// The production pinned-plugin RPC [`ActionExecutor`] (HP.7 host attachment).
///
/// Resolves each plugin's live child Connect socket from the host's shared launch
/// map **keyed by `(plugin_id, binding generation)`**, so an intent pinned to an
/// older generation never contacts a replacement generation's socket (the host
/// records only the currently-launched generation). Performs
/// `plugin.Invoke`/`plugin.Reconcile`/`plugin.Cancel` over
/// [`agalma_ipc::PluginClient`] and verifies every returned
/// [`PluginOperationStatus`] against the requested identity before using it. It
/// performs no effect bookkeeping itself: the [`ActionService`] owns the durable
/// intent/receipt journal. A child that cannot be reached is a known refusal (no
/// effect); a failure after the request was sent is unconfirmed
/// (`Lost`/`Unknown`), never a fabricated success.
pub struct PluginRpcActionExecutor {
    sockets: Arc<Mutex<BTreeMap<(String, u64), PathBuf>>>,
    runtime: tokio::runtime::Runtime,
}

impl PluginRpcActionExecutor {
    /// Build the executor over the host's live `(plugin_id, generation)` socket
    /// map.
    pub fn new(
        sockets: Arc<Mutex<BTreeMap<(String, u64), PathBuf>>>,
    ) -> Result<Self, ContractError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                ContractError::KnownFailure(format!("action executor runtime: {err}"))
            })?;
        Ok(PluginRpcActionExecutor { sockets, runtime })
    }

    /// The live child socket for the **pinned** `(plugin_id, generation)`.
    ///
    /// A pinned generation with no live launch (replaced, retired or never
    /// started) is [`FailureCode::StateUnavailable`]: the executor refuses rather
    /// than contacting a different generation.
    pub fn socket(&self, plugin_id: &str, generation: u64) -> Result<PathBuf, ContractError> {
        self.sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(plugin_id.to_string(), generation))
            .cloned()
            .ok_or_else(|| {
                FailureCode::StateUnavailable.failure(
                    plugin_id,
                    format!(
                        "no live child socket for the pinned generation {generation}; \
                         a replacement generation is never contacted"
                    ),
                )
            })
    }

    /// Connect, handshake and perform one spoke method for the pinned generation.
    /// A connect/handshake failure is `delivered == false`; a failure after the
    /// handshake is `delivered == true` (unconfirmed).
    fn call(
        &self,
        plugin_id: &str,
        generation: u64,
        method: &'static str,
        request: Value,
    ) -> Result<Value, CallError> {
        let socket = match self.socket(plugin_id, generation) {
            Ok(socket) => socket,
            Err(error) => return Err(CallError { error, delivered: false }),
        };
        let plugin = plugin_id.to_string();
        let fut = async move {
            let mut client = match agalma_ipc::PluginClient::connect(&socket).await {
                Ok(client) => client,
                Err(err) => {
                    return Err(CallError {
                        error: FailureCode::StateUnavailable.failure(
                            &plugin,
                            format!("plugin RPC connect failed: {err}"),
                        ),
                        delivered: false,
                    })
                }
            };
            if let Err(err) = client.handshake(&plugin).await {
                return Err(CallError {
                    error: FailureCode::StateUnavailable.failure(
                        &plugin,
                        format!("plugin handshake refused: {err}"),
                    ),
                    delivered: false,
                });
            }
            let result = match method {
                METHOD_INVOKE => client.invoke(request).await,
                METHOD_RECONCILE => client.reconcile(request).await,
                METHOD_CANCEL => client.cancel(request).await,
                _ => unreachable!("only spoke action methods are dispatched"),
            };
            result.map_err(|err| CallError {
                error: FailureCode::StateUnavailable.failure(
                    &plugin,
                    format!("plugin {method} failed: {err}"),
                ),
                delivered: true,
            })
        };
        let runtime = &self.runtime;
        let run = |fut| {
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::task::block_in_place(|| runtime.block_on(fut))
            } else {
                runtime.block_on(fut)
            }
        };
        run(fut)
    }

    fn status(&self, value: Value) -> Result<PluginOperationStatus, ContractError> {
        let status: PluginOperationStatus = serde_json::from_value(value).map_err(|err| {
            ContractError::KnownFailure(format!("plugin operation status invalid: {err}"))
        })?;
        status.version_gate()?;
        Ok(status)
    }

    fn verified_status(
        &self,
        intent: &ActionIntent,
        value: Value,
    ) -> Result<PluginOperationStatus, ContractError> {
        let status = self.status(value)?;
        verify_plugin_status(intent, &status)?;
        Ok(status)
    }

    fn terminal_value(status: &PluginOperationStatus) -> Value {
        status
            .receipt
            .as_ref()
            .map(|receipt| receipt.outcome.clone())
            .unwrap_or(Value::Null)
    }
}

impl ActionExecutor for PluginRpcActionExecutor {
    fn invoke(&mut self, intent: &ActionIntent) -> ExecutionOutcome {
        let request = match serde_json::to_value(intent) {
            Ok(request) => request,
            Err(err) => {
                return ExecutionOutcome::Refused(ContractError::KnownFailure(format!(
                    "action intent json: {err}"
                )))
            }
        };
        match self.call(
            &intent.plugin_id,
            intent.binding.generation,
            METHOD_INVOKE,
            request,
        ) {
            Ok(value) => match self.verified_status(intent, value) {
                Ok(status) => match status.state {
                    OperationState::Completed => {
                        ExecutionOutcome::Completed(Self::terminal_value(&status))
                    }
                    OperationState::Failed | OperationState::Cancelled => {
                        ExecutionOutcome::Refused(FailureCode::InvalidRecord.failure(
                            &status.operation_id,
                            format!("plugin reported the action {:?}", status.state),
                        ))
                    }
                    // Journaled + accepted; the effect settles via reconcile.
                    OperationState::Accepted
                    | OperationState::Running
                    | OperationState::CancelRequested
                    | OperationState::Unknown => ExecutionOutcome::Accepted,
                },
                Err(err) => ExecutionOutcome::Refused(err),
            },
            // No child reachable: the request was never delivered (no effect).
            Err(CallError {
                error,
                delivered: false,
            }) => ExecutionOutcome::Refused(error),
            // Sent but unconfirmed: never a fabricated success.
            Err(_) => ExecutionOutcome::Lost,
        }
    }

    fn reconcile(&mut self, intent: &ActionIntent) -> ReconcileOutcome {
        let request = match serde_json::to_value(intent) {
            Ok(request) => request,
            Err(_) => return ReconcileOutcome::Unknown,
        };
        match self.call(
            &intent.plugin_id,
            intent.binding.generation,
            METHOD_RECONCILE,
            request,
        ) {
            Ok(value) => match self.verified_status(intent, value) {
                Ok(status) => match status.state {
                    OperationState::Completed => {
                        ReconcileOutcome::Completed(Self::terminal_value(&status))
                    }
                    OperationState::Failed => ReconcileOutcome::Failed(format!(
                        "plugin reported the action {:?}",
                        status.state
                    )),
                    OperationState::CancelRequested => ReconcileOutcome::CancelRequested,
                    OperationState::Cancelled => ReconcileOutcome::Cancelled,
                    OperationState::Accepted | OperationState::Running | OperationState::Unknown => {
                        ReconcileOutcome::Unknown
                    }
                },
                Err(_) => ReconcileOutcome::Unknown,
            },
            Err(_) => ReconcileOutcome::Unknown,
        }
    }

    fn cancel(
        &mut self,
        intent: &ActionIntent,
        cancel: &PluginCancelRequest,
    ) -> ReconcileOutcome {
        let request = match serde_json::to_value(cancel) {
            Ok(request) => request,
            Err(_) => return ReconcileOutcome::CancelRequested,
        };
        match self.call(
            &intent.plugin_id,
            intent.binding.generation,
            METHOD_CANCEL,
            request,
        ) {
            Ok(value) => match self.verified_status(intent, value) {
                Ok(status) => match status.state {
                    OperationState::Cancelled => ReconcileOutcome::Cancelled,
                    OperationState::Completed => {
                        ReconcileOutcome::Completed(Self::terminal_value(&status))
                    }
                    OperationState::Failed => {
                        ReconcileOutcome::Failed("plugin reported a failed action".to_string())
                    }
                    // Acknowledgement alone is not settlement evidence.
                    OperationState::Accepted
                    | OperationState::Running
                    | OperationState::CancelRequested
                    | OperationState::Unknown => ReconcileOutcome::CancelRequested,
                },
                Err(_) => ReconcileOutcome::CancelRequested,
            },
            // The cancel request was journaled locally; the ack is unconfirmed.
            Err(_) => ReconcileOutcome::CancelRequested,
        }
    }
}

/// Verify a plugin's returned `PluginOperationStatus/v1` against the requested
/// identity before its state/outcome is used.
///
/// The full pinned binding (plugin, generation, config digest, implementation),
/// operation id, input digest and — when terminal — the durable receipt's
/// identity must match the journaled intent exactly. Any mismatch is a
/// fail-closed refusal: a reply for a different plugin/generation/operation can
/// never complete the wrong durable intent.
pub fn verify_plugin_status(
    intent: &ActionIntent,
    status: &PluginOperationStatus,
) -> Result<(), ContractError> {
    let mismatch = |detail: String| FailureCode::IdentityConflict.failure(&intent.operation_id, detail);
    if status.plugin_id != intent.plugin_id {
        return Err(mismatch(format!(
            "plugin reply identity `{}` does not match `{}`",
            status.plugin_id, intent.plugin_id
        )));
    }
    if status.binding != intent.binding {
        return Err(mismatch(
            "plugin reply binding/generation does not match the pinned intent".to_string(),
        ));
    }
    if status.operation_id != intent.operation_id {
        return Err(mismatch(format!(
            "plugin reply operation `{}` does not match `{}`",
            status.operation_id, intent.operation_id
        )));
    }
    if status.input_digest != intent.input_digest {
        return Err(mismatch(
            "plugin reply input digest does not match the pinned intent".to_string(),
        ));
    }
    if let Some(receipt) = &status.receipt {
        let stored = stored_operation_id(intent.binding.generation, &intent.operation_id);
        if receipt.operation_id != stored {
            return Err(mismatch(
                "plugin receipt operation identity does not match the pinned intent".to_string(),
            ));
        }
        if receipt.input_digest != intent.input_digest {
            return Err(mismatch(
                "plugin receipt input digest does not match the pinned intent".to_string(),
            ));
        }
        if receipt.binding != intent.binding {
            return Err(mismatch(
                "plugin receipt binding/generation does not match the pinned intent".to_string(),
            ));
        }
    }
    Ok(())
}

/// A validated action request (UI/policy → hub).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    /// Presented per-launch grant token (lowercase hex).
    pub grant: String,
    /// Caller's declared plugin id (checked against the grant).
    pub plugin_id: String,
    /// Caller's declared pinned binding generation (checked against the grant).
    pub generation: u64,
    /// The action to invoke.
    pub action_id: String,
    /// Durable operation identity; effect dedup anchor.
    pub operation_id: String,
    /// Canonical typed input.
    pub input: Value,
    /// Deadline (Unix milliseconds); at/after this a new effect is refused.
    pub deadline_unix_ms: u64,
    /// Who requested the action.
    pub actor: Actor,
    /// Human confirmation phrase, when the descriptor requires one.
    pub confirmation: Option<String>,
    /// The authorizing scope.
    pub scope: ActionScope,
}

/// A cancellation request for one journaled operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionCancelRequest {
    /// Presented per-launch grant token (lowercase hex).
    pub grant: String,
    /// Caller's declared plugin id (checked against the grant).
    pub plugin_id: String,
    /// Caller's declared pinned binding generation (checked against the grant).
    pub generation: u64,
    /// The operation to cancel.
    pub operation_id: String,
    /// Stable cancel idempotency anchor.
    pub cancel_request_id: String,
    /// Cancel deadline (Unix milliseconds).
    pub deadline_unix_ms: u64,
    /// Who requested the cancel.
    pub actor: Actor,
    /// Human confirmation phrase, when the descriptor requires one.
    pub confirmation: Option<String>,
    /// The authorizing scope.
    pub scope: ActionScope,
}

/// The durable record of one journaled action.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionRecord {
    /// The pinned binding generation the operation is journaled for.
    pub generation: u64,
    /// Deadline recorded at journal time.
    pub deadline_unix_ms: u64,
    /// The immutable journaled intent.
    pub intent: ActionIntent,
    /// The derived current state.
    pub state: ComponentOperationState,
    /// The durable receipt when terminal.
    pub receipt: Option<ComponentReceipt>,
}

/// The resolved outcome of a validated action request.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionOutcome {
    /// The pinned intent this outcome belongs to.
    pub intent: ActionIntent,
    /// The current durable state.
    pub state: ComponentOperationState,
    /// The durable receipt when terminal.
    pub receipt: Option<ComponentReceipt>,
}

impl ActionOutcome {
    /// Whether the state is terminal (completed, failed or cancelled).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            ComponentOperationState::Completed
                | ComponentOperationState::Failed
                | ComponentOperationState::Cancelled
        )
    }

    /// Whether the outcome is unresolved (unknown) and must be reconciled.
    pub fn is_unknown(&self) -> bool {
        matches!(self.state, ComponentOperationState::Unknown)
    }

    fn from_record(record: ActionRecord) -> Self {
        ActionOutcome {
            intent: record.intent,
            state: record.state,
            receipt: record.receipt,
        }
    }
}

/// The durable action-namespace checkpoint (the known operation ids).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct ActionCheckpoint {
    operations: BTreeSet<String>,
}

/// The hub's common action service.
pub struct ActionService {
    catalog: Box<dyn ActionCatalog>,
    grants: GrantTable,
    journal: Box<dyn ActionStore>,
    executor: Box<dyn ActionExecutor>,
    stop: Arc<dyn StopAdmission>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    plugins: BTreeSet<String>,
}

impl ActionService {
    /// Build the service with injected catalog, durable journal, executor and
    /// stop admission.
    pub fn new(
        catalog: Box<dyn ActionCatalog>,
        journal: Box<dyn ActionStore>,
        executor: Box<dyn ActionExecutor>,
        stop: Arc<dyn StopAdmission>,
    ) -> Self {
        ActionService {
            catalog,
            grants: GrantTable::new(),
            journal,
            executor,
            stop,
            now: Arc::new(system_now_ms),
            plugins: BTreeSet::new(),
        }
    }

    /// Override the clock (deterministic tests).
    pub fn with_clock(mut self, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now = now;
        self
    }

    /// Install a per-launch grant; returns its stored fingerprint. Also records
    /// the plugin id as a known action client (used by [`Self::recover`] after a
    /// restart, once the hub re-installs its grants).
    pub fn install_grant(&mut self, grant: &LaunchGrant) -> String {
        self.plugins.insert(grant.scope().plugin_id.clone());
        self.grants.insert(grant)
    }

    /// Revoke a grant by fingerprint (returns whether one was removed).
    pub fn revoke_grant(&mut self, fingerprint: &str) -> bool {
        self.grants.revoke(fingerprint)
    }

    /// Register a plugin id as an action client (for recovery before grants are
    /// re-installed).
    pub fn observe_plugin(&mut self, plugin_id: &str) {
        self.plugins.insert(plugin_id.to_string());
    }

    /// The durable record of one journaled operation, if present.
    pub fn record(
        &self,
        plugin_id: &str,
        generation: u64,
        operation_id: &str,
    ) -> Result<Option<ActionRecord>, ContractError> {
        self.read_record(plugin_id, generation, operation_id)
    }

    // --- authentication / validation helpers -------------------------------

    fn authenticate(
        &self,
        grant_hex: &str,
        now: u64,
    ) -> Result<(GrantScope, [u8; 32]), ContractError> {
        let token = hex_decode_32(grant_hex)?;
        let scope = self.grants.authenticate(&token, now)?.clone();
        if scope.method != METHOD_INVOKE_CAPABILITY {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant authorizes `{}`, not `{METHOD_INVOKE_CAPABILITY}`",
                    scope.method
                ),
            ));
        }
        Ok((scope, token))
    }

    fn validate_identity(
        &self,
        scope: &GrantScope,
        plugin_id: &str,
        generation: u64,
    ) -> Result<(), ContractError> {
        if scope.plugin_id != plugin_id {
            return Err(FailureCode::AuthorityDenied.failure(
                &scope.plugin_id,
                format!(
                    "grant identity `{}` does not match supplied `{plugin_id}`",
                    scope.plugin_id
                ),
            ));
        }
        if scope.binding.generation != generation {
            return Err(FailureCode::StaleGeneration.failure(
                &scope.plugin_id,
                format!(
                    "grant pins generation {}, request declared {generation}",
                    scope.binding.generation
                ),
            ));
        }
        Ok(())
    }

    fn advertised(
        &self,
        scope: &GrantScope,
        generation: u64,
        action_id: &str,
    ) -> Result<AdvertisedAction, ContractError> {
        let advertised = self
            .catalog
            .action(&scope.plugin_id, generation, action_id)
            .ok_or_else(|| {
                FailureCode::InvalidRecord.failure(
                    action_id,
                    "action is not advertised by this generation",
                )
            })?;
        if advertised.binding.generation != generation {
            return Err(FailureCode::StaleGeneration.failure(
                action_id,
                format!(
                    "advertised binding generation {} is not the requested generation {generation}",
                    advertised.binding.generation
                ),
            ));
        }
        if advertised.descriptor.required_capability != scope.resource_namespace {
            return Err(FailureCode::MissingCapability.failure(
                &scope.plugin_id,
                format!(
                    "action requires `{}`, grant authorizes `{}`",
                    advertised.descriptor.required_capability, scope.resource_namespace
                ),
            ));
        }
        Ok(advertised)
    }

    fn check_human_confirmation(
        &self,
        descriptor: &ActionDescriptor,
        actor: Actor,
        confirmation: Option<&str>,
    ) -> Result<(), ContractError> {
        if let Some(phrase) = descriptor.human_confirmation.as_deref() {
            if actor != Actor::Human {
                return Err(FailureCode::AuthorityDenied.failure(
                    &descriptor.action_id,
                    "a policy actor cannot satisfy a human-confirmation requirement",
                ));
            }
            if confirmation != Some(phrase) {
                return Err(FailureCode::AuthorityDenied.failure(
                    &descriptor.action_id,
                    "missing or incorrect human confirmation phrase",
                ));
            }
        }
        Ok(())
    }

    fn check_stop(&self, scope: &GrantScope) -> Result<(), ContractError> {
        match self.stop.decision() {
            StopDecision::Running => Ok(()),
            StopDecision::Stopped(reason) => {
                Err(FailureCode::Stopped.failure(&scope.plugin_id, reason))
            }
            StopDecision::Unavailable(reason) => {
                Err(FailureCode::StateUnavailable.failure(&scope.plugin_id, reason))
            }
        }
    }

    fn build_intent(
        &self,
        scope: &GrantScope,
        token: &[u8; 32],
        request: &ActionRequest,
    ) -> Result<ActionIntent, ContractError> {
        let intent = ActionIntent {
            format: ACTION_INTENT_FORMAT.to_string(),
            plugin_id: scope.plugin_id.clone(),
            binding: scope.binding.clone(),
            action_id: request.action_id.clone(),
            operation_id: request.operation_id.clone(),
            input: request.input.clone(),
            input_digest: canonical_digest(&request.input)?,
            deadline_unix_ms: request.deadline_unix_ms,
            scope: request.scope.clone(),
            grant_fingerprint: sha256_digest(token),
            actor: request.actor,
        };
        intent.version_gate()?;
        intent.verify_input_digest()?;
        Ok(intent)
    }

    // --- journal read helpers ----------------------------------------------

    fn read_record(
        &self,
        plugin_id: &str,
        generation: u64,
        operation_id: &str,
    ) -> Result<Option<ActionRecord>, ContractError> {
        let namespace = action_namespace(plugin_id);
        let stored = stored_operation_id(generation, operation_id);
        let Some(row) = self.journal.component_operation(&namespace, &stored)? else {
            return Ok(None);
        };
        let intent: ActionIntent = serde_json::from_value(row.operation.intent.clone()).map_err(
            |e| ContractError::KnownFailure(format!("journaled action intent is invalid: {e}")),
        )?;
        let events = self.events_for_operation(&namespace, generation, operation_id)?;
        let (state, receipt) = derive_state(&events)?;
        Ok(Some(ActionRecord {
            generation,
            deadline_unix_ms: row.operation.deadline_unix_ms,
            intent,
            state,
            receipt,
        }))
    }

    fn events_for_operation(
        &self,
        namespace: &str,
        generation: u64,
        operation_id: &str,
    ) -> Result<Vec<ComponentEventRow>, ContractError> {
        let mut out = Vec::new();
        let mut after = 0u64;
        loop {
            let rows = self.journal.component_events(namespace, after, EVENT_PAGE)?;
            if rows.is_empty() {
                break;
            }
            let last = rows.last().map(|r| r.sequence).unwrap_or(after);
            for row in rows {
                if event_belongs(&row.event.event_id, generation, operation_id) {
                    out.push(row);
                }
            }
            if last <= after {
                break;
            }
            after = last;
        }
        Ok(out)
    }

    fn checkpoint_for(&self, namespace: &str) -> Result<ActionCheckpoint, ContractError> {
        let state = self.journal.component_read(namespace)?;
        checkpoint_from(state.as_ref())
    }

    /// The generation an operation is journaled under, if it is journaled at all
    /// (used to distinguish a stale-binding request from an unknown operation).
    fn journaled_generation(
        &self,
        plugin_id: &str,
        operation_id: &str,
    ) -> Result<Option<u64>, ContractError> {
        let namespace = action_namespace(plugin_id);
        let checkpoint = self.checkpoint_for(&namespace)?;
        for stored in &checkpoint.operations {
            if let Some((generation, op)) = parse_stored_operation_id(stored) {
                if op == operation_id {
                    return Ok(Some(generation));
                }
            }
        }
        Ok(None)
    }

    /// The record for an operation, or the correct refusal when it is not
    /// journaled for the requested generation.
    fn record_for_request(
        &self,
        plugin_id: &str,
        generation: u64,
        operation_id: &str,
    ) -> Result<ActionRecord, ContractError> {
        if let Some(record) = self.read_record(plugin_id, generation, operation_id)? {
            return Ok(record);
        }
        if self.journaled_generation(plugin_id, operation_id)?.is_some() {
            return Err(FailureCode::StaleGeneration.failure(
                operation_id,
                "operation is journaled under a different pinned generation",
            ));
        }
        Err(FailureCode::StateUnavailable.failure(
            operation_id,
            "operation is not journaled for this binding",
        ))
    }

    // --- journal write helpers ---------------------------------------------

    fn commit_transition(
        &self,
        plugin_id: &str,
        stored_operation_id: &str,
        event: ComponentEvent,
        operation: Option<ComponentOperation>,
        receipt: Option<ComponentReceipt>,
    ) -> Result<(), ContractError> {
        let namespace = action_namespace(plugin_id);
        let state = self.journal.component_read(&namespace)?;
        let expected_revision = state.as_ref().map(|s| s.revision).unwrap_or(0);
        let mut checkpoint = checkpoint_from(state.as_ref())?;
        checkpoint.operations.insert(stored_operation_id.to_string());
        let commit_id = event.event_id.clone();
        let mut commit = ComponentCommit {
            format: COMPONENT_COMMIT_FORMAT.to_string(),
            namespace,
            commit_id,
            input_digest: String::new(),
            expected_revision,
            checkpoint: Some(serde_json::to_value(&checkpoint).map_err(|e| {
                ContractError::KnownFailure(format!("action checkpoint json: {e}"))
            })?),
            events: vec![event],
            operations: operation.into_iter().collect(),
            receipts: receipt.into_iter().collect(),
        };
        commit.input_digest = commit.computed_input_digest()?;
        self.journal.component_commit(&commit)?;
        Ok(())
    }

    /// Commit the immutable intent row plus the `accepted` event.
    fn journal_intent(&self, intent: &ActionIntent, now: u64) -> Result<(), ContractError> {
        let stored = stored_operation_id(intent.binding.generation, &intent.operation_id);
        let operation = ComponentOperation {
            operation_id: stored.clone(),
            input_digest: intent.input_digest.clone(),
            binding: intent.binding.clone(),
            intent: serde_json::to_value(intent)
                .map_err(|e| ContractError::KnownFailure(format!("action intent json: {e}")))?,
            state: ComponentOperationState::Accepted,
            deadline_unix_ms: intent.deadline_unix_ms,
        };
        let event = ComponentEvent {
            event_id: event_id(intent.binding.generation, &intent.operation_id, KIND_ACCEPTED),
            kind: "action.accepted".to_string(),
            payload: serde_json::to_value(intent)
                .map_err(|e| ContractError::KnownFailure(format!("action intent json: {e}")))?,
            recorded_at_unix_ms: now,
        };
        self.commit_transition(&intent.plugin_id, &stored, event, Some(operation), None)
    }

    /// Commit one non-terminal state event.
    fn append_event(
        &self,
        intent: &ActionIntent,
        suffix: &str,
        kind: &str,
        payload: Value,
        now: u64,
    ) -> Result<(), ContractError> {
        let stored = stored_operation_id(intent.binding.generation, &intent.operation_id);
        let event = ComponentEvent {
            event_id: event_id(intent.binding.generation, &intent.operation_id, suffix),
            kind: kind.to_string(),
            payload,
            recorded_at_unix_ms: now,
        };
        self.commit_transition(&intent.plugin_id, &stored, event, None, None)
    }

    /// Commit a terminal receipt plus its terminal event.
    fn append_terminal(
        &self,
        intent: &ActionIntent,
        suffix: &str,
        kind: &str,
        receipt: &ComponentReceipt,
        now: u64,
    ) -> Result<(), ContractError> {
        let stored = stored_operation_id(intent.binding.generation, &intent.operation_id);
        let event = ComponentEvent {
            event_id: event_id(intent.binding.generation, &intent.operation_id, suffix),
            kind: kind.to_string(),
            payload: serde_json::to_value(receipt)
                .map_err(|e| ContractError::KnownFailure(format!("action receipt json: {e}")))?,
            recorded_at_unix_ms: now,
        };
        self.commit_transition(&intent.plugin_id, &stored, event, None, Some(receipt.clone()))
    }

    fn make_receipt(&self, intent: &ActionIntent, outcome: Value, now: u64) -> ComponentReceipt {
        ComponentReceipt {
            operation_id: stored_operation_id(intent.binding.generation, &intent.operation_id),
            input_digest: intent.input_digest.clone(),
            binding: intent.binding.clone(),
            outcome,
            completed_at_unix_ms: now,
        }
    }

    fn prior_cancel(
        &self,
        namespace: &str,
        generation: u64,
        operation_id: &str,
        cancel_request_id: &str,
    ) -> Result<Option<PluginCancelRequest>, ContractError> {
        let events = self.events_for_operation(namespace, generation, operation_id)?;
        for row in events {
            if row.event.kind != "action.cancel_requested" {
                continue;
            }
            // Exact parse: strip the precise `gen:op:cancel_requested:` prefix and
            // compare the remainder to the request id with full equality. A
            // suffix match would alias distinct ids (e.g. `"a"` vs `"cancel:a"`).
            let prefix = event_id(generation, operation_id, CANCEL_REQUESTED_PREFIX);
            let Some(recorded_id) = row.event.event_id.strip_prefix(&prefix) else {
                continue;
            };
            if recorded_id != cancel_request_id {
                continue;
            }
            let cancel: PluginCancelRequest =
                serde_json::from_value(row.event.payload.clone()).map_err(|e| {
                    ContractError::KnownFailure(format!("journaled cancel request is invalid: {e}"))
                })?;
            return Ok(Some(cancel));
        }
        Ok(None)
    }

    // --- public operations --------------------------------------------------

    /// Validate and execute one action request.
    ///
    /// Any refusal journals nothing and calls no executor (zero effects). A
    /// completed/refused/ambiguous outcome from the pinned executor is recorded
    /// durably. A duplicate delivery of the same operation id + input returns the
    /// recorded outcome without re-invoking the executor.
    pub fn submit(&mut self, request: &ActionRequest) -> Result<ActionOutcome, ContractError> {
        let now = (self.now)();
        let (scope, token) = self.authenticate(&request.grant, now)?;
        self.validate_identity(&scope, &request.plugin_id, request.generation)?;
        let advertised = self.advertised(&scope, request.generation, &request.action_id)?;
        if request.deadline_unix_ms <= now {
            return Err(FailureCode::DeadlineExceeded.failure(
                &scope.plugin_id,
                format!(
                    "deadline {} already passed (now {now})",
                    request.deadline_unix_ms
                ),
            ));
        }
        let intent = self.build_intent(&scope, &token, request)?;
        self.check_human_confirmation(
            &advertised.descriptor,
            request.actor,
            request.confirmation.as_deref(),
        )?;
        self.check_stop(&scope)?;

        // Durable dedup: an existing operation is either a changed-input conflict
        // or an acknowledged replay (never a second effect).
        if let Some(existing) =
            self.read_record(&request.plugin_id, request.generation, &request.operation_id)?
        {
            if existing.intent.input_digest != intent.input_digest {
                return Err(FailureCode::IdentityConflict.failure(
                    &request.operation_id,
                    "action operation_id reused with changed input",
                ));
            }
            return Ok(ActionOutcome::from_record(existing));
        }

        // Fresh: journal the immutable intent BEFORE invoking the executor.
        self.journal_intent(&intent, now)?;
        let outcome = self.executor.invoke(&intent);
        match outcome {
            ExecutionOutcome::Completed(value) => {
                let receipt = self.make_receipt(&intent, value, now);
                self.append_terminal(&intent, KIND_COMPLETED, "action.completed", &receipt, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Completed,
                    receipt: Some(receipt),
                })
            }
            ExecutionOutcome::Accepted => {
                // The plugin durably accepted the operation; the effect settles
                // via reconcile on the pinned generation.
                let payload = serde_json::json!({"reason": "accepted; awaiting reconciliation"});
                self.append_event(&intent, KIND_RUNNING, "action.running", payload, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Running,
                    receipt: None,
                })
            }
            ExecutionOutcome::Refused(error) => {
                // A known refusal before an effect: record the failed state and
                // propagate the error (no external effect happened).
                let payload = serde_json::json!({"error": error.to_string()});
                self.append_event(&intent, KIND_FAILED, "action.failed", payload, now)?;
                Err(error)
            }
            ExecutionOutcome::Lost => {
                // Never fabricate success: park as Unknown for reconciliation.
                let payload = serde_json::json!({"reason": "acknowledgement lost"});
                self.append_event(&intent, KIND_UNKNOWN, "action.unknown", payload, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Unknown,
                    receipt: None,
                })
            }
        }
    }

    /// Reconcile an interrupted/ambiguous operation on its pinned generation.
    ///
    /// Never dispatches a new effect and never uses a replacement generation: a
    /// request whose generation differs from the journaled operation is
    /// [`FailureCode::StaleGeneration`].
    pub fn reconcile(&mut self, request: &ActionRequest) -> Result<ActionOutcome, ContractError> {
        let now = (self.now)();
        let (scope, _token) = self.authenticate(&request.grant, now)?;
        self.validate_identity(&scope, &request.plugin_id, request.generation)?;
        let record = self.record_for_request(
            &request.plugin_id,
            request.generation,
            &request.operation_id,
        )?;
        // The advertised descriptor must exist for the pinned generation.
        self.advertised(&scope, request.generation, &record.intent.action_id)?;
        if record.intent.binding.generation != request.generation {
            return Err(FailureCode::StaleGeneration.failure(
                &request.operation_id,
                format!(
                    "journaled operation pins generation {}, request declared {}",
                    record.intent.binding.generation, request.generation
                ),
            ));
        }
        if record.state == ComponentOperationState::Completed
            || record.state == ComponentOperationState::Failed
            || record.state == ComponentOperationState::Cancelled
        {
            return Ok(ActionOutcome::from_record(record));
        }

        let intent = record.intent;
        match self.executor.reconcile(&intent) {
            ReconcileOutcome::Completed(value) => {
                let receipt = self.make_receipt(&intent, value, now);
                self.append_terminal(&intent, KIND_COMPLETED, "action.completed", &receipt, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Completed,
                    receipt: Some(receipt),
                })
            }
            ReconcileOutcome::Failed(message) => {
                let payload = serde_json::json!({"error": message});
                self.append_event(&intent, KIND_FAILED, "action.failed", payload, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Failed,
                    receipt: None,
                })
            }
            ReconcileOutcome::CancelRequested => {
                if record.state != ComponentOperationState::CancelRequested {
                    let payload = serde_json::json!({"source": "reconcile"});
                    self.append_event(
                        &intent,
                        KIND_CANCEL_REPORTED,
                        "action.cancel_reported",
                        payload,
                        now,
                    )?;
                }
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::CancelRequested,
                    receipt: None,
                })
            }
            ReconcileOutcome::Cancelled => {
                let receipt = self.make_receipt(
                    &intent,
                    serde_json::json!({"outcome": "cancelled"}),
                    now,
                );
                self.append_terminal(&intent, KIND_CANCELLED, "action.cancelled", &receipt, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Cancelled,
                    receipt: Some(receipt),
                })
            }
            // Unavailable reconciliation stays parked; never redispatched.
            ReconcileOutcome::Unknown => Ok(ActionOutcome {
                intent,
                state: ComponentOperationState::Unknown,
                receipt: None,
            }),
        }
    }

    /// Request cancellation of a journaled operation.
    ///
    /// The cancel request is journaled durably **before** the executor is
    /// consulted. An acknowledgement (`CancelRequested`) is not terminal: only
    /// settlement evidence (`Cancelled`) or a terminal completion produces a
    /// terminal receipt. Cleanup/cancellation is permitted while the factory is
    /// stopped (it reduces existing exposure), so stop admission is not applied
    /// here.
    pub fn cancel(
        &mut self,
        request: &ActionCancelRequest,
    ) -> Result<ActionOutcome, ContractError> {
        let now = (self.now)();
        let (scope, token) = self.authenticate(&request.grant, now)?;
        self.validate_identity(&scope, &request.plugin_id, request.generation)?;
        if request.deadline_unix_ms <= now {
            return Err(FailureCode::DeadlineExceeded.failure(
                &scope.plugin_id,
                format!("cancel deadline {} already passed", request.deadline_unix_ms),
            ));
        }
        let record = self.record_for_request(
            &request.plugin_id,
            request.generation,
            &request.operation_id,
        )?;
        if record.intent.binding.generation != request.generation {
            return Err(FailureCode::StaleGeneration.failure(
                &request.operation_id,
                "journaled operation pins a different generation",
            ));
        }
        let advertised =
            self.advertised(&scope, request.generation, &record.intent.action_id)?;
        self.check_human_confirmation(
            &advertised.descriptor,
            request.actor,
            request.confirmation.as_deref(),
        )?;

        let cancel = PluginCancelRequest {
            format: PLUGIN_CANCEL_REQUEST_FORMAT.to_string(),
            plugin_id: request.plugin_id.clone(),
            binding: record.intent.binding.clone(),
            cancel_request_id: request.cancel_request_id.clone(),
            operation_id: request.operation_id.clone(),
            deadline_unix_ms: request.deadline_unix_ms,
            grant_fingerprint: sha256_digest(&token),
        };
        cancel.version_gate()?;

        let namespace = action_namespace(&request.plugin_id);
        if let Some(prior) = self.prior_cancel(
            &namespace,
            request.generation,
            &request.operation_id,
            &request.cancel_request_id,
        )? {
            // Duplicate cancel: identical content is a no-op, changed content
            // conflicts; either way the current state is returned.
            cancel.check_replay(&prior)?;
            return Ok(ActionOutcome::from_record(record));
        }
        // An already-terminal operation has nothing to cancel.
        if record.state == ComponentOperationState::Completed
            || record.state == ComponentOperationState::Failed
            || record.state == ComponentOperationState::Cancelled
        {
            return Ok(ActionOutcome::from_record(record));
        }

        let intent = record.intent;
        // Journal the cancel request durably BEFORE calling the executor.
        self.append_event(
            &intent,
            &format!("{CANCEL_REQUESTED_PREFIX}{}", request.cancel_request_id),
            "action.cancel_requested",
            serde_json::to_value(&cancel).map_err(|e| {
                ContractError::KnownFailure(format!("cancel request json: {e}"))
            })?,
            now,
        )?;
        match self.executor.cancel(&intent, &cancel) {
            ReconcileOutcome::Cancelled => {
                let receipt =
                    self.make_receipt(&intent, serde_json::json!({"outcome": "cancelled"}), now);
                self.append_terminal(&intent, KIND_CANCELLED, "action.cancelled", &receipt, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Cancelled,
                    receipt: Some(receipt),
                })
            }
            // Acknowledgement alone is NOT terminal cleanup evidence.
            ReconcileOutcome::CancelRequested | ReconcileOutcome::Unknown => Ok(ActionOutcome {
                intent,
                state: ComponentOperationState::CancelRequested,
                receipt: None,
            }),
            ReconcileOutcome::Completed(value) => {
                let receipt = self.make_receipt(&intent, value, now);
                self.append_terminal(&intent, KIND_COMPLETED, "action.completed", &receipt, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Completed,
                    receipt: Some(receipt),
                })
            }
            ReconcileOutcome::Failed(message) => {
                let payload = serde_json::json!({"error": message});
                self.append_event(&intent, KIND_FAILED, "action.failed", payload, now)?;
                Ok(ActionOutcome {
                    intent,
                    state: ComponentOperationState::Failed,
                    receipt: None,
                })
            }
        }
    }

    /// Scan every known plugin's non-terminal journaled operations and reconcile
    /// each on its **pinned** generation. `Unknown` stays parked. Returns the
    /// reconciled outcomes.
    ///
    /// Known plugins come from installed grants and observed plugin ids; the hub
    /// re-installs grants (or calls [`Self::observe_plugin`]) at startup before
    /// recovery.
    pub fn recover(&mut self) -> Result<Vec<ActionOutcome>, ContractError> {
        let mut outcomes = Vec::new();
        for plugin_id in self.plugins.clone() {
            let namespace = action_namespace(&plugin_id);
            let checkpoint = self.checkpoint_for(&namespace)?;
            for stored in checkpoint.operations {
                let Some((generation, operation_id)) = parse_stored_operation_id(&stored) else {
                    continue;
                };
                let Some(record) = self.read_record(&plugin_id, generation, &operation_id)? else {
                    continue;
                };
                if matches!(
                    record.state,
                    ComponentOperationState::Completed
                        | ComponentOperationState::Failed
                        | ComponentOperationState::Cancelled
                ) {
                    continue;
                }
                let now = (self.now)();
                let intent = record.intent;
                match self.executor.reconcile(&intent) {
                    ReconcileOutcome::Completed(value) => {
                        let receipt = self.make_receipt(&intent, value, now);
                        self.append_terminal(
                            &intent,
                            KIND_COMPLETED,
                            "action.completed",
                            &receipt,
                            now,
                        )?;
                        outcomes.push(ActionOutcome {
                            intent,
                            state: ComponentOperationState::Completed,
                            receipt: Some(receipt),
                        });
                    }
                    ReconcileOutcome::Failed(message) => {
                        let payload = serde_json::json!({"error": message});
                        self.append_event(&intent, KIND_FAILED, "action.failed", payload, now)?;
                        outcomes.push(ActionOutcome {
                            intent,
                            state: ComponentOperationState::Failed,
                            receipt: None,
                        });
                    }
                    ReconcileOutcome::CancelRequested => {
                        outcomes.push(ActionOutcome {
                            intent,
                            state: ComponentOperationState::CancelRequested,
                            receipt: None,
                        });
                    }
                    ReconcileOutcome::Cancelled => {
                        let receipt = self.make_receipt(
                            &intent,
                            serde_json::json!({"outcome": "cancelled"}),
                            now,
                        );
                        self.append_terminal(
                            &intent,
                            KIND_CANCELLED,
                            "action.cancelled",
                            &receipt,
                            now,
                        )?;
                        outcomes.push(ActionOutcome {
                            intent,
                            state: ComponentOperationState::Cancelled,
                            receipt: Some(receipt),
                        });
                    }
                    ReconcileOutcome::Unknown => outcomes.push(ActionOutcome {
                        intent,
                        state: ComponentOperationState::Unknown,
                        receipt: None,
                    }),
                }
            }
        }
        Ok(outcomes)
    }
}

/// Read the checkpoint from an optional state row.
fn checkpoint_from(state: Option<&ComponentStateRow>) -> Result<ActionCheckpoint, ContractError> {
    match state.and_then(|s| s.checkpoint.as_ref()) {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| ContractError::KnownFailure(format!("action checkpoint invalid: {e}"))),
        None => Ok(ActionCheckpoint::default()),
    }
}

/// Derive the current state and terminal receipt from an operation's events.
///
/// The latest event wins; the ledger returns events in ascending sequence order.
/// A terminal receipt is carried by the terminal event payload.
fn derive_state(
    events: &[ComponentEventRow],
) -> Result<(ComponentOperationState, Option<ComponentReceipt>), ContractError> {
    let Some(last) = events.last() else {
        return Ok((ComponentOperationState::Accepted, None));
    };
    match last.event.kind.as_str() {
        "action.completed" => {
            let receipt = parse_receipt(&last.event.payload)?;
            Ok((ComponentOperationState::Completed, Some(receipt)))
        }
        "action.cancelled" => {
            let receipt = parse_receipt(&last.event.payload)?;
            Ok((ComponentOperationState::Cancelled, Some(receipt)))
        }
        "action.failed" => Ok((ComponentOperationState::Failed, None)),
        "action.cancel_requested" | "action.cancel_reported" => {
            Ok((ComponentOperationState::CancelRequested, None))
        }
        "action.unknown" => Ok((ComponentOperationState::Unknown, None)),
        "action.running" => Ok((ComponentOperationState::Running, None)),
        _ => Ok((ComponentOperationState::Accepted, None)),
    }
}

fn parse_receipt(value: &Value) -> Result<ComponentReceipt, ContractError> {
    serde_json::from_value(value.clone())
        .map_err(|e| ContractError::KnownFailure(format!("terminal action receipt invalid: {e}")))
}

/// The current wall clock in Unix milliseconds.
fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::digest::FailureCode as Code;

    fn binding(generation: u64) -> BindingPin {
        BindingPin {
            binding_id: "example.plugin".to_string(),
            api: "plugin".to_string(),
            api_version: 1,
            implementation_version: "1.2.3".to_string(),
            generation,
            config_digest: format!("sha256:{}", "a".repeat(64)),
        }
    }

    fn operation(generation: u64, operation_id: &str, input_digest: &str) -> ComponentOperation {
        ComponentOperation {
            operation_id: stored_operation_id(generation, operation_id),
            input_digest: input_digest.to_string(),
            binding: binding(generation),
            intent: serde_json::json!({"op": operation_id}),
            state: ComponentOperationState::Accepted,
            deadline_unix_ms: 1,
        }
    }

    fn commit(
        store: &InMemoryActionStore,
        commit_id: &str,
        revision: u64,
        events: Vec<ComponentEvent>,
        operations: Vec<ComponentOperation>,
        receipts: Vec<ComponentReceipt>,
    ) -> Result<ComponentCommitAnchor, ContractError> {
        let mut commit = ComponentCommit {
            format: COMPONENT_COMMIT_FORMAT.to_string(),
            namespace: action_namespace("example.plugin"),
            commit_id: commit_id.to_string(),
            input_digest: String::new(),
            expected_revision: revision,
            checkpoint: None,
            events,
            operations,
            receipts,
        };
        commit.input_digest = commit.computed_input_digest()?;
        store.component_commit(&commit)
    }

    #[test]
    fn in_memory_store_is_immutable_per_identity() {
        let store = InMemoryActionStore::new();
        let op = operation(1, "op-1", "sha256:aa");
        commit(
            &store,
            "c1",
            0,
            vec![ComponentEvent {
                event_id: "1:op-1:accepted".to_string(),
                kind: "action.accepted".to_string(),
                payload: serde_json::json!({}),
                recorded_at_unix_ms: 1,
            }],
            vec![op.clone()],
            vec![],
        )
        .unwrap();

        // Identical re-commit of the same operation is a no-op (new commit id).
        commit(&store, "c2", 1, vec![], vec![op.clone()], vec![]).unwrap();
        // Changed content under the same operation id conflicts.
        let changed = operation(1, "op-1", "sha256:bb");
        let err = commit(&store, "c3", 2, vec![], vec![changed], vec![]).unwrap_err();
        assert_eq!(
            agalma_contracts::digest::failure_code(&err),
            Some(Code::IdentityConflict)
        );
    }

    #[test]
    fn event_belongs_distinguishes_prefix_operation_ids() {
        assert!(event_belongs("1:a:accepted", 1, "a"));
        assert!(event_belongs("1:a:cancel_requested:x", 1, "a"));
        assert!(!event_belongs("1:a:b:accepted", 1, "a"));
        assert!(event_belongs("1:a:b:accepted", 1, "a:b"));
        assert!(!event_belongs("2:a:accepted", 1, "a"));
    }

    #[test]
    fn stored_operation_id_round_trips() {
        assert_eq!(parse_stored_operation_id("7:a:b"), Some((7, "a:b".to_string())));
        assert_eq!(parse_stored_operation_id("nope"), None);
    }
}
