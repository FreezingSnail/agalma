//! Interactive control actions (TUI.5, `agalma-3kr.6`): pause/resume (kill
//! latch), cancel execution, human-only guards, policy checks, and durable
//! control receipts (ledger schema v4 `control_receipts`).
//!
//! Actions are human-only: the TUI adds no privilege the CLI does not have.
//! Every attempt — applied or denied — is recorded as exactly one durable
//! [`ControlReceipt`]. Nothing here rewinds Git or the ledger: pause/resume only
//! set/clear the durable kill latch (resume also removes the `STOP` sentinel);
//! cancel requests **per-execution** cancellation through
//! [`ExecutionApi::cancel`] (a durable `cancel_requested` intent on that
//! execution) and deliberately does **not** touch the global pause latch. No
//! silent rewinds, no schema rewinds.
//!
//! - **Pause** sets the kill latch (stops new claims/dispatch; in-flight work is
//!   not interrupted).
//! - **Resume** clears the latch and removes `STOP` — same semantics as
//!   `agalma resume` — and requires a typed confirmation (`type "resume"`).
//! - **Cancel** cancels one execution and requires a typed confirmation
//!   (`type "cancel"`).
//!
//! TUI keybindings: agalma-3kr.7 — the full-screen composition's key handler
//! calls [`ControlPlane::apply`] with a [`ControlRequest`] and renders the
//! returned [`ControlOutcome`] plus the recorded receipt.

// TUI keybindings: agalma-3kr.7

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::digest::FailureCode;
use agalma_contracts::{
    ContractError, ControlReceipt, ExecutionApi, ExecutionId, ExecutionState, LedgerApi,
};
use serde_json::json;

use crate::stop::FileStopControl;

/// The only actor permitted to drive control actions.
pub const ACTOR_HUMAN: &str = "human";

/// Who requested a control action.
///
/// Actions are human-only. The TUI and CLI both run as [`Actor::Human`];
/// [`Actor::Agent`] exists so the guard is explicit and testable — a worker or
/// agent must never pause, resume, or cancel the factory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// A human operator (TUI or CLI).
    Human,
    /// An automated agent/worker (always rejected by the human-only guard).
    Agent,
}

impl Actor {
    /// Stable wire/ledger string.
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::Human => ACTOR_HUMAN,
            Actor::Agent => "agent",
        }
    }
}

/// One interactive control action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    /// Set the durable kill latch (stops new claims/dispatch; in-flight work is
    /// not interrupted).
    Pause,
    /// Clear the latch and remove the `STOP` sentinel.
    Resume,
    /// Request cancellation of one execution.
    Cancel { execution: ExecutionId },
}

impl ControlAction {
    /// Stable action name (`pause` / `resume` / `cancel`); also the typed
    /// confirmation phrase.
    pub fn name(&self) -> &'static str {
        match self {
            ControlAction::Pause => "pause",
            ControlAction::Resume => "resume",
            ControlAction::Cancel { .. } => "cancel",
        }
    }

    /// Subject acted on: the execution id for `cancel`, empty for latch actions.
    pub fn target(&self) -> String {
        match self {
            ControlAction::Cancel { execution } => execution.to_string(),
            _ => String::new(),
        }
    }

    /// Whether the human must type the action name back. Destructive actions
    /// (resume, cancel) require it; pause does not.
    pub fn requires_confirmation(&self) -> bool {
        matches!(self, ControlAction::Resume | ControlAction::Cancel { .. })
    }
}

/// One control request: what, who, and the typed confirmation collected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    /// Idempotency anchor for the recorded receipt (unique per attempt).
    pub action_id: String,
    pub action: ControlAction,
    pub actor: Actor,
    /// The phrase the human typed; must match [`ControlAction::name`] for
    /// actions that require confirmation.
    pub confirmation: Option<String>,
}

impl ControlRequest {
    /// Build a request with no typed confirmation (callers that need one set it).
    pub fn new(action_id: impl Into<String>, action: ControlAction, actor: Actor) -> Self {
        ControlRequest {
            action_id: action_id.into(),
            action,
            actor,
            confirmation: None,
        }
    }

    /// Set the typed confirmation phrase.
    pub fn with_confirmation(mut self, typed: impl Into<String>) -> Self {
        self.confirmation = Some(typed.into());
        self
    }
}

/// Result of a control attempt. Both variants are receipted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    /// The action was performed.
    Applied,
    /// A guard or policy check rejected the action; nothing was changed.
    Denied { reason: String },
}

