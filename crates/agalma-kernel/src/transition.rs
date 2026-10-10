//! Pure transition: `step(state, command, inputs) -> StepResult`.
//!
//! No I/O, no clock, no model/tool calls: every non-deterministic input is
//! recorded in [`Inputs`]. [`transition`] is the design's mandated shape
//! (`transition(state, command, inputs) -> Vec<Effect>`); [`step`] additionally
//! returns the next state and any detected [`Conflict`] so the engine and the
//! reducer can share one deterministic implementation.
//!
//! Effects are `Commit(batch)` (state + event + receipts + intents in one atomic
//! ledger commit) and `Dispatch(intent)` (the post-commit hand-off the
//! conductor performs). A commit that announces work is always followed by its
//! dispatch, which is why a crash between them is recoverable.

use agalma_contracts::{
    ArtifactRef, CommitBatch, DispatchIntent, ExecutionEvent, OperationId, OperationReceipt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::exec_def::{
    encode_event, Command, DirectiveRecord, DirectiveStep, ExecState, Inputs, KernelEvent,
    OperationRecord, OperationState, Probe, EXEC_DEF_VERSION,
};
use crate::hash::content_hash as hash_content;
use crate::state::KernelState;

/// One output of a transition.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Atomic ledger commit (state projection + event + receipts + intents).
    Commit(CommitBatch),
    /// Post-commit hand-off of a recorded intent to an adapter/handler.
    Dispatch(DispatchIntent),
}

/// A rejected input: an id reused with different contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// The reused operation/signal id.
    pub id: String,
    /// Human-readable reason.
    pub reason: String,
}

/// The full result of a pure transition.
#[derive(Clone, Debug, PartialEq)]
pub struct StepResult {
    /// The resulting state (unchanged on conflict).
    pub state: KernelState,
    /// Ordered effects to execute.
    pub effects: Vec<Effect>,
    /// Set when the command was rejected as a conflict.
    pub conflict: Option<Conflict>,
}

/// Pure transition returning only its effects (design-mandated signature).
pub fn transition(state: &KernelState, command: &Command, inputs: &Inputs) -> Vec<Effect> {
    step(state, command, inputs).effects
}

/// Pure transition returning the next state, effects, and any conflict.
pub fn step(state: &KernelState, command: &Command, inputs: &Inputs) -> StepResult {
    if let Some(conflict) = detect_conflict(state, command, inputs) {
        return StepResult {
            state: state.clone(),
            effects: Vec::new(),
            conflict: Some(conflict),
        };
    }
    match command {
        Command::Start {
            task_id,
            generation,
        } => start(state, task_id, *generation, command, inputs),
        Command::Deliver {
            operation_id,
            inputs_hash,
        } => deliver(state, operation_id, inputs_hash, command, inputs),
        Command::Decision {
            operation_id,
            inputs_hash,
        } => decision(state, operation_id, inputs_hash, command, inputs),
        Command::TimerFired {
            timer_id,
            content_hash,
        } => timer_fired(state, timer_id, content_hash, command, inputs),
        Command::DirectiveUpdate {
            directive_id,
            version,
            content_hash,
        } => directive_update(state, directive_id, *version, content_hash, command, inputs),
        Command::Cancel => cancel(state, command, inputs),
        Command::KillLatch { latched } => kill_latch(state, *latched, command, inputs),
        Command::Retry { operation_id } => retry(state, operation_id, command, inputs),
    }
}

