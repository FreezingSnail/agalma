//! Activity protocol: named activity registry, handler seam, and the durable
//! operation driver.
//!
//! `docs/b0-kernel.md` §"Activity protocol": handlers are registered under a
//! versioned [`ActivityDef`] and expose `execute(ctx) -> Outcome` and
//! `reconcile(ctx) -> EffectProbe`. The driver in this module performs the
//! non-deterministic external work *around* the pure transition:
//!
//! 1. **Stable operation IDs + input hashes.** An operation is
//!    `op:<execution>:<step>`; its input hash is a deterministic digest of the
//!    activity definition and recorded inputs. Reusing an id with different
//!    contents is a [`Conflict`].
//! 2. **Persist before advancing.** A new operation is announced (dispatch
//!    intent durable) before the handler runs; the completion receipt is
//!    committed before any dependent work.
//! 3. **Duplicate delivery returns the stored result.** A recorded receipt is
//!    returned unchanged; the handler is never re-run.
//! 4. **Reconcile before retry.** A pending operation's external effect is
//!    probed first: present → complete; absent → (re-)execute; ambiguous → park.
//! 5. **Binding pinning.** The component/API binding the operation was
//!    dispatched under is recorded on the intent and reused on reconciliation,
//!    never the currently configured adapter.
//!
//! The transition function never calls a handler; only this driver does.

use std::collections::BTreeMap;

use agalma_contracts::{
    ArtifactRef, ContractError, DispatchIntent, ExecutionId, ExecutionPhase, LedgerApi,
    OperationId, OperationReceipt, TaskId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::binding::{active_binding, BindingPin};
use crate::crash::{
    crash_hook, AFTER_DISPATCH, AFTER_EFFECT_BEFORE_COMPLETION, AFTER_MERGE_BEFORE_COMPLETION,
    BEFORE_DISPATCH,
};
use crate::engine::KernelEngine;
use crate::exec_def::{Command, ExecState, Inputs, OperationState, Probe};
use crate::transition::Conflict;

/// A versioned activity definition (`name` + `version`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ActivityDef {
    pub name: String,
    pub version: String,
}

impl ActivityDef {
    /// Build a definition.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        ActivityDef {
            name: name.into(),
            version: version.into(),
        }
    }

    /// Registry key `name@version`.
    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// The recorded outcome of a successful activity: a reference to the result
/// artifact (large artifacts never enter history).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityOutcome {
    pub outcome_ref: ArtifactRef,
}

impl ActivityOutcome {
    pub fn new(outcome_ref: ArtifactRef) -> Self {
        ActivityOutcome { outcome_ref }
    }
}

/// Result of probing a pending operation's external effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectProbe {
    /// The effect is present; the carried outcome completes it without
    /// repeating the effect.
    Present(ActivityOutcome),
    /// The effect is observably absent; it is safe to (re-)execute.
    Absent,
    /// Cannot determine whether the effect happened; park rather than risk a
    /// duplicate.
    Ambiguous,
}

/// Errors an activity handler may return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityError {
    /// The effect failed for a known reason.
    KnownFailure(String),
    /// The outcome is unknown; the driver parks so the effect can be reconciled.
    UnknownOutcome(String),
}

impl std::fmt::Display for ActivityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivityError::KnownFailure(message) => write!(f, "activity known failure: {message}"),
            ActivityError::UnknownOutcome(message) => {
                write!(f, "activity unknown outcome: {message}")
            }
        }
    }
}

impl std::error::Error for ActivityError {}

/// Everything an activity handler needs to perform one operation.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityContext {
    pub execution_id: ExecutionId,
    pub operation_id: OperationId,
    pub task_id: TaskId,
    /// Phase whose effect this operation performs.
    pub phase: ExecutionPhase,
    /// 1-based attempt number.
    pub attempt: u32,
    /// The registered activity definition.
    pub activity: ActivityDef,
    /// The component/API binding this operation is pinned to, if any.
    pub binding: Option<BindingPin>,
    /// Bounded, opaque operation inputs recorded with the intent.
    pub inputs: Value,
    pub now_unix_ms: u64,
}