impl ControlOutcome {
    /// Whether the action was performed.
    pub fn is_applied(&self) -> bool {
        matches!(self, ControlOutcome::Applied)
    }

    /// Stable outcome string (`applied` / `denied`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ControlOutcome::Applied => "applied",
            ControlOutcome::Denied { .. } => "denied",
        }
    }
}

/// Whether `typed` matches the action's required confirmation phrase.
///
/// Actions that require no confirmation are always considered confirmed.
pub fn confirmed(action: &ControlAction, typed: Option<&str>) -> bool {
    !action.requires_confirmation() || typed.map(str::trim) == Some(action.name())
}

/// Build a locally-unique action id for one CLI/TUI attempt.
///
/// Ids only need to be unique within a state dir; the action name, pid, and
/// millisecond timestamp are ample for a single interactive operator.
pub fn new_action_id(action: &ControlAction) -> String {
    format!("{}-{}-{}", action.name(), std::process::id(), now_ms())
}

/// The cancellation seam: production uses [`agalma_execution::Executor`] through
/// the blanket impl below; tests may substitute a double.
pub trait ExecutionCancel {
    /// Request per-execution cancellation (durable `cancel_requested` intent;
    /// does not set the global pause latch).
    fn request_cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError>;

    /// Current durable state, for the pre-cancel policy check.
    fn execution_state(
        &self,
        execution: &ExecutionId,
    ) -> Result<Option<ExecutionState>, ContractError>;
}

impl<L: LedgerApi> ExecutionCancel for agalma_execution::Executor<L> {
    fn request_cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError> {
        self.cancel(execution)
    }

    fn execution_state(
        &self,
        execution: &ExecutionId,
    ) -> Result<Option<ExecutionState>, ContractError> {
        Ok(self
            .ledger()
            .execution(execution)?
            .map(|record| record.state))
    }
}

/// A no-op [`ExecutionCancel`] for callers that only drive latch actions
/// (pause/resume). Cancelling through it is an `UnsupportedCapability`.
pub struct NoCancel;

impl ExecutionCancel for NoCancel {
    fn request_cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError> {
        Err(ContractError::UnsupportedCapability(format!(
            "no execution-cancel seam bound; cannot cancel {execution}"
        )))
    }

    fn execution_state(
        &self,
        execution: &ExecutionId,
    ) -> Result<Option<ExecutionState>, ContractError> {
        Err(ContractError::UnsupportedCapability(format!(
            "no execution-cancel seam bound; cannot inspect {execution}"
        )))
    }
}

/// Applies control actions over a ledger and a cancellation seam, recording a
/// durable receipt for every attempt.
pub struct ControlPlane<'a, L: LedgerApi, C: ExecutionCancel> {
    ledger: &'a mut L,
    cancel: &'a mut C,
    state_dir: &'a Path,
}

impl<'a, L: LedgerApi, C: ExecutionCancel> ControlPlane<'a, L, C> {
    /// Build a control plane over the bound ledger, cancel seam, and state dir.
    pub fn new(ledger: &'a mut L, cancel: &'a mut C, state_dir: &'a Path) -> Self {
        ControlPlane {
            ledger,
            cancel,
            state_dir,
        }
    }

    /// Apply `request`: enforce the guards/policy, perform the action, and
    /// record exactly one durable receipt regardless of outcome.
    ///
    /// Returns the outcome (`Applied`/`Denied`); `Err` is reserved for an
    /// infrastructure failure (e.g. a receipt write fault), never for a guard or
    /// policy denial — denials are normal, receipted outcomes.
    pub fn apply(&mut self, request: &ControlRequest) -> Result<ControlOutcome, ContractError> {
        let outcome = self.evaluate(request)?;
        self.record(request, &outcome)?;
        Ok(outcome)
    }

    /// Guards + policy check, then perform. Returns the outcome without
    /// recording (the caller records).
    fn evaluate(&mut self, request: &ControlRequest) -> Result<ControlOutcome, ContractError> {
        // Human-only guard: the TUI adds no privilege the CLI lacks.
        if request.actor != Actor::Human {
            return Ok(ControlOutcome::Denied {
                reason: format!(
                    "control actions are human-only (actor={})",
                    request.actor.as_str()
                ),
            });
        }
        // Typed confirmation guard for destructive actions.
        if !confirmed(&request.action, request.confirmation.as_deref()) {
            return Ok(ControlOutcome::Denied {
                reason: format!(
                    "typed confirmation required: type \"{}\"",
                    request.action.name()
                ),
            });
        }
        self.perform(&request.action)
    }

