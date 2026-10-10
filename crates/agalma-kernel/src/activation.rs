//! Staged activation, rollback, and restore mechanics.
//!
//! `docs/b0-kernel.md` §"Upgrade, activation, rollback" and
//! `docs/durable-execution-bootstrap.md` §"Bootstrap sequence" (Kernel
//! activation) fix the safe procedure for swapping the `ExecutionApi`
//! implementation:
//!
//! **Activate:** set latch → settle/reconcile in-flight executions → stage the
//! candidate binding → swap the binding (old active → draining, candidate →
//! active; S0d rebind rules) → schema compatibility check → clear latch →
//! resume.
//!
//! **Rollback:** keep the seed binding and its recovery path; mark the candidate
//! retired, reactivate the seed, leave in-flight work pinned to the binding it
//! started under.
//!
//! **Restore:** check schema compatibility first, then reconcile ledger, bd,
//! Git, and process state through the [`RestoreReconciler`] seam **before**
//! dispatch; an ambiguous domain parks (latch stays set, nothing dispatches).
//!
//! All three are deterministic and side-effect-free outside the [`LedgerApi`]
//! (the seam is a fake in tests), and record durable evidence in `kernel_meta`.

use std::collections::BTreeSet;

use agalma_contracts::{
    BindingRecord, BindingState, ContractError, ExecutionId, ExecutionState, LedgerApi,
};
use serde::{Deserialize, Serialize};

use crate::activity::{settle_pending, ActivityRegistry, OperationResult};
use crate::engine::{KernelEngine, EXPECTED_LEDGER_SCHEMA_VERSION};
use crate::exec_def::{Command, Inputs};

/// Durable `kernel_meta` key prefix for activation evidence.
pub const ACTIVATION_EVIDENCE_PREFIX: &str = "activation_evidence:";
/// Durable `kernel_meta` key prefix for rollback evidence.
pub const ROLLBACK_EVIDENCE_PREFIX: &str = "rollback_evidence:";
/// Durable `kernel_meta` key prefix for restore evidence.
pub const RESTORE_EVIDENCE_PREFIX: &str = "restore_evidence:";

/// One ordered step of the staged-activation procedure (audit trail).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivationStep {
    SetLatch,
    SettleInFlight,
    StageCandidate,
    SwapBinding,
    SchemaCheck,
    ClearLatch,
}

/// Outcome of a staged activation or rollback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationReport {
    /// Ordered steps completed.
    pub steps: Vec<ActivationStep>,
    /// In-flight operations reconciled (external effect already present).
    pub settled_operations: Vec<String>,
    /// Candidate/retired binding id, when one applied.
    pub candidate_binding: Option<String>,
    /// Previously active binding id, when there was one.
    pub previous_binding: Option<String>,
    /// Resulting active binding id for the component api.
    pub active_binding: Option<String>,
    /// Ledger schema version observed during the compatibility check.
    pub schema_version: u32,
}

/// Whether `record` passes the bind gate: every required capability is
/// advertised. (S0d: a binding whose required set is unmet never binds.)
pub fn capability_gate(record: &BindingRecord) -> Result<(), ContractError> {
    if record.required_capabilities.is_subset(&record.capabilities) {
        return Ok(());
    }
    let missing: BTreeSet<_> = record
        .required_capabilities
        .difference(&record.capabilities)
        .cloned()
        .collect();
    Err(ContractError::UnsupportedCapability(format!(
        "binding {} requires unmet capabilities {missing:?}",
        record.binding_id.as_str()
    )))
}

/// Replace `record`'s state.
fn with_state(record: &BindingRecord, state: BindingState) -> BindingRecord {
    let mut record = record.clone();
    record.state = state;
    record
}

fn active_for<L: LedgerApi>(
    engine: &KernelEngine<L>,
    component_api: &str,
) -> Result<Option<BindingRecord>, ContractError> {
    Ok(engine
        .ledger()
        .bindings()?
        .into_iter()
        .find(|b| b.component_api == component_api && b.state == BindingState::Active))
}