/// A registered activity handler.
pub trait ActivityHandler {
    /// The versioned definition this handler serves.
    fn def(&self) -> ActivityDef;

    /// Perform the effect. Called only when the probe returned [`EffectProbe::Absent`]
    /// (or for a brand-new operation).
    fn execute(&mut self, ctx: &ActivityContext) -> Result<ActivityOutcome, ActivityError>;

    /// Probe a pending operation's external effect and, when present, complete it.
    fn reconcile(&mut self, ctx: &ActivityContext) -> Result<EffectProbe, ActivityError>;
}

/// Registry of activity definitions to handlers, keyed by `name@version`.
///
/// Registering the same `name@version` twice is a [`Conflict`] (an id reused
/// with different contents).
#[derive(Default)]
pub struct ActivityRegistry {
    handlers: BTreeMap<String, Box<dyn ActivityHandler>>,
}

impl ActivityRegistry {
    pub fn new() -> Self {
        ActivityRegistry {
            handlers: BTreeMap::new(),
        }
    }

    /// Register a handler. Errors when its `name@version` is already present.
    pub fn register(&mut self, handler: Box<dyn ActivityHandler>) -> Result<(), Conflict> {
        let def = handler.def();
        let key = def.key();
        if self.handlers.contains_key(&key) {
            return Err(Conflict {
                id: def.name.clone(),
                reason: format!("activity {key} is already registered"),
            });
        }
        self.handlers.insert(key, handler);
        Ok(())
    }

    /// Whether a handler is registered for `def`.
    pub fn contains(&self, def: &ActivityDef) -> bool {
        self.handlers.contains_key(&def.key())
    }

    /// Mutable handler for `def`, if registered.
    pub fn handler_mut<'a>(
        &'a mut self,
        def: &ActivityDef,
    ) -> Option<&'a mut (dyn ActivityHandler + 'static)> {
        self.handlers
            .get_mut(&def.key())
            .map(|handler| &mut **handler)
    }

    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

/// A request to run one activity operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationRequest {
    pub activity: ActivityDef,
    /// Operation-id step (`op:<execution>:<step>`).
    pub step: String,
    pub phase: ExecutionPhase,
    /// Component API whose active binding is pinned for this operation.
    pub component_api: String,
    /// Bounded, opaque operation inputs (hashed and recorded).
    pub inputs: Value,
    pub now_unix_ms: u64,
}

/// The result of running one activity operation.
#[derive(Clone, Debug, PartialEq)]
pub enum OperationResult {
    /// The operation completed (receipt recorded). `duplicate` is true when a
    /// stored receipt was returned; `reconciled` is true when a present external
    /// effect was completed without re-executing it.
    Completed {
        operation_id: OperationId,
        receipt: OperationReceipt,
        reconciled: bool,
        duplicate: bool,
    },
    /// The operation parked (ambiguous external effect).
    Parked {
        operation_id: OperationId,
        reason: String,
    },
    /// The operation id was reused with different contents.
    Conflict(Conflict),
    /// Dispatch was blocked (kill latch).
    Blocked { operation_id: OperationId },
    /// No handler is registered for the requested activity.
    UnknownActivity { activity: ActivityDef },
}

/// Deterministic content hash of an operation's inputs (`activity` + `inputs`).
///
/// Used as the transition's `inputs_hash`: reusing an operation id with a
/// different hash is a [`Conflict`]. FNV-1a is a conflict detector, not a
/// security primitive (the pinned dependency set has no hash crate).
pub fn operation_input_hash(activity: &ActivityDef, inputs: &Value) -> String {
    let canonical = serde_json::json!({ "activity": activity, "inputs": inputs });
    crate::hash::content_hash(&canonical)
}

/// Durable metadata for one operation, sourced from the request or a recovered
/// dispatch intent.
#[derive(Clone, Debug)]
pub(crate) struct PendingMeta {
    pub(crate) execution_id: ExecutionId,
    pub(crate) operation_id: OperationId,
    pub(crate) activity: ActivityDef,
    pub(crate) phase: ExecutionPhase,
    pub(crate) attempt: u32,
    pub(crate) inputs: Value,
    pub(crate) inputs_hash: String,
    pub(crate) binding: Option<BindingPin>,
    pub(crate) now_unix_ms: u64,
}