/// Detect an id reused with different contents (`Conflict`), else `None`.
///
/// Operations (`Deliver`/`Decision`/`Retry`) conflict when the same operation id
/// carries a different input hash than the recorded one; directives conflict
/// when the same id + version carries different contents; timers conflict when
/// the same id fires with different contents.
pub fn detect_conflict(
    state: &KernelState,
    command: &Command,
    inputs: &Inputs,
) -> Option<Conflict> {
    match command {
        Command::Deliver {
            operation_id,
            inputs_hash,
        }
        | Command::Decision {
            operation_id,
            inputs_hash,
        } => {
            if let Some(receipt) = &inputs.recorded_receipt {
                if receipt.inputs_hash != *inputs_hash {
                    return Some(Conflict {
                        id: operation_id.to_string(),
                        reason: format!(
                            "operation reused with different inputs (existing={}, new={})",
                            receipt.inputs_hash, inputs_hash
                        ),
                    });
                }
            }
            if let Some(record) = state.operations.get(operation_id) {
                if record.inputs_hash != *inputs_hash {
                    return Some(Conflict {
                        id: operation_id.to_string(),
                        reason: format!(
                            "operation reused with different inputs (existing={}, new={})",
                            record.inputs_hash, inputs_hash
                        ),
                    });
                }
            }
            None
        }
        Command::DirectiveUpdate {
            directive_id,
            version,
            content_hash,
        } => state.directives.get(directive_id).and_then(|record| {
            (record.version == *version && record.content_hash != *content_hash).then(|| Conflict {
                id: directive_id.clone(),
                reason: format!(
                    "directive reused with different contents at version {version} \
                     (existing={}, new={content_hash})",
                    record.content_hash
                ),
            })
        }),
        Command::TimerFired {
            timer_id,
            content_hash,
        } => state.fired_timers.get(timer_id).and_then(|existing| {
            (existing != content_hash).then(|| Conflict {
                id: timer_id.clone(),
                reason: format!(
                    "timer reused with different contents (existing={existing}, new={content_hash})"
                ),
            })
        }),
        Command::Start { .. }
        | Command::Cancel
        | Command::KillLatch { .. }
        | Command::Retry { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

fn start(
    state: &KernelState,
    task_id: &agalma_contracts::TaskId,
    generation: u32,
    command: &Command,
    inputs: &Inputs,
) -> StepResult {
    if state.state != ExecState::Pending || !state.task_id.as_str().is_empty() {
        return noop(state);
    }
    let mut next = state.clone();
    next.task_id = task_id.clone();
    next.generation = generation;
    next.exec_def_version = EXEC_DEF_VERSION.to_string();
    next.state = ExecState::Ready;
    next.phase = agalma_contracts::ExecutionPhase::Intake;
    next.bump_revision();
    let commit = commit_effect(state, &next, command, inputs, Vec::new(), Vec::new());
    StepResult {
        state: next,
        effects: vec![Effect::Commit(commit)],
        conflict: None,
    }
}

fn deliver(
    state: &KernelState,
    operation_id: &OperationId,
    inputs_hash: &str,
    command: &Command,
    inputs: &Inputs,
) -> StepResult {
    // Duplicate delivery of an already-recorded operation: return the stored
    // receipt, repeat nothing.
    if inputs.recorded_receipt.is_some() {
        return noop(state);
    }
    match state.operations.get(operation_id).map(|r| r.state) {
        Some(crate::exec_def::OperationState::Completed) => return noop(state),
        // Already announced and dispatching: recovery re-reads the durable
        // intent (never a second announce); a completion may still arrive.
        Some(crate::exec_def::OperationState::Dispatched) if inputs.probe == Probe::Absent => {
            return noop(state)
        }
        _ => {}
    }
    if state.is_terminal() {
        return noop(state);
    }

    match inputs.probe {
        Probe::Unknown => park(
            state,
            command,
            inputs,
            format!("ambiguous external effect for {operation_id}: no probe evidence"),
        ),
        Probe::Present => match &inputs.outcome_ref {
            Some(outcome_ref) => {
                let mut next = state.clone();
                next.complete_operation(operation_id, inputs_hash, outcome_ref);
                let receipt =
                    operation_receipt(state, operation_id, inputs_hash, outcome_ref, inputs);
                let commit =
                    commit_effect(state, &next, command, inputs, vec![receipt], Vec::new());
                StepResult {
                    state: next,
                    effects: vec![Effect::Commit(commit)],
                    conflict: None,
                }
            }
            None => park(
                state,
                command,
                inputs,
                format!("effect present for {operation_id} but no reconcile outcome recorded"),
            ),
        },
        Probe::Absent => {
            if state.kill_latched {
                // Kill latch blocks new and recovered dispatch; no intent, no effect.
                return noop(state);
            }
            let intent = dispatch_intent(state, operation_id, inputs);
            let mut next = state.clone();
            next.announce_operation(operation_id, inputs_hash);
            let commit = commit_effect(
                state,
                &next,
                command,
                inputs,
                Vec::new(),
                vec![intent.clone()],
            );
            StepResult {
                state: next,
                effects: vec![Effect::Commit(commit), Effect::Dispatch(intent)],
                conflict: None,
            }
        }
    }
}

fn decision(
    state: &KernelState,
    operation_id: &OperationId,
    inputs_hash: &str,
    command: &Command,
    inputs: &Inputs,
) -> StepResult {
    if state.is_terminal() {
        return noop(state);
    }
    // A completed decision is terminal: a late/duplicate result cannot
    // supersede the recorded outcome (no new event, no revision bump).
    if matches!(
        state.operations.get(operation_id).map(|r| r.state),
        Some(crate::exec_def::OperationState::Completed)
    ) {
        return noop(state);
    }
    match &inputs.decision {
        Some(outcome) => {
            let mut next = state.clone();
            next.operations.insert(
                operation_id.clone(),
                crate::exec_def::OperationRecord {
                    state: crate::exec_def::OperationState::Completed,
                    inputs_hash: inputs_hash.to_string(),
                    outcome_ref: None,
                },
            );
            next.bump_revision();
            let receipt = OperationReceipt {
                operation_id: operation_id.clone(),
                execution_id: state.execution_id.clone(),
                kind: "decision".to_string(),
                inputs_hash: inputs_hash.to_string(),
                result: serde_json::to_value(outcome).unwrap_or(Value::Null),
                completed_at_unix_ms: inputs.now_unix_ms,
            };
            let commit = commit_effect(state, &next, command, inputs, vec![receipt], Vec::new());
            StepResult {
                state: next,
                effects: vec![Effect::Commit(commit)],
                conflict: None,
            }
        }
        None => {
            if state.kill_latched {
                return noop(state);
            }
            let intent = dispatch_intent(state, operation_id, inputs);
            let mut next = state.clone();
            next.announce_operation(operation_id, inputs_hash);
            let commit = commit_effect(
                state,
                &next,
                command,
                inputs,
                Vec::new(),
                vec![intent.clone()],
            );
            StepResult {
                state: next,
                effects: vec![Effect::Commit(commit), Effect::Dispatch(intent)],
                conflict: None,
            }
        }
    }
}

fn timer_fired(
    state: &KernelState,
    timer_id: &str,
    content_hash: &str,
    command: &Command,
    inputs: &Inputs,
) -> StepResult {
    if state.fired_timers.contains_key(timer_id) {
        return noop(state); // idempotent by timer id
    }
    let mut next = state.clone();
    next.fired_timers
        .insert(timer_id.to_string(), content_hash.to_string());
    if next.state == ExecState::Waiting {
        next.state = ExecState::Ready;
    }
    next.bump_revision();
    let commit = commit_effect(state, &next, command, inputs, Vec::new(), Vec::new());
    StepResult {
        state: next,
        effects: vec![Effect::Commit(commit)],
        conflict: None,
    }
}

fn directive_update(
    state: &KernelState,
    directive_id: &str,
    version: u64,
    content_hash: &str,
    command: &Command,
    inputs: &Inputs,
) -> StepResult {
    let existing = state.directives.get(directive_id);
    if let Some(existing) = existing {
        if existing.version >= version {
            return noop(state); // duplicate or older: harmless
        }
    }
    if state.is_terminal() {
        return noop(state);
    }

    let plan = inputs.directive.as_ref();
    let source = plan.map(|p| p.source.clone()).unwrap_or_default();
    let new_steps: Vec<String> = plan
        .map(|p| p.steps.iter().map(|s| s.step.clone()).collect())
        .unwrap_or_default();

    let mut next = state.clone();
    next.directives.insert(
        directive_id.to_string(),
        DirectiveRecord {
            version,
            content_hash: content_hash.to_string(),
            source,
            steps: new_steps.clone(),
        },
    );

    // Replace queued work: cancel operations derived from the prior plan that
    // the new version no longer wants and that are not already completed.
    if let Some(prior) = existing {
        for step in &prior.steps {
            if new_steps.contains(step) {
                continue;
            }
            let op = OperationId::derive(&state.execution_id, step);
            if let Some(record) = next.operations.get_mut(&op) {
                if record.state != OperationState::Completed {
                    record.state = OperationState::Cancelled;
                }
            }
        }
    }

    // Derive queued commands from the active directive: announce and dispatch
    // the plan's steps. The kill latch blocks new/recovered dispatch.
    let mut intents: Vec<DispatchIntent> = Vec::new();
    if let Some(plan) = plan {
        if !state.kill_latched {
            for step in &plan.steps {
                let op = OperationId::derive(&state.execution_id, &step.step);
                match next.operations.get(&op).map(|r| r.state) {
                    Some(OperationState::Completed) | Some(OperationState::Dispatched) => continue,
                    _ => {}
                }
                let step_hash = hash_content(&serde_json::json!({
                    "activity": step.activity,
                    "inputs": step.inputs,
                }));
                next.operations.insert(
                    op,
                    OperationRecord {
                        state: OperationState::Dispatched,
                        inputs_hash: step_hash,
                        outcome_ref: None,
                    },
                );
                intents.push(plan_intent(state, step, inputs));
            }
        }
    }

    if !intents.is_empty() && next.state == ExecState::Ready {
        next.state = ExecState::Running;
    }
    next.bump_revision();

    let mut effects = vec![Effect::Commit(commit_effect(
        state,
        &next,
        command,
        inputs,
        Vec::new(),
        intents.clone(),
    ))];
    for intent in intents {
        effects.push(Effect::Dispatch(intent));
    }
    StepResult {
        state: next,
        effects,
        conflict: None,
    }
}

fn cancel(state: &KernelState, command: &Command, inputs: &Inputs) -> StepResult {
    if state.is_terminal() {
        return noop(state);
    }
    let mut next = state.clone();
    next.state = ExecState::Cancelled;
    next.parked_reason = None;
    for record in next.operations.values_mut() {
        if !matches!(
            record.state,
            crate::exec_def::OperationState::Completed | crate::exec_def::OperationState::Failed
        ) {
            record.state = crate::exec_def::OperationState::Cancelled;
        }
    }
    next.bump_revision();
    let commit = commit_effect(state, &next, command, inputs, Vec::new(), Vec::new());
    StepResult {
        state: next,
        effects: vec![Effect::Commit(commit)],
        conflict: None,
    }
}

fn kill_latch(
    state: &KernelState,
    latched: bool,
    command: &Command,
    inputs: &Inputs,
) -> StepResult {
    if state.kill_latched == latched {
        return noop(state);
    }
    let mut next = state.clone();
    next.kill_latched = latched;
    next.bump_revision();
    let commit = commit_effect(state, &next, command, inputs, Vec::new(), Vec::new());
    StepResult {
        state: next,
        effects: vec![Effect::Commit(commit)],
        conflict: None,
    }
}

fn retry(
    state: &KernelState,
    operation_id: &OperationId,
    command: &Command,
    inputs: &Inputs,
) -> StepResult {
    if state.is_terminal() {
        return noop(state);
    }
    let attempt = state.attempt + 1;
    let new_op = crate::exec_def::retry_operation_id(operation_id, attempt);
    let mut next = state.clone();
    next.attempt = attempt;
    next.announce_operation(&new_op, operation_id.as_str());
    let mut intent = dispatch_intent(state, &new_op, inputs);
    intent.payload = serde_json::json!({
        "operation": new_op.as_str(),
        "retry_of": operation_id.as_str(),
        "attempt": attempt,
    });
    let commit = commit_effect(
        state,
        &next,
        command,
        inputs,
        Vec::new(),
        vec![intent.clone()],
    );
    StepResult {
        state: next,
        effects: vec![Effect::Commit(commit), Effect::Dispatch(intent)],
        conflict: None,
    }
}

// ---------------------------------------------------------------------------
// Effect builders
// ---------------------------------------------------------------------------

fn noop(state: &KernelState) -> StepResult {
    StepResult {
        state: state.clone(),
        effects: Vec::new(),
        conflict: None,
    }
}

fn park(state: &KernelState, command: &Command, inputs: &Inputs, reason: String) -> StepResult {
    let mut next = state.park(reason);
    next.bump_revision();
    let commit = commit_effect(state, &next, command, inputs, Vec::new(), Vec::new());
    StepResult {
        state: next,
        effects: vec![Effect::Commit(commit)],
        conflict: None,
    }
}

fn operation_receipt(
    state: &KernelState,
    operation_id: &OperationId,
    inputs_hash: &str,
    outcome_ref: &ArtifactRef,
    inputs: &Inputs,
) -> OperationReceipt {
    OperationReceipt {
        operation_id: operation_id.clone(),
        execution_id: state.execution_id.clone(),
        kind: "operation".to_string(),
        inputs_hash: inputs_hash.to_string(),
        // Reference only: large artifacts never enter history.
        result: KernelState::outcome_result(outcome_ref),
        completed_at_unix_ms: inputs.now_unix_ms,
    }
}

/// Build the dispatch intent for a directive-queued step.
fn plan_intent(state: &KernelState, step: &DirectiveStep, inputs: &Inputs) -> DispatchIntent {
    let operation_id = OperationId::derive(&state.execution_id, &step.step);
    DispatchIntent {
        operation_id: operation_id.clone(),
        execution_id: state.execution_id.clone(),
        payload: serde_json::json!({
            "operation": operation_id.as_str(),
            "phase": format!("{:?}", step.phase).to_lowercase(),
            "attempt": state.attempt,
            "activity": step.activity,
            "inputs": step.inputs,
            "binding": inputs.binding,
            "operation_phase": step.phase,
        }),
        enqueued_at_unix_ms: inputs.now_unix_ms,
        consumed: false,
    }
}

fn dispatch_intent(
    state: &KernelState,
    operation_id: &OperationId,
    inputs: &Inputs,
) -> DispatchIntent {
    DispatchIntent {
        operation_id: operation_id.clone(),
        execution_id: state.execution_id.clone(),
        payload: serde_json::json!({
            "operation": operation_id.as_str(),
            "phase": format!("{:?}", state.phase).to_lowercase(),
            "attempt": state.attempt,
            // Activity-protocol metadata: recovered from the durable intent so a
            // restart can reconcile the pending operation against its pinned
            // binding without the original caller.
            "activity": inputs.activity,
            "inputs": inputs.operation_inputs,
            "binding": inputs.binding,
            "operation_phase": inputs.phase,
        }),
        enqueued_at_unix_ms: inputs.now_unix_ms,
        consumed: false,
    }
}

fn commit_effect(
    from: &KernelState,
    next: &KernelState,
    command: &Command,
    inputs: &Inputs,
    receipts: Vec<OperationReceipt>,
    intents: Vec<DispatchIntent>,
) -> CommitBatch {
    let event = ExecutionEvent {
        execution_id: next.execution_id.clone(),
        sequence: 0, // ledger-owned; assigned strictly monotonic per execution
        kind: command.kind().to_string(),
        payload: encode_event(&KernelEvent {
            command: command.clone(),
            inputs: inputs.clone(),
        }),
        recorded_at_unix_ms: inputs.now_unix_ms,
    };
    CommitBatch {
        expected_revision: Some(from.revision),
        state: Some(next.projection()),
        events: vec![event],
        receipts,
        intents,
    }
}