/// Set the latch, then reconcile every in-flight operation through its pinned
/// binding. Returns `(settled, blocked)` operation ids. A non-empty `blocked`
/// set means an ambiguous effect could not be reconciled and activation must
/// not proceed.
/// Set (or clear) the global kill latch: the physical conductor latch **and**
/// the durable per-execution latch every non-terminal execution records, so
/// recovered dispatch is blocked at both layers.
fn set_latch<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    latched: bool,
) -> Result<(), ContractError> {
    engine.ledger_mut().set_kill_latch(latched)?;
    for record in engine.ledger().executions()? {
        if matches!(
            record.state,
            ExecutionState::Completed | ExecutionState::Failed | ExecutionState::Parked
        ) {
            continue;
        }
        engine.apply(
            &record.execution_id,
            &Command::KillLatch { latched },
            &Inputs::default(),
        )?;
    }
    Ok(())
}

fn set_latch_and_settle<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    now_unix_ms: u64,
) -> Result<(Vec<String>, Vec<String>), ContractError> {
    set_latch(engine, true)?;
    let mut settled = Vec::new();
    let mut blocked = Vec::new();
    for record in engine.ledger().executions()? {
        for result in settle_pending(engine, registry, &record.execution_id, now_unix_ms)? {
            match result {
                OperationResult::Completed { operation_id, .. } => {
                    settled.push(operation_id.to_string());
                }
                OperationResult::Parked { operation_id, .. } => {
                    blocked.push(operation_id.to_string());
                }
                _ => {}
            }
        }
    }
    Ok((settled, blocked))
}

fn record_evidence<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    prefix: &str,
    key_suffix: &str,
    value: &impl Serialize,
) -> Result<(), ContractError> {
    let encoded = serde_json::to_string(value)
        .map_err(|e| ContractError::KnownFailure(format!("activation evidence json: {e}")))?;
    engine
        .ledger_mut()
        .kernel_meta_set(&format!("{prefix}{key_suffix}"), &encoded)
}

/// Staged activation: safely swap the active binding for `candidate`'s
/// component api.
///
/// The latch is set first so no new dispatch happens during the swap; in-flight
/// operations are reconciled through their pinned binding; the candidate is
/// staged, the previous active binding is marked draining, and the candidate is
/// activated; the schema is checked; the latch is cleared. An in-flight
/// ambiguous effect or an incompatible schema aborts with the latch still set
/// (no dispatch until an operator resolves it).
pub fn staged_activate<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    candidate: &BindingRecord,
    now_unix_ms: u64,
) -> Result<ActivationReport, ContractError> {
    capability_gate(candidate)?;
    let mut steps = vec![ActivationStep::SetLatch];

    let (settled, blocked) = set_latch_and_settle(engine, registry, now_unix_ms)?;
    steps.push(ActivationStep::SettleInFlight);
    if !blocked.is_empty() {
        // Leave the latch set: do not replace a binding over an unresolved
        // in-flight effect.
        return Err(ContractError::UnknownOutcome(format!(
            "activation blocked: in-flight operations could not be reconciled: {blocked:?}"
        )));
    }

    let previous = active_for(engine, &candidate.component_api)?;
    engine
        .ledger_mut()
        .upsert_binding(&with_state(candidate, BindingState::Staged))?;
    steps.push(ActivationStep::StageCandidate);

    if let Some(previous) = &previous {
        engine
            .ledger_mut()
            .upsert_binding(&with_state(previous, BindingState::Draining))?;
    }
    let active = with_state(candidate, BindingState::Active);
    engine.ledger_mut().upsert_binding(&active)?;
    steps.push(ActivationStep::SwapBinding);

    let schema_version = engine.ledger().schema_version()?;
    if schema_version != EXPECTED_LEDGER_SCHEMA_VERSION {
        return Err(ContractError::Conflict(format!(
            "activation blocked: ledger schema {schema_version} != expected \
             {EXPECTED_LEDGER_SCHEMA_VERSION}"
        )));
    }
    steps.push(ActivationStep::SchemaCheck);

    set_latch(engine, false)?;
    steps.push(ActivationStep::ClearLatch);

    let report = ActivationReport {
        steps,
        settled_operations: settled,
        candidate_binding: Some(candidate.binding_id.as_str().to_string()),
        previous_binding: previous.as_ref().map(|b| b.binding_id.as_str().to_string()),
        active_binding: Some(candidate.binding_id.as_str().to_string()),
        schema_version,
    };
    record_evidence(
        engine,
        ACTIVATION_EVIDENCE_PREFIX,
        &candidate.component_api,
        &report,
    )?;
    Ok(report)
}