impl PendingMeta {
    fn from_intent(intent: &DispatchIntent) -> Option<Self> {
        let payload = &intent.payload;
        let activity: ActivityDef =
            serde_json::from_value(payload.get("activity")?.clone()).ok()?;
        let inputs = payload.get("inputs").cloned().unwrap_or(Value::Null);
        let binding = payload
            .get("binding")
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        let phase = payload
            .get("operation_phase")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .or_else(|| {
                payload
                    .get("phase")
                    .and_then(Value::as_str)
                    .and_then(phase_from_str)
            })
            .unwrap_or(ExecutionPhase::Intake);
        let attempt = payload.get("attempt").and_then(Value::as_u64).unwrap_or(0) as u32;
        Some(PendingMeta {
            execution_id: intent.execution_id.clone(),
            operation_id: intent.operation_id.clone(),
            inputs_hash: operation_input_hash(&activity, &inputs),
            activity,
            phase,
            attempt,
            inputs,
            binding,
            now_unix_ms: intent.enqueued_at_unix_ms,
        })
    }

    pub(crate) fn context<L: LedgerApi>(
        &self,
        engine: &KernelEngine<L>,
    ) -> Result<ActivityContext, ContractError> {
        let state = engine.load(&self.execution_id)?;
        Ok(ActivityContext {
            execution_id: self.execution_id.clone(),
            operation_id: self.operation_id.clone(),
            task_id: state.task_id,
            phase: self.phase,
            attempt: self.attempt,
            activity: self.activity.clone(),
            binding: self.binding.clone(),
            inputs: self.inputs.clone(),
            now_unix_ms: self.now_unix_ms,
        })
    }

    fn command(&self) -> Command {
        Command::Deliver {
            operation_id: self.operation_id.clone(),
            inputs_hash: self.inputs_hash.clone(),
        }
    }

    fn inputs(&self, probe: Probe) -> Inputs {
        Inputs {
            probe,
            binding: self.binding.clone(),
            activity: Some(self.activity.clone()),
            operation_inputs: Some(self.inputs.clone()),
            phase: Some(self.phase),
            now_unix_ms: self.now_unix_ms,
            ..Inputs::default()
        }
    }
}

/// Run one activity operation: announce (if new), perform or reconcile the
/// effect, then persist the completion.
///
/// Duplicate delivery returns the stored receipt; a pending operation is
/// reconciled before retry; an ambiguous effect parks.
pub fn run_activity<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    execution_id: &ExecutionId,
    request: &OperationRequest,
) -> Result<OperationResult, ContractError> {
    let operation_id = OperationId::derive(execution_id, &request.step);
    let inputs_hash = operation_input_hash(&request.activity, &request.inputs);

    // Duplicate delivery / conflict against the recorded receipt.
    if let Some(receipt) = engine.receipt(&operation_id)? {
        if receipt.inputs_hash != inputs_hash {
            return Ok(OperationResult::Conflict(Conflict {
                id: operation_id.to_string(),
                reason: format!(
                    "operation reused with different inputs (existing={}, new={inputs_hash})",
                    receipt.inputs_hash
                ),
            }));
        }
        return Ok(OperationResult::Completed {
            operation_id,
            receipt,
            reconciled: false,
            duplicate: true,
        });
    }

    let state = engine.load(execution_id)?;
    if state.state == ExecState::Parked {
        return Ok(OperationResult::Parked {
            operation_id,
            reason: state.parked_reason.clone().unwrap_or_default(),
        });
    }
    if state.kill_latched {
        return Ok(OperationResult::Blocked { operation_id });
    }

    if let Some(record) = state.operations.get(&operation_id) {
        if record.inputs_hash != inputs_hash {
            return Ok(OperationResult::Conflict(Conflict {
                id: operation_id.to_string(),
                reason: format!(
                    "operation reused with different inputs (existing={}, new={inputs_hash})",
                    record.inputs_hash
                ),
            }));
        }
    }

    // A pending intent keeps its pinned binding; a new operation pins the
    // component's current active binding.
    let binding = match pending_meta(engine, &operation_id)? {
        Some(meta) => meta.binding,
        None => active_binding(engine.ledger(), &request.component_api)?,
    };

    let meta = PendingMeta {
        execution_id: execution_id.clone(),
        operation_id: operation_id.clone(),
        activity: request.activity.clone(),
        phase: request.phase,
        attempt: state.attempt,
        inputs: request.inputs.clone(),
        inputs_hash,
        binding,
        now_unix_ms: request.now_unix_ms,
    };

    let pending = state
        .operations
        .get(&operation_id)
        .map(|record| record.state)
        == Some(OperationState::Dispatched);
    if pending {
        reconcile_pending(engine, registry, &meta)
    } else {
        dispatch_and_execute(engine, registry, &meta)
    }
}