    fn perform(&mut self, action: &ControlAction) -> Result<ControlOutcome, ContractError> {
        match action {
            ControlAction::Pause => {
                if self.ledger.kill_latch()? {
                    return Ok(ControlOutcome::Denied {
                        reason: "already paused (kill latch set)".to_string(),
                    });
                }
                self.ledger.set_kill_latch(true)?;
                Ok(ControlOutcome::Applied)
            }
            ControlAction::Resume => {
                let sentinel = FileStopControl::new(self.state_dir).sentinel_present();
                if !self.ledger.kill_latch()? && !sentinel {
                    return Ok(ControlOutcome::Denied {
                        reason: "not stopped: kill latch clear and no STOP sentinel".to_string(),
                    });
                }
                self.ledger.set_kill_latch(false)?;
                let _ = std::fs::remove_file(self.state_dir.join("STOP"));
                Ok(ControlOutcome::Applied)
            }
            ControlAction::Cancel { execution } => match self.cancel.execution_state(execution)? {
                None => Ok(ControlOutcome::Denied {
                    reason: format!("unknown execution {execution}"),
                }),
                Some(state) if is_terminal(state) => Ok(ControlOutcome::Denied {
                    reason: format!(
                        "execution {execution} is terminal ({state:?}); nothing to cancel"
                    ),
                }),
                Some(_) => {
                    self.cancel.request_cancel(execution)?;
                    Ok(ControlOutcome::Applied)
                }
            },
        }
    }

    /// Record the durable receipt for the attempt (applied or denied).
    fn record(
        &mut self,
        request: &ControlRequest,
        outcome: &ControlOutcome,
    ) -> Result<(), ContractError> {
        let detail = match outcome {
            ControlOutcome::Applied => json!({ "outcome": "applied" }),
            ControlOutcome::Denied { reason } => {
                json!({ "outcome": "denied", "reason": reason })
            }
        };
        self.ledger.record_control_receipt(&ControlReceipt {
            action_id: request.action_id.clone(),
            action: request.action.name().to_string(),
            target: request.action.target(),
            actor: request.actor.as_str().to_string(),
            request: detail,
            recorded_at_unix_ms: now_ms(),
        })
    }
}

/// Whether an execution state is terminal (`completed`/`failed`/`parked`).
fn is_terminal(state: ExecutionState) -> bool {
    matches!(
        state,
        ExecutionState::Completed | ExecutionState::Failed | ExecutionState::Parked
    )
}

/// The human-facing control-plane attachment for advertised plugin actions.
///
/// The TUI/CLI drives plugin actions through this adapter after the HP.6
/// `watch.rs` handoff. It enforces the human-only actor guard (a worker/agent
/// never gains action authority) and forwards to the validated
/// [`crate::plugin_actions::ActionService`], which owns descriptor/grant/binding/
/// input-hash/deadline validation and the declared human-confirmation
/// requirement. It adds no authority the service does not already enforce.
pub struct PluginActionControl<'a> {
    service: &'a mut crate::plugin_actions::ActionService,
}

impl<'a> PluginActionControl<'a> {
    /// Build the attachment over the hub's common action service.
    pub fn new(service: &'a mut crate::plugin_actions::ActionService) -> Self {
        PluginActionControl { service }
    }