/// Roll back to `seed`: settle in-flight work, retire the candidate binding, and
/// reactivate the seed binding.
///
/// The seed's recovery path is retained: in-flight operations keep their pinned
/// binding, and the candidate is kept (retired) for reconciliation. Returns the
/// rollback report with the reactivated seed binding.
pub fn rollback<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    registry: &mut ActivityRegistry,
    seed: &BindingRecord,
    now_unix_ms: u64,
) -> Result<ActivationReport, ContractError> {
    capability_gate(seed)?;
    let mut steps = vec![ActivationStep::SetLatch];

    let (settled, blocked) = set_latch_and_settle(engine, registry, now_unix_ms)?;
    steps.push(ActivationStep::SettleInFlight);
    if !blocked.is_empty() {
        return Err(ContractError::UnknownOutcome(format!(
            "rollback blocked: in-flight operations could not be reconciled: {blocked:?}"
        )));
    }

    let current = active_for(engine, &seed.component_api)?;
    let previous_binding = current.as_ref().map(|b| b.binding_id.as_str().to_string());
    // Retire the candidate but keep it for reconciliation of pinned operations.
    if let Some(current) = &current {
        if current.binding_id != seed.binding_id {
            engine
                .ledger_mut()
                .upsert_binding(&with_state(current, BindingState::Retired))?;
        }
    }
    let active = with_state(seed, BindingState::Active);
    engine.ledger_mut().upsert_binding(&active)?;
    steps.push(ActivationStep::SwapBinding);

    let schema_version = engine.ledger().schema_version()?;
    if schema_version != EXPECTED_LEDGER_SCHEMA_VERSION {
        return Err(ContractError::Conflict(format!(
            "rollback blocked: ledger schema {schema_version} != expected \
             {EXPECTED_LEDGER_SCHEMA_VERSION}"
        )));
    }
    steps.push(ActivationStep::SchemaCheck);

    set_latch(engine, false)?;
    steps.push(ActivationStep::ClearLatch);

    let report = ActivationReport {
        steps,
        settled_operations: settled,
        candidate_binding: previous_binding.clone(),
        previous_binding,
        active_binding: Some(seed.binding_id.as_str().to_string()),
        schema_version,
    };
    record_evidence(
        engine,
        ROLLBACK_EVIDENCE_PREFIX,
        &seed.component_api,
        &report,
    )?;
    Ok(report)
}

/// A restore reconciliation domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreDomain {
    Ledger,
    Bd,
    Git,
    Processes,
}

impl RestoreDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            RestoreDomain::Ledger => "ledger",
            RestoreDomain::Bd => "bd",
            RestoreDomain::Git => "git",
            RestoreDomain::Processes => "processes",
        }
    }

    /// All domains, in reconciliation order.
    pub const ALL: [RestoreDomain; 4] = [
        RestoreDomain::Ledger,
        RestoreDomain::Bd,
        RestoreDomain::Git,
        RestoreDomain::Processes,
    ];
}

/// Result of reconciling one restore domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconcileOutcome {
    /// The domain's actual state was established; safe to proceed.
    Reconciled { detail: String },
    /// The domain's state is ambiguous; park rather than dispatch.
    Ambiguous { detail: String },
}