/// Reconcile every pending activity operation of `execution_id`, in durable
/// intent order. Operations without activity metadata are skipped (caller-owned).
pub fn recover_pending<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    execution_id: &ExecutionId,
    now_unix_ms: u64,
) -> Result<Vec<OperationResult>, ContractError> {
    let mut results = Vec::new();
    for intent in engine.pending_dispatch()? {
        if &intent.execution_id != execution_id {
            continue;
        }
        // A park halts further recovery for this execution; the kill latch
        // blocks recovered dispatch exactly as it blocks new work.
        let state = engine.load(execution_id)?;
        if state.state == ExecState::Parked || state.kill_latched {
            break;
        }
        let Some(mut meta) = PendingMeta::from_intent(&intent) else {
            continue;
        };
        meta.now_unix_ms = now_unix_ms;
        if engine.receipt(&meta.operation_id)?.is_some() {
            continue;
        }
        results.push(reconcile_pending(engine, registry, &meta)?);
    }
    Ok(results)
}

/// Reconcile every pending activity operation of `execution_id`, ignoring the
/// kill latch.
///
/// Used by the staged-activation procedure ([`crate::activation`]): the latch is
/// set to stop **new** dispatch while the binding is swapped, but effects that
/// are already in flight must still be reconciled. A parked execution still
/// stops recovery; an absent effect is left pending (it resumes under its pinned
/// binding after activation) and never re-executed here. Ambiguous effects
/// return [`OperationResult::Parked`].
pub fn settle_pending<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    execution_id: &ExecutionId,
    now_unix_ms: u64,
) -> Result<Vec<OperationResult>, ContractError> {
    let mut results = Vec::new();
    for intent in engine.pending_dispatch()? {
        if &intent.execution_id != execution_id {
            continue;
        }
        let state = engine.load(execution_id)?;
        if state.state == ExecState::Parked {
            break;
        }
        let Some(mut meta) = PendingMeta::from_intent(&intent) else {
            continue;
        };
        meta.now_unix_ms = now_unix_ms;
        if engine.receipt(&meta.operation_id)?.is_some() {
            continue;
        }
        let ctx = meta.context(engine)?;
        match call_reconcile(registry, &ctx)? {
            EffectProbe::Present(outcome) => results.push(complete(engine, &meta, outcome, true)?),
            // Absent: leave pending for the post-activation resume.
            EffectProbe::Absent => {}
            EffectProbe::Ambiguous => {
                let apply = engine.apply(
                    &meta.execution_id,
                    &meta.command(),
                    &meta.inputs(Probe::Unknown),
                )?;
                results.push(OperationResult::Parked {
                    operation_id: meta.operation_id.clone(),
                    reason: apply.state.parked_reason.unwrap_or_default(),
                });
            }
        }
    }
    Ok(results)
}

pub(crate) fn pending_meta<L: LedgerApi>(
    engine: &KernelEngine<L>,
    operation_id: &OperationId,
) -> Result<Option<PendingMeta>, ContractError> {
    Ok(engine
        .pending_dispatch()?
        .iter()
        .find(|intent| &intent.operation_id == operation_id)
        .and_then(PendingMeta::from_intent))
}