    /// Dispatch a human action request. `Actor::Policy` (and any non-human
    /// actor) is refused before the service is consulted (zero effects).
    pub fn dispatch(
        &mut self,
        request: &crate::plugin_actions::ActionRequest,
    ) -> Result<crate::plugin_actions::ActionOutcome, ContractError> {
        // Human-only guard: the UI/CLI add no privilege a worker does not have.
        // Uses the action-service actor type (`agalma_contracts::Actor`), not the
        // receipt actor enum above.
        if request.actor != agalma_contracts::Actor::Human {
            return Err(FailureCode::AuthorityDenied.failure(
                &request.operation_id,
                "plugin actions are human-only through the control plane",
            ));
        }
        self.service.submit(request)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use agalma_contracts::{CommitBatch, ExecutionPhase, ExecutionRecord, LedgerApi, TaskId};
    use agalma_execution::{Executor, ExecutorConfig};
    use agalma_ledger::SqliteLedger;
    use agalma_testkit::ScratchDir;

    fn scratch(name: &str) -> ScratchDir {
        ScratchDir::new(&format!("control-{name}"))
    }

    fn executor(dir: &Path) -> Executor<SqliteLedger> {
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        Executor::new(
            ledger,
            ExecutorConfig {
                max_attempts: 2,
                execution_definition_version: 1,
            },
        )
    }

    /// A second connection to the same ledger (WAL) for plane-side receipt/event
    /// reads while the [`Executor`] holds its own connection.
    fn ledger(dir: &Path) -> SqliteLedger {
        SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger")
    }

    // Pause → latch set + applied receipt; resume without confirmation → denied
    // (latch untouched); resume with the typed phrase → latch cleared + applied.
    #[test]
    fn pause_and_resume_drive_the_latch_and_record_receipts() {
        let dir = scratch("pause-resume");
        let mut ex = executor(&dir);
        let mut cancel = NoCancel;

        // Pause: sets the latch, no typed confirmation required.
        let pause = ControlRequest::new("c1", ControlAction::Pause, Actor::Human);
        {
            let mut plane = ControlPlane::new(ex.ledger_mut(), &mut cancel, &dir);
            assert_eq!(plane.apply(&pause).expect("pause"), ControlOutcome::Applied);
        }
        assert!(ex.ledger().kill_latch().expect("latch"), "paused");

        // Resume without the typed confirmation is denied and changes nothing.
        let resume_no_confirm = ControlRequest::new("c2", ControlAction::Resume, Actor::Human);
        {
            let mut plane = ControlPlane::new(ex.ledger_mut(), &mut cancel, &dir);
            let outcome = plane.apply(&resume_no_confirm).expect("resume");
            assert!(!outcome.is_applied(), "denied without confirmation");
        }
        assert!(ex.ledger().kill_latch().expect("latch"), "still paused");

        // Resume with the typed phrase clears the latch.
        let resume = ControlRequest::new("c3", ControlAction::Resume, Actor::Human)
            .with_confirmation("resume");
        {
            let mut plane = ControlPlane::new(ex.ledger_mut(), &mut cancel, &dir);
            assert_eq!(
                plane.apply(&resume).expect("resume"),
                ControlOutcome::Applied
            );
        }
        assert!(!ex.ledger().kill_latch().expect("latch"), "resumed");

        let receipts = ex.ledger().control_receipts().expect("receipts");
        assert_eq!(receipts.len(), 3, "one receipt per attempt: {receipts:?}");
        let applied = receipts
            .iter()
            .filter(|r| r.request["outcome"] == "applied")
            .count();
        let denied = receipts
            .iter()
            .filter(|r| r.request["outcome"] == "denied")
            .count();
        assert_eq!(applied, 2, "pause + resume applied");
        assert_eq!(denied, 1, "unconfirmed resume denied");
        assert_eq!(receipts[0].action, "pause");
        assert_eq!(receipts[0].actor, ACTOR_HUMAN);
        assert_eq!(receipts[0].target, "");
    }

    // Cancel: the typed confirmation runs the real [`ExecutionApi::cancel`],
    // recording a durable `cancel_requested` intent on that execution and an
    // applied receipt — WITHOUT touching the global pause latch.
    #[test]
    fn cancel_with_confirmation_records_receipt_and_requests_cancellation() {
        let dir = scratch("cancel");
        let mut ex = executor(&dir);
        let task = TaskId::new("control-cancel");
        let execution = ex.start(&task).expect("start execution");

        // The plane records receipts through a second connection to the same
        // ledger while the executor (the cancel seam) holds its own.
        let mut plane_ledger = ledger(&dir);
        let confirm = ControlRequest::new(
            "cancel-2",
            ControlAction::Cancel {
                execution: execution.clone(),
            },
            Actor::Human,
        )
        .with_confirmation("cancel");
        let outcome = {
            let mut plane = ControlPlane::new(&mut plane_ledger, &mut ex, &dir);
            plane.apply(&confirm).expect("cancel")
        };
        assert_eq!(outcome, ControlOutcome::Applied);

        // Cancellation is a durable per-execution intent; the global pause
        // latch is UNCHANGED (pause owns the latch, not cancel).
        assert!(
            !plane_ledger.kill_latch().expect("latch"),
            "cancel must not set the global pause latch"
        );
        let events = plane_ledger.events(&execution).expect("events");
        assert!(
            events.iter().any(|e| e.kind == "cancel_requested"),
            "cancel_requested recorded: {:?}",
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );

        let receipts = plane_ledger.control_receipts().expect("receipts");
        assert_eq!(receipts.len(), 1, "one receipt: {receipts:?}");
        assert_eq!(receipts[0].action, "cancel");
        assert_eq!(receipts[0].target, execution.to_string());
        assert_eq!(receipts[0].request["outcome"], "applied");
    }

    // Human-only guard: an agent actor is rejected and the denial is receipted;
    // no state changes.
    #[test]
    fn agent_actor_is_denied_and_receipted() {
        let dir = scratch("human-only");
        let mut ex = executor(&dir);
        let mut cancel = NoCancel;
        let request = ControlRequest::new("a1", ControlAction::Pause, Actor::Agent);
        let outcome = {
            let mut plane = ControlPlane::new(ex.ledger_mut(), &mut cancel, &dir);
            plane.apply(&request).expect("apply")
        };
        match outcome {
            ControlOutcome::Denied { reason } => {
                assert!(reason.contains("human-only"), "reason: {reason}");
            }
            other => panic!("expected denial, got {other:?}"),
        }
        assert!(!ex.ledger().kill_latch().expect("latch"), "no latch change");
        let receipts = ex.ledger().control_receipts().expect("receipts");
        assert_eq!(receipts.len(), 1, "denial receipted: {receipts:?}");
        assert_eq!(receipts[0].actor, "agent");
        assert_eq!(receipts[0].request["outcome"], "denied");
        assert!(receipts[0].request["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("human-only"));
    }

    // Policy checks: resume when not stopped, and cancel of an unknown or
    // terminal execution, are denied (and receipted).
    #[test]
    fn policy_checks_reject_and_receipt() {
        let dir = scratch("policy");
        let mut ex = executor(&dir);

        // Resume with nothing stopped.
        let resume = ControlRequest::new("p1", ControlAction::Resume, Actor::Human)
            .with_confirmation("resume");
        let outcome = {
            let mut no_cancel = NoCancel;
            let mut plane = ControlPlane::new(ex.ledger_mut(), &mut no_cancel, &dir);
            plane.apply(&resume).expect("resume")
        };
        assert!(!outcome.is_applied(), "resume with nothing stopped denied");

        // Cancel an unknown execution, then a terminal one.
        let terminal = ExecutionId::new("exec:fix:1");
        ex.ledger_mut()
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(ExecutionRecord {
                    execution_id: terminal.clone(),
                    task_id: TaskId::new("control-policy"),
                    generation: 1,
                    phase: ExecutionPhase::Done,
                    state: ExecutionState::Completed,
                    attempt: 1,
                    revision: 0,
                }),
                events: vec![],
                receipts: vec![],
                intents: vec![],
            })
            .expect("commit terminal execution");

        let unknown = ControlRequest::new(
            "p2",
            ControlAction::Cancel {
                execution: ExecutionId::new("exec:missing:1"),
            },
            Actor::Human,
        )
        .with_confirmation("cancel");
        let terminal_req = ControlRequest::new(
            "p3",
            ControlAction::Cancel {
                execution: terminal.clone(),
            },
            Actor::Human,
        )
        .with_confirmation("cancel");

        let mut plane_ledger = ledger(&dir);
        let unknown_outcome = {
            let mut cancel = StateDouble::default();
            let mut plane = ControlPlane::new(&mut plane_ledger, &mut cancel, &dir);
            plane.apply(&unknown).expect("cancel unknown")
        };
        assert!(!unknown_outcome.is_applied(), "unknown execution denied");

        let terminal_outcome = {
            let mut cancel = StateDouble::default();
            cancel
                .states
                .insert(terminal.to_string(), ExecutionState::Completed);
            let mut plane = ControlPlane::new(&mut plane_ledger, &mut cancel, &dir);
            plane.apply(&terminal_req).expect("cancel terminal")
        };
        assert!(!terminal_outcome.is_applied(), "terminal execution denied");

        let receipts = plane_ledger.control_receipts().expect("receipts");
        assert_eq!(receipts.len(), 3, "each denied attempt receipted");
        assert!(receipts.iter().all(|r| r.request["outcome"] == "denied"));
    }

    /// A deterministic cancellation double backed by an explicit state map:
    /// absent id → unknown; present → that state.
    #[derive(Default)]
    struct StateDouble {
        states: HashMap<String, ExecutionState>,
    }

    impl ExecutionCancel for StateDouble {
        fn request_cancel(&mut self, _execution: &ExecutionId) -> Result<(), ContractError> {
            Ok(())
        }

        fn execution_state(
            &self,
            execution: &ExecutionId,
        ) -> Result<Option<ExecutionState>, ContractError> {
            Ok(self.states.get(execution.as_str()).copied())
        }
    }
}