/// The restore seam: establish actual ledger/bd/Git/process state before
/// dispatch.
///
/// The live conductor binds real implementations; recovery tests bind a
/// deterministic fake. A domain that cannot be established returns
/// [`ReconcileOutcome::Ambiguous`], which parks the execution.
pub trait RestoreReconciler {
    fn reconcile(
        &mut self,
        domain: RestoreDomain,
        execution: &ExecutionId,
    ) -> Result<ReconcileOutcome, ContractError>;
}

/// Outcome of a restore procedure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub execution_id: ExecutionId,
    pub schema_version: u32,
    /// `(domain, reconciled)` in reconciliation order, up to the first failure.
    pub outcomes: Vec<(String, bool)>,
    /// True when the restore parked instead of resuming dispatch.
    pub parked: bool,
    pub parked_reason: Option<String>,
}

/// Restore procedure: check schema compatibility, reconcile ledger/bd/Git/
/// processes **before dispatch**, then clear the latch and resume.
///
/// The latch is set for the duration; an incompatible schema or an ambiguous
/// domain parks the execution and leaves the latch set (nothing dispatches on
/// stale or ambiguous external state).
pub fn restore<L: LedgerApi, R: RestoreReconciler>(
    engine: &mut KernelEngine<L>,
    reconciler: &mut R,
    execution_id: &ExecutionId,
    now_unix_ms: u64,
) -> Result<RestoreReport, ContractError> {
    // 1. Schema/version compatibility is checked first.
    let schema_version = engine.ledger().schema_version()?;
    if schema_version != EXPECTED_LEDGER_SCHEMA_VERSION {
        let reason = format!(
            "restore refused: ledger schema {schema_version} != expected \
             {EXPECTED_LEDGER_SCHEMA_VERSION}"
        );
        let report = RestoreReport {
            execution_id: execution_id.clone(),
            schema_version,
            outcomes: Vec::new(),
            parked: true,
            parked_reason: Some(reason.clone()),
        };
        record_evidence(
            engine,
            RESTORE_EVIDENCE_PREFIX,
            execution_id.as_str(),
            &report,
        )?;
        return Ok(report);
    }

    // 2. Block dispatch while reconciling.
    set_latch(engine, true)?;

    // 3. Reconcile every domain before any dispatch.
    let mut outcomes = Vec::new();
    let mut ambiguous = None;
    for domain in RestoreDomain::ALL {
        match reconciler.reconcile(domain, execution_id)? {
            ReconcileOutcome::Reconciled { .. } => {
                outcomes.push((domain.as_str().to_string(), true));
            }
            ReconcileOutcome::Ambiguous { detail } => {
                outcomes.push((domain.as_str().to_string(), false));
                ambiguous = Some(format!(
                    "{} reconciliation ambiguous: {detail}",
                    domain.as_str()
                ));
                break;
            }
        }
    }

    if let Some(reason) = ambiguous {
        // Park; keep the latch set so nothing dispatches on ambiguous state.
        engine.park_blocked(execution_id, &reason, now_unix_ms)?;
        let report = RestoreReport {
            execution_id: execution_id.clone(),
            schema_version,
            outcomes,
            parked: true,
            parked_reason: Some(reason),
        };
        record_evidence(
            engine,
            RESTORE_EVIDENCE_PREFIX,
            execution_id.as_str(),
            &report,
        )?;
        return Ok(report);
    }

    // 4. Reconciled: clear the latch and resume.
    set_latch(engine, false)?;
    let report = RestoreReport {
        execution_id: execution_id.clone(),
        schema_version,
        outcomes,
        parked: false,
        parked_reason: None,
    };
    record_evidence(
        engine,
        RESTORE_EVIDENCE_PREFIX,
        execution_id.as_str(),
        &report,
    )?;
    Ok(report)
}