/// Announce the dispatch intent, then execute the effect and persist completion.
fn dispatch_and_execute<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    meta: &PendingMeta,
) -> Result<OperationResult, ContractError> {
    if !registry.contains(&meta.activity) {
        return Ok(OperationResult::UnknownActivity {
            activity: meta.activity.clone(),
        });
    }

    let announce = engine.apply(
        &meta.execution_id,
        &meta.command(),
        &meta.inputs(Probe::Absent),
    )?;
    if let Some(conflict) = announce.conflict {
        return Ok(OperationResult::Conflict(conflict));
    }

    let ctx = meta.context(engine)?;
    crash_hook(BEFORE_DISPATCH);
    let outcome = call_execute(registry, &ctx)?;
    crash_hook(AFTER_DISPATCH);
    crash_hook(AFTER_EFFECT_BEFORE_COMPLETION);
    if meta.phase == ExecutionPhase::Integrate {
        crash_hook(AFTER_MERGE_BEFORE_COMPLETION);
    }

    complete(engine, meta, outcome, false)
}

/// Probe a pending operation's external effect, then complete, retry, or park.
fn reconcile_pending<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    meta: &PendingMeta,
) -> Result<OperationResult, ContractError> {
    if !registry.contains(&meta.activity) {
        return Ok(OperationResult::UnknownActivity {
            activity: meta.activity.clone(),
        });
    }

    // The kill latch blocks recovered dispatch (a recovering conductor must not
    // re-run a pending effect while latched).
    if engine.load(&meta.execution_id)?.kill_latched {
        return Ok(OperationResult::Blocked {
            operation_id: meta.operation_id.clone(),
        });
    }

    let ctx = meta.context(engine)?;
    crash_hook(BEFORE_DISPATCH);
    let probe = call_reconcile(registry, &ctx)?;
    crash_hook(AFTER_DISPATCH);

    match probe {
        EffectProbe::Present(outcome) => complete(engine, meta, outcome, true),
        EffectProbe::Absent => {
            crash_hook(BEFORE_DISPATCH);
            let outcome = call_execute(registry, &ctx)?;
            crash_hook(AFTER_DISPATCH);
            crash_hook(AFTER_EFFECT_BEFORE_COMPLETION);
            if meta.phase == ExecutionPhase::Integrate {
                crash_hook(AFTER_MERGE_BEFORE_COMPLETION);
            }
            complete(engine, meta, outcome, false)
        }
        EffectProbe::Ambiguous => {
            let apply = engine.apply(
                &meta.execution_id,
                &meta.command(),
                &meta.inputs(Probe::Unknown),
            )?;
            Ok(OperationResult::Parked {
                operation_id: meta.operation_id.clone(),
                reason: apply.state.parked_reason.unwrap_or_default(),
            })
        }
    }
}

/// Commit the completion receipt for `meta` and return the recorded result.
pub(crate) fn complete<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    meta: &PendingMeta,
    outcome: ActivityOutcome,
    reconciled: bool,
) -> Result<OperationResult, ContractError> {
    let mut inputs = meta.inputs(Probe::Present);
    inputs.outcome_ref = Some(outcome.outcome_ref);
    let apply = engine.apply(&meta.execution_id, &meta.command(), &inputs)?;
    if let Some(conflict) = apply.conflict {
        return Ok(OperationResult::Conflict(conflict));
    }
    let receipt = engine.receipt(&meta.operation_id)?.ok_or_else(|| {
        ContractError::KnownFailure(format!(
            "completion receipt missing for {}",
            meta.operation_id
        ))
    })?;
    Ok(OperationResult::Completed {
        operation_id: meta.operation_id.clone(),
        receipt,
        reconciled,
        duplicate: false,
    })
}

// ---------------------------------------------------------------------------
// Handler-registry protocol (B1-F1): caller-side dispatch over the boundary
// ---------------------------------------------------------------------------

/// Where an operation stands when it is handed to a caller-side handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectMode {
    /// A brand-new operation: its dispatch intent is durable; the caller executes.
    Dispatch,
    /// A pending (in-flight) operation: the caller probes, then executes if absent.
    Reconcile,
}

/// An operation handed to a caller-side handler.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityBegin {
    /// Everything the handler needs (operation id, activity, phase, inputs,
    /// pinned binding, context clock).
    pub context: ActivityContext,
    /// Whether the caller executes straight away or probes first.
    pub mode: EffectMode,
}

/// The result of beginning an operation over the boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum BeginOutcome {
    /// The caller must perform this effect, then call [`finish_activity`].
    Effect(ActivityBegin),
    /// No effect is needed; this is the settled result.
    Terminal(OperationResult),
}

/// The caller's report after performing (or probing) an operation's effect.
///
/// Mirrors the wire [`agalma_ipc::EffectReport`] without depending on it, so the
/// kernel's activity module stays free of the transport types.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectReply {
    /// The reconcile probe found the effect already present.
    Present(ActivityOutcome),
    /// The effect was executed now.
    Executed(ActivityOutcome),
    /// Ambiguous effect; the execution parks.
    Ambiguous,
    /// The handler failed for a known reason.
    KnownFailure(String),
    /// The handler's outcome is unknown; the execution parks.
    UnknownOutcome(String),
}

/// Begin an activity operation without running the handler: perform the durable
/// checks and, for a fresh operation, announce its intent.
///
/// This is the caller-side-dispatch half of [`run_activity`]: the engine owns
/// every durable step (duplicate/conflict/park/blocked checks and the announce),
/// while the nondeterministic effect is deferred to the caller through the
/// handler-registry protocol. `run_activity` remains the in-process composition
/// of this and [`finish_activity`] with a local registry.
pub fn begin_activity<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    execution_id: &ExecutionId,
    request: &OperationRequest,
) -> Result<BeginOutcome, ContractError> {
    let operation_id = OperationId::derive(execution_id, &request.step);
    let inputs_hash = operation_input_hash(&request.activity, &request.inputs);

    // Duplicate delivery / conflict against the recorded receipt.
    if let Some(receipt) = engine.receipt(&operation_id)? {
        if receipt.inputs_hash != inputs_hash {
            return Ok(BeginOutcome::Terminal(OperationResult::Conflict(Conflict {
                id: operation_id.to_string(),
                reason: format!(
                    "operation reused with different inputs (existing={}, new={inputs_hash})",
                    receipt.inputs_hash
                ),
            })));
        }
        return Ok(BeginOutcome::Terminal(OperationResult::Completed {
            operation_id,
            receipt,
            reconciled: false,
            duplicate: true,
        }));
    }

    let state = engine.load(execution_id)?;
    if state.state == ExecState::Parked {
        return Ok(BeginOutcome::Terminal(OperationResult::Parked {
            operation_id,
            reason: state.parked_reason.clone().unwrap_or_default(),
        }));
    }
    if state.kill_latched {
        return Ok(BeginOutcome::Terminal(OperationResult::Blocked { operation_id }));
    }
    if let Some(record) = state.operations.get(&operation_id) {
        if record.inputs_hash != inputs_hash {
            return Ok(BeginOutcome::Terminal(OperationResult::Conflict(Conflict {
                id: operation_id.to_string(),
                reason: format!(
                    "operation reused with different inputs (existing={}, new={inputs_hash})",
                    record.inputs_hash
                ),
            })));
        }
    }

    // A pending intent keeps its pinned binding; a new operation pins the
    // component's current active binding.
    let binding = match pending_meta(engine, &operation_id)? {
        Some(meta) => meta.binding,
        None => active_binding(engine.ledger(), &request.component_api)?,
    };
    let meta = PendingMeta {
        execution_id: execution_id.clone(),
        operation_id: operation_id.clone(),
        activity: request.activity.clone(),
        phase: request.phase,
        attempt: state.attempt,
        inputs: request.inputs.clone(),
        inputs_hash,
        binding,
        now_unix_ms: request.now_unix_ms,
    };

    let pending = state.operations.get(&operation_id).map(|record| record.state)
        == Some(OperationState::Dispatched);
    if pending {
        let context = meta.context(engine)?;
        return Ok(BeginOutcome::Effect(ActivityBegin {
            context,
            mode: EffectMode::Reconcile,
        }));
    }

    // Announce (durable) *before* the caller runs the effect: a crash after the
    // effect but before completion is recovered from the intent.
    let announce = engine.apply(
        &meta.execution_id,
        &meta.command(),
        &meta.inputs(Probe::Absent),
    )?;
    if let Some(conflict) = announce.conflict {
        return Ok(BeginOutcome::Terminal(OperationResult::Conflict(conflict)));
    }
    let context = meta.context(engine)?;
    Ok(BeginOutcome::Effect(ActivityBegin {
        context,
        mode: EffectMode::Dispatch,
    }))
}

/// Finish an operation begun by [`begin_activity`], committing the caller's
/// effect report (or parking on an ambiguous/unknown outcome).
pub fn finish_activity<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    execution_id: &ExecutionId,
    operation_id: &OperationId,
    reply: EffectReply,
    now_unix_ms: u64,
) -> Result<OperationResult, ContractError> {
    let Some(mut meta) = pending_meta(engine, operation_id)? else {
        return Err(ContractError::KnownFailure(format!(
            "no pending operation {operation_id} to complete"
        )));
    };
    if &meta.execution_id != execution_id {
        return Err(ContractError::KnownFailure(format!(
            "operation {operation_id} does not belong to {execution_id}"
        )));
    }
    meta.now_unix_ms = now_unix_ms;
    // Idempotent: a duplicate completion returns the recorded receipt.
    if let Some(receipt) = engine.receipt(operation_id)? {
        return Ok(OperationResult::Completed {
            operation_id: operation_id.clone(),
            receipt,
            reconciled: false,
            duplicate: true,
        });
    }
    match reply {
        EffectReply::Present(outcome) => complete(engine, &meta, outcome, true),
        EffectReply::Executed(outcome) => complete(engine, &meta, outcome, false),
        EffectReply::Ambiguous => park_ambiguous(engine, &meta),
        EffectReply::KnownFailure(message) => Err(ContractError::KnownFailure(message)),
        EffectReply::UnknownOutcome(message) => Err(ContractError::UnknownOutcome(message)),
    }
}

/// Park an execution via the ambiguous-effect path (no probe evidence), so the
/// durable projection and history stay in agreement.
fn park_ambiguous<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    meta: &PendingMeta,
) -> Result<OperationResult, ContractError> {
    let apply = engine.apply(
        &meta.execution_id,
        &meta.command(),
        &meta.inputs(Probe::Unknown),
    )?;
    Ok(OperationResult::Parked {
        operation_id: meta.operation_id.clone(),
        reason: apply.state.parked_reason.unwrap_or_default(),
    })
}

fn call_execute(
    registry: &mut ActivityRegistry,
    ctx: &ActivityContext,
) -> Result<ActivityOutcome, ContractError> {
    let handler = registry.handler_mut(&ctx.activity).ok_or_else(|| {
        ContractError::KnownFailure(format!(
            "no handler registered for activity {}",
            ctx.activity.key()
        ))
    })?;
    handler.execute(ctx).map_err(activity_err)
}

fn call_reconcile(
    registry: &mut ActivityRegistry,
    ctx: &ActivityContext,
) -> Result<EffectProbe, ContractError> {
    let handler = registry.handler_mut(&ctx.activity).ok_or_else(|| {
        ContractError::KnownFailure(format!(
            "no handler registered for activity {}",
            ctx.activity.key()
        ))
    })?;
    handler.reconcile(ctx).map_err(activity_err)
}

fn activity_err(error: ActivityError) -> ContractError {
    match error {
        ActivityError::KnownFailure(message) => ContractError::KnownFailure(message),
        ActivityError::UnknownOutcome(message) => ContractError::UnknownOutcome(message),
    }
}

fn phase_from_str(raw: &str) -> Option<ExecutionPhase> {
    Some(match raw {
        "intake" => ExecutionPhase::Intake,
        "checkout" => ExecutionPhase::Checkout,
        "build" => ExecutionPhase::Build,
        "verify" => ExecutionPhase::Verify,
        "integrate" => ExecutionPhase::Integrate,
        "done" => ExecutionPhase::Done,
        "parked" => ExecutionPhase::Parked,
        _ => return None,
    })
}
