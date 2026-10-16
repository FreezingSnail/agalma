//! Kernel engine: history, projection, deterministic replay, and effect execution.
//!
//! The engine owns no state of its own beyond the ledger handle. `apply` loads
//! the current state by replaying history (no I/O beyond reading the event log),
//! runs the pure transition, and executes its effects: each `Commit` is one
//! atomic [`LedgerApi::commit`] (state projection + event + receipts + intents),
//! then the crash hook fires before the `Dispatch` hand-off. Recovery rebuilds
//! state from events alone and compares it with the projection; a mismatch parks
//! rather than trusting the projection.

use agalma_contracts::{
    CancellationSettlement, CancellationState, ContractError, DirectiveRow, DirectiveState,
    DispatchIntent, ExecutionEvent, ExecutionId, LedgerApi, OperationId, OperationReceipt,
    ProcessApi, StopEvidence, TaskId, TimerRecord,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::crash::{
    crash_hook, AFTER_CANCEL_REQUEST, AFTER_DIRECTIVE_COMMIT, AFTER_TIMER_COMMIT,
    AFTER_TRANSITION_COMMIT,
};
use crate::exec_def::{
    decode_event, DirectivePlan, ExecDef, ExecState, OperationState, EXEC_DEF_VERSION,
    KERNEL_VERSION,
};
use crate::hash::content_hash;
use crate::ownership::{
    lease_fence_key, stop_evidence_key, workers_key, Reclaim, StaleLease, TerminationRecord,
    WorkerRecord,
};
use crate::state::KernelState;
use crate::transition::{step, Conflict, Effect};

/// Ledger key holding the persisted execution-definition version.
pub const META_EXEC_DEF_VERSION: &str = "exec_def_version";
/// Ledger key holding the persisted kernel implementation version.
pub const META_KERNEL_VERSION: &str = "kernel_version";

/// Ledger `kernel_meta` key prefix holding the per-execution cancellation
/// settlement JSON (`cancellation_settlement:<execution_id>`).
pub const META_CANCELLATION_SETTLEMENT: &str = "cancellation_settlement:";

/// Physical ledger schema version this kernel build understands.
///
/// Mirrors `agalma_ledger::SCHEMA_VERSION` (the kernel deliberately depends only
/// on the frozen contracts, so it carries its own copy and refuses a mismatch
/// rather than importing an implementation crate). A stored schema that differs
/// parks the kernel: it never auto-migrates and never writes under a schema it
/// does not understand.
pub const EXPECTED_LEDGER_SCHEMA_VERSION: u32 = 4;

/// Outcome of applying one command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplyOutcome {
    /// The resulting reconstructed state.
    pub state: KernelState,
    /// Post-commit dispatch hand-offs the conductor must perform, in order.
    pub dispatches: Vec<DispatchIntent>,
    /// Set when the command was rejected as a conflict (no effects executed).
    pub conflict: Option<Conflict>,
    /// Set when the command was fenced by a superseded lease: the execution did
    /// not advance, no effect ran, and the rejection was recorded.
    pub stale_lease: Option<StaleLease>,
}

/// Outcome of firing one due timer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerFire {
    pub timer_id: String,
    /// True when this call recorded the `TimerFired` event; false when it was
    /// already recorded (idempotent no-op) but the row was marked fired.
    pub applied: bool,
}

/// Durable execution kernel over a [`LedgerApi`].
pub struct KernelEngine<L: LedgerApi> {
    ledger: L,
    exec_def: ExecDef,
    /// Set when the persisted `exec_def_version` is unknown: every load/apply
    /// parks rather than guessing.
    unknown_version: Option<String>,
    /// Set when the ledger's physical schema version is not the one this kernel
    /// understands: every load/apply parks rather than writing under an unknown
    /// schema.
    schema_mismatch: Option<(u32, u32)>,
    /// Optional process-lifecycle seam (component contract). Injected like the
    /// activity registry; the kernel never calls it from the transition, only
    /// from the engine's ownership/cancellation paths. `+ Send` (B1.7) so a
    /// `KernelEngine` can be hosted behind the `ExecutionApi` process boundary
    /// (`register_execution` requires `Send`).
    launcher: Option<Box<dyn ProcessApi + Send>>,
}

impl<L: LedgerApi> KernelEngine<L> {
    /// Open the kernel over `ledger`.
    ///
    /// On first open the kernel records `exec_def_version`/`kernel_version` in
    /// `kernel_meta`. A stored `exec_def_version` the kernel does not know opens
    /// successfully but parks all work (never a guess, never a silent upgrade).
    pub fn open(mut ledger: L) -> Result<Self, ContractError> {
        let schema_version = ledger.schema_version()?;
        let schema_mismatch = (schema_version != EXPECTED_LEDGER_SCHEMA_VERSION)
            .then_some((schema_version, EXPECTED_LEDGER_SCHEMA_VERSION));
        let stored = ledger.kernel_meta_get(META_EXEC_DEF_VERSION)?;
        let unknown_version = match (&stored, schema_mismatch) {
            (Some(version), None) => ExecDef::for_version(version)
                .is_none()
                .then(|| version.clone()),
            _ => None,
        };
        if stored.is_none() && schema_mismatch.is_none() {
            ledger.kernel_meta_set(META_EXEC_DEF_VERSION, EXEC_DEF_VERSION)?;
            ledger.kernel_meta_set(META_KERNEL_VERSION, KERNEL_VERSION)?;
        }
        Ok(KernelEngine {
            ledger,
            exec_def: ExecDef::v1(),
            unknown_version,
            schema_mismatch,
            launcher: None,
        })
    }

    /// Open the kernel, applying an explicit migration for an unknown
    /// `exec_def_version` when `migrations` resolves one to the current
    /// definition.
    ///
    /// `docs/b0-kernel.md` §"Upgrade, activation, rollback": migrations are
    /// explicit; incompatible state with no migration still parks. The
    /// migration runs before any work is reconstructed, and only through the
    /// [`agalma_contracts::LedgerApi`] (no external effects).
    pub fn open_with_migrations(
        mut ledger: L,
        migrations: &crate::upgrade::MigrationRegistry,
    ) -> Result<Self, ContractError> {
        let schema_version = ledger.schema_version()?;
        if schema_version != EXPECTED_LEDGER_SCHEMA_VERSION {
            // A schema mismatch parks; never migrate history under it.
            return KernelEngine::open(ledger);
        }
        if let Some(version) = ledger.kernel_meta_get(META_EXEC_DEF_VERSION)? {
            if ExecDef::for_version(&version).is_none() {
                if let Some(migration) = migrations.resolve(&version, EXEC_DEF_VERSION) {
                    migration.migrate(&mut ledger)?;
                }
            }
        }
        KernelEngine::open(ledger)
    }

    /// Re-read the persisted contract versions and schema after an import.
    fn refresh_incompat(&mut self) -> Result<(), ContractError> {
        let schema_version = self.ledger.schema_version()?;
        self.schema_mismatch = (schema_version != EXPECTED_LEDGER_SCHEMA_VERSION)
            .then_some((schema_version, EXPECTED_LEDGER_SCHEMA_VERSION));
        let stored = self.ledger.kernel_meta_get(META_EXEC_DEF_VERSION)?;
        self.unknown_version = match (&stored, self.schema_mismatch) {
            (Some(version), None) => ExecDef::for_version(version)
                .is_none()
                .then(|| version.clone()),
            _ => None,
        };
        Ok(())
    }

    /// The ledger schema mismatch `(found, expected)`, if the kernel parked on
    /// one.
    pub fn schema_mismatch(&self) -> Option<(u32, u32)> {
        self.schema_mismatch
    }

    /// Export the complete canonical state as a versioned snapshot
    /// (history + projection + receipts + intents + timers + directives +
    /// bindings + contract versions).
    pub fn export_state(&self) -> Result<agalma_contracts::StateSnapshot, ContractError> {
        self.ledger.export_state()
    }

    /// Replace all durable state with `snapshot` (a compatible replacement),
    /// then re-evaluate compatibility. Refuses an incompatible snapshot.
    pub fn import_state(
        &mut self,
        snapshot: &agalma_contracts::StateSnapshot,
    ) -> Result<(), ContractError> {
        self.ledger.import_state(snapshot)?;
        self.refresh_incompat()
    }

    /// Inject the [`ProcessApi`] seam (process-tree termination + evidence).
    ///
    /// Consuming builder: `KernelEngine::open(ledger)?.with_launcher(Box::new(launcher))`.
    /// Without a launcher the engine can still fence stale leases; it cannot
    /// confirm termination of a prior process and parks reclaim instead.
    pub fn with_launcher(mut self, launcher: Box<dyn ProcessApi + Send>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    /// Whether a process-launcher seam is bound.
    pub fn has_launcher(&self) -> bool {
        self.launcher.is_some()
    }

    /// Mutable access to the bound launcher, if any.
    pub fn launcher_mut(&mut self) -> Option<&mut (dyn ProcessApi + Send + 'static)> {
        self.launcher.as_mut().map(|launcher| &mut **launcher)
    }

    /// The resolved execution definition.
    pub fn exec_def(&self) -> &ExecDef {
        &self.exec_def
    }

    /// The persisted execution-definition version, when known.
    pub fn exec_def_version(&self) -> Option<&str> {
        self.unknown_version
            .is_none()
            .then_some(self.exec_def.version())
    }

    /// Borrow the ledger (diagnostics / tests).
    pub fn ledger(&self) -> &L {
        &self.ledger
    }

    /// Mutably borrow the ledger (diagnostics / tests).
    pub fn ledger_mut(&mut self) -> &mut L {
        &mut self.ledger
    }

    /// Reconstruct an execution's state from history alone, then require it to
    /// agree with the stored projection. A mismatch (or unknown `exec_def`
    /// version) yields a parked state, never a guess.
    pub fn load(&self, execution_id: &ExecutionId) -> Result<KernelState, ContractError> {
        if let Some((found, expected)) = self.schema_mismatch {
            return Ok(KernelState::parked(
                execution_id,
                format!(
                    "incompatible ledger schema version {found} (kernel expects {expected}); \
                     parked (no guessing)"
                ),
            ));
        }
        if let Some(version) = &self.unknown_version {
            return Ok(KernelState::parked(
                execution_id,
                format!("unknown exec_def version {version:?}; parked (no guessing)"),
            ));
        }
        let events = self.ledger.events(execution_id)?;
        let replayed = replay(execution_id, &events)?;
        match self.ledger.execution(execution_id)? {
            Some(projection) => {
                if replayed.matches_projection(&projection) {
                    Ok(replayed)
                } else {
                    Ok(replayed.park(format!(
                        "projection mismatch: history/derived does not match executions row \
                         (derived phase={:?} state={:?} attempt={}, projected phase={:?} state={:?} attempt={})",
                        replayed.phase,
                        crate::exec_def::coarse_state(replayed.state),
                        replayed.attempt,
                        projection.phase,
                        projection.state,
                        projection.attempt,
                    )))
                }
            }
            None => Ok(replayed),
        }
    }

    /// Run one command: load, transition, then commit and dispatch.
    ///
    /// Effects are executed in order; the crash hook fires after each transition
    /// commit and before its dispatch, so a crash there is recovered from the
    /// durable intent. A conflict executes nothing.
    pub fn apply(
        &mut self,
        execution_id: &ExecutionId,
        command: &crate::exec_def::Command,
        inputs: &crate::exec_def::Inputs,
    ) -> Result<ApplyOutcome, ContractError> {
        let state = self.load(execution_id)?;
        // Ownership fence: a command carrying a superseded lease cannot advance
        // the task. The rejection is recorded and the effect is dropped.
        if let Some(stale) = self.fence_check(&state, command, inputs)? {
            return Ok(ApplyOutcome {
                state,
                dispatches: Vec::new(),
                conflict: None,
                stale_lease: Some(stale),
            });
        }
        if state.state == crate::exec_def::ExecState::Parked {
            return Ok(ApplyOutcome {
                state,
                dispatches: Vec::new(),
                conflict: None,
                stale_lease: None,
            });
        }
        let result = step(&state, command, inputs);
        if result.conflict.is_some() {
            return Ok(ApplyOutcome {
                state: result.state,
                dispatches: Vec::new(),
                conflict: result.conflict,
                stale_lease: None,
            });
        }
        let mut dispatches = Vec::new();
        for effect in result.effects {
            match effect {
                Effect::Commit(batch) => {
                    self.ledger.commit(batch)?;
                    // Transition commit returned; dispatch has not run.
                    crash_hook(AFTER_TRANSITION_COMMIT);
                }
                Effect::Dispatch(intent) => dispatches.push(intent),
            }
        }

        // Completed/cancelled work cancels its pending timers: a terminal
        // execution must never have a timer fire on its behalf.
        if matches!(
            result.state.state,
            ExecState::Completed | ExecState::Failed | ExecState::Cancelled
        ) {
            self.ledger.cancel_timers_for_execution(execution_id)?;
        }

        // Cancelled operations release their queued dispatch intents and
        // terminate any confined worker, recording cleanup evidence, so a
        // replacement directive or cancel does not leave stale work (or a live
        // process) behind.
        let mut cancelled = Vec::new();
        for (operation_id, record) in &result.state.operations {
            if record.state != OperationState::Cancelled {
                continue;
            }
            let was_cancelled = state.operations.get(operation_id).map(|prior| prior.state)
                == Some(OperationState::Cancelled);
            if !was_cancelled {
                cancelled.push(operation_id.clone());
            }
        }
        for operation_id in cancelled {
            self.ledger.consume_dispatch_intent(&operation_id)?;
            self.terminate_worker_for(execution_id, &operation_id, inputs.now_unix_ms)?;
        }

        Ok(ApplyOutcome {
            state: result.state,
            dispatches,
            conflict: None,
            stale_lease: None,
        })
    }

    /// Durable (unconsumed) dispatch intents — the recovery work list.
    pub fn pending_dispatch(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        self.ledger.pending_intents()
    }

    /// Recorded receipt for a completed operation (idempotency anchor).
    pub fn receipt(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        self.ledger.operation_receipt(operation_id)
    }

    // -----------------------------------------------------------------------
    // Durable waiting: timers, deadlines, and directives
    // -----------------------------------------------------------------------

    /// Schedule a durable retry timer or deadline, atomically.
    ///
    /// The row is durable before it can fire; a crash immediately after the row
    /// commits is recovered by [`KernelEngine::fire_due_timers`]. Returns `true`
    /// when a new row was written; an identical duplicate is a no-op (`false`)
    /// and reusing an id with different contents is a conflict.
    pub fn schedule_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError> {
        let inserted = self.ledger.insert_timer(timer)?;
        if inserted {
            // The row is durable; firing happens on a later wake.
            crash_hook(AFTER_TIMER_COMMIT);
        }
        Ok(inserted)
    }

    /// Pending timers due at or before `now_unix_ms`, ordered by deadline.
    pub fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError> {
        self.ledger.due_timers(now_unix_ms)
    }

    /// Fire every overdue pending timer **exactly once**, in deadline order.
    ///
    /// Each due timer applies a recovered `Command::TimerFired` (idempotent by
    /// timer id: a timer already recorded in history produces no second event
    /// and never advances the phase twice), then the row is marked fired. A
    /// crash between the event commit and the row update is harmless: the next
    /// call re-reads the due row, the transition is a no-op, and the row is
    /// marked fired. A second restart observes the fired row and fires nothing.
    pub fn fire_due_timers(&mut self, now_unix_ms: u64) -> Result<Vec<TimerFire>, ContractError> {
        let due = self.ledger.due_timers(now_unix_ms)?;
        let mut fired = Vec::new();
        for timer in due {
            let content_hash = timer_content_hash(&timer);
            let before = self.load(&timer.execution_id)?;
            let apply = self.apply(
                &timer.execution_id,
                &crate::exec_def::Command::TimerFired {
                    timer_id: timer.timer_id.clone(),
                    content_hash,
                },
                &crate::exec_def::Inputs::default(),
            )?;
            let applied = apply.state.fired_timers.contains_key(&timer.timer_id)
                && !before.fired_timers.contains_key(&timer.timer_id);
            self.ledger.mark_timer_fired(&timer.timer_id)?;
            fired.push(TimerFire {
                timer_id: timer.timer_id,
                applied,
            });
        }
        Ok(fired)
    }

    /// Earliest pending timer deadline (epoch ms), if any.
    ///
    /// The caller's wait contract: when there is no runnable work, sleep until
    /// this deadline (or until external input arrives) and then call
    /// [`KernelEngine::fire_due_timers`]. `None` means no timer is pending.
    pub fn next_deadline(&self) -> Result<Option<u64>, ContractError> {
        self.ledger.next_deadline()
    }

    /// Whether runnable work exists right now: an overdue timer or an
    /// unconsumed dispatch intent.
    ///
    /// A caller with no runnable work must **not** busy-wait; it releases its
    /// agent sessions/workers and sleeps until [`KernelEngine::next_deadline`]
    /// (or external input). The kernel never polls on its own.
    pub fn has_runnable_work(&self, now_unix_ms: u64) -> Result<bool, ContractError> {
        if !self.ledger.due_timers(now_unix_ms)?.is_empty() {
            return Ok(true);
        }
        Ok(!self.ledger.pending_intents()?.is_empty())
    }

    /// Persist a directive work plan, then apply it to `execution_id`.
    ///
    /// Crash-safe: the row is durable before the transition, so a crash after
    /// the row commits but before the transition is recovered by
    /// [`KernelEngine::apply_directive`]. A duplicate plan (same id + version +
    /// contents) is a harmless no-op; a newer version replaces queued work.
    pub fn submit_directive(
        &mut self,
        execution_id: &ExecutionId,
        directive_id: &str,
        version: u64,
        plan: &DirectivePlan,
        now_unix_ms: u64,
    ) -> Result<ApplyOutcome, ContractError> {
        let row = DirectiveRow {
            directive_id: directive_id.to_string(),
            version,
            source: plan.source.clone(),
            state: DirectiveState::Active,
            payload: serde_json::to_value(plan).map_err(json_err)?,
            received_at_unix_ms: now_unix_ms,
        };
        let changed = self.ledger.upsert_directive(&row)?;
        if changed {
            // The directive row is durable; the transition may not be.
            crash_hook(AFTER_DIRECTIVE_COMMIT);
        }
        self.apply_directive(execution_id, directive_id, now_unix_ms)
    }

    /// Apply the durable directive row `directive_id` to `execution_id`.
    ///
    /// Idempotent by `(directive_id, version)`: recovery after a crash between
    /// the row commit and the transition, and a duplicate update, are no-ops. A
    /// missing row is a no-op.
    pub fn apply_directive(
        &mut self,
        execution_id: &ExecutionId,
        directive_id: &str,
        now_unix_ms: u64,
    ) -> Result<ApplyOutcome, ContractError> {
        let Some(row) = self.ledger.directive(directive_id)? else {
            return Ok(ApplyOutcome {
                state: self.load(execution_id)?,
                dispatches: Vec::new(),
                conflict: None,
                stale_lease: None,
            });
        };
        let plan: DirectivePlan = serde_json::from_value(row.payload.clone()).map_err(json_err)?;
        let content_hash = content_hash(&row.payload);
        self.apply(
            execution_id,
            &crate::exec_def::Command::DirectiveUpdate {
                directive_id: directive_id.to_string(),
                version: row.version,
                content_hash,
            },
            &crate::exec_def::Inputs {
                directive: Some(plan),
                now_unix_ms,
                ..crate::exec_def::Inputs::default()
            },
        )
    }

    // -----------------------------------------------------------------------
    // Ownership: leases, workers, fencing, reclaim, cancellation
    // -----------------------------------------------------------------------

    /// Begin a new claim cycle for `task`: increment and persist its lease
    /// generation (starts at `1`). Wraps [`LedgerApi::begin_lease`].
    pub fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError> {
        self.ledger.begin_lease(task)
    }

    /// The task's current lease generation (`0` when never claimed).
    pub fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        self.ledger.lease_generation(task)
    }

    /// Recorded workers for `execution_id`, in insertion order.
    pub fn workers(&self, execution_id: &ExecutionId) -> Result<Vec<WorkerRecord>, ContractError> {
        self.read_json_array(&workers_key(execution_id))
    }

    /// Record (upsert) a confined worker for an operation, durably. The record
    /// carries the lease generation the worker launched under.
    pub fn record_worker(&mut self, record: &WorkerRecord) -> Result<(), ContractError> {
        let key = workers_key(&record.execution_id);
        let mut workers: Vec<WorkerRecord> = self.read_json_array(&key)?;
        workers.retain(|w| w.operation_id != record.operation_id);
        workers.push(record.clone());
        self.write_json_array(&key, &workers)
    }

    /// Forget every recorded worker for `execution_id` (after reconciliation).
    pub fn clear_workers(&mut self, execution_id: &ExecutionId) -> Result<(), ContractError> {
        self.write_json_array(&workers_key(execution_id), &Vec::<WorkerRecord>::new())
    }

    /// Recorded termination evidence for `execution_id`, in order.
    pub fn termination_evidence(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<TerminationRecord>, ContractError> {
        self.read_json_array(&stop_evidence_key(execution_id))
    }

    /// Fenced stale-lease rejections for `task`, in order (audit log).
    pub fn lease_fence_log(&self, task: &TaskId) -> Result<Vec<StaleLease>, ContractError> {
        self.read_json_array(&lease_fence_key(task))
    }

    /// Terminate `record`'s process tree through the launcher and record the
    /// returned [`StopEvidence`]. Errors when no launcher is bound.
    pub fn terminate_worker(
        &mut self,
        record: &WorkerRecord,
        now_unix_ms: u64,
    ) -> Result<TerminationRecord, ContractError> {
        let launcher = self.launcher.as_mut().ok_or_else(|| {
            ContractError::KnownFailure(
                "cannot terminate worker: no process launcher bound".to_string(),
            )
        })?;
        let child = record.child();
        let alive = launcher.alive(&child)?;
        let evidence = if alive {
            launcher.terminate(&child)?
        } else {
            StopEvidence {
                process_group_gone: true,
                detail: "prior process already gone".to_string(),
            }
        };
        let termination = TerminationRecord {
            worker: record.clone(),
            evidence,
            recorded_at_unix_ms: now_unix_ms,
        };
        self.append_termination(&termination)?;
        Ok(termination)
    }

    /// Reclaim `task`: reconcile every prior process of `execution_id`, then
    /// begin a fresh lease and release stale-lease intents.
    ///
    /// Reconciliation runs **before** the new lease is usable: a live survivor
    /// is terminated with recorded evidence; a survivor that cannot be confirmed
    /// gone (no launcher, or termination failed) parks the execution and returns
    /// `blocked_reason` — fresh work is never dispatched over an unconfirmed
    /// process.
    pub fn reclaim(
        &mut self,
        task: &TaskId,
        execution_id: &ExecutionId,
        now_unix_ms: u64,
    ) -> Result<Reclaim, ContractError> {
        let previous_lease = self.ledger.lease_generation(task)?;
        let workers = self.workers(execution_id)?;

        // An unconfirmed survivor parks: reclaim requires evidence.
        if !workers.is_empty() && self.launcher.is_none() {
            let reason = format!(
                "reclaim blocked: cannot confirm termination of {} prior worker(s); \
                 no process launcher bound",
                workers.len()
            );
            self.park_blocked(execution_id, &reason, now_unix_ms)?;
            return Ok(Reclaim {
                previous_lease,
                new_lease: previous_lease,
                terminations: Vec::new(),
                settled_intents: Vec::new(),
                blocked_reason: Some(reason),
            });
        }

        let mut terminations = Vec::new();
        for worker in &workers {
            let termination = self.terminate_worker(worker, now_unix_ms)?;
            if !termination.evidence.process_group_gone {
                let reason = format!(
                    "reclaim blocked: termination of worker pid {} unconfirmed ({})",
                    worker.pid, termination.evidence.detail
                );
                self.park_blocked(execution_id, &reason, now_unix_ms)?;
                return Ok(Reclaim {
                    previous_lease,
                    new_lease: previous_lease,
                    terminations,
                    settled_intents: Vec::new(),
                    blocked_reason: Some(reason),
                });
            }
            terminations.push(termination);
        }
        self.clear_workers(execution_id)?;

        // Only now is a fresh lease usable.
        let new_lease = self.ledger.begin_lease(task)?;

        // Release stale-lease intents so a superseded worker cannot redispatch.
        let mut settled_intents = Vec::new();
        for intent in self.ledger.pending_intents()? {
            if &intent.execution_id != execution_id {
                continue;
            }
            let lease = intent
                .payload
                .get("lease")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            if lease < new_lease {
                self.ledger.consume_dispatch_intent(&intent.operation_id)?;
                settled_intents.push(intent.operation_id);
            }
        }

        Ok(Reclaim {
            previous_lease,
            new_lease,
            terminations,
            settled_intents,
            blocked_reason: None,
        })
    }

    /// Persist a cancellation request as a kernel command (SF.4).
    ///
    /// Idempotent and terminal-aware. The durable settlement record is written
    /// `Requested` **before** the `Cancel` transition/cleanup (intent precedes
    /// effects), then updated to `Settled`/`Settling` from the recorded
    /// termination evidence. A second cancel, or a cancel of already-terminal
    /// work, is a harmless no-op: the existing settlement is returned as-is.
    pub fn cancel(
        &mut self,
        execution_id: &ExecutionId,
        now_unix_ms: u64,
    ) -> Result<ApplyOutcome, ContractError> {
        // Idempotent: an existing settlement changes nothing.
        if self.cancellation_settlement(execution_id)?.is_some() {
            return Ok(ApplyOutcome {
                state: self.load(execution_id)?,
                dispatches: Vec::new(),
                conflict: None,
                stale_lease: None,
            });
        }
        let pre = self.load(execution_id)?;
        let settlement = CancellationSettlement {
            execution_id: execution_id.clone(),
            state: CancellationState::Requested,
            request_operation_id: format!("{execution_id}/cancel"),
            requested_at_unix_ms: now_unix_ms,
            settled_at_unix_ms: None,
            unconfirmed_operations: Vec::new(),
        };
        // Durable intent BEFORE the transition/cleanup runs.
        self.write_settlement(&settlement)?;
        // A crash here (intent durable, `Cancel` not yet applied) is recovered
        // by `settle_cancellation`, which replays the transition and consumes
        // the pending intents itself.
        crash_hook(AFTER_CANCEL_REQUEST);

        let outcome = self.apply(
            execution_id,
            &crate::exec_def::Command::Cancel,
            &crate::exec_def::Inputs {
                now_unix_ms,
                ..crate::exec_def::Inputs::default()
            },
        )?;

        let mut next = settlement;
        if pre.is_terminal() {
            // Completed/terminal-before-cancel work keeps its recorded outcome;
            // cancellation is a no-op receipt only.
            next.state = CancellationState::Settled;
            next.settled_at_unix_ms = Some(now_unix_ms);
            self.write_settlement(&next)?;
            return Ok(outcome);
        }

        // The post-commit cleanup consumed cancelled intents and recorded
        // termination evidence. Confirm each recorded worker/operation.
        let unconfirmed = self.unconfirmed_operations(execution_id)?;
        next.unconfirmed_operations = unconfirmed;
        if next.unconfirmed_operations.is_empty() {
            next.state = CancellationState::Settled;
            next.settled_at_unix_ms = Some(now_unix_ms);
        } else {
            next.state = CancellationState::Settling;
        }
        self.write_settlement(&next)?;
        Ok(outcome)
    }

    /// The durable cancellation settlement for `execution_id`, if recorded.
    pub fn cancellation_settlement(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Option<CancellationSettlement>, ContractError> {
        let key = settlement_key(execution_id);
        match self.ledger.kernel_meta_get(&key)? {
            Some(raw) => {
                let parsed: CancellationSettlement = serde_json::from_str(&raw).map_err(|err| {
                    ContractError::KnownFailure(format!("cancellation settlement {key}: {err}"))
                })?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    /// Finish a pending cancellation settlement (boot/recovery).
    ///
    /// Loads the settlement; an already-`Settled` record is returned as-is.
    /// Otherwise it **replays the `Cancel` transition first**: a crash may have
    /// journaled the durable request (`Requested`) before the transition
    /// committed, or committed `Cancelled` but crashed before its post-commit
    /// cleanup. `Cancel` is idempotent (a terminal execution is a no-op), so
    /// this either applies the cancellation now or converges an
    /// already-cancelled row. It then re-runs the terminate pass over any
    /// still-recorded worker and **accounts for every dispatch intent still
    /// pending** for the execution: an already-completed or cancelled
    /// operation's intent is consumed (never dispatched), and any intent that
    /// cannot be accounted for keeps the record `Settling`. A `Settled` record
    /// therefore never leaves a dispatchable pending intent.
    pub fn settle_cancellation(
        &mut self,
        execution_id: &ExecutionId,
        now_unix_ms: u64,
    ) -> Result<Option<CancellationSettlement>, ContractError> {
        let Some(mut settlement) = self.cancellation_settlement(execution_id)? else {
            return Ok(None);
        };
        if settlement.state == CancellationState::Settled {
            return Ok(Some(settlement));
        }

        // 1. Replay cancellation. For a crashed-before-apply request this
        //    transitions the execution to `Cancelled` and consumes its
        //    cancelled operations' intents; for an already-committed cancel it
        //    is a terminal no-op.
        self.apply(
            execution_id,
            &crate::exec_def::Command::Cancel,
            &crate::exec_def::Inputs {
                now_unix_ms,
                ..crate::exec_def::Inputs::default()
            },
        )?;

        // 2. Re-run the terminate pass over recorded workers (a launcher that is
        //    now bound can confirm termination a launcher-less cancel could not).
        for worker in self.workers(execution_id)? {
            self.terminate_worker_for(execution_id, &worker.operation_id, now_unix_ms)?;
        }

        // 3. Account for every pending dispatch intent before settlement.
        let mut unconfirmed = self.settle_cancelled_intents(execution_id)?;
        for operation in self.unconfirmed_operations(execution_id)? {
            if !unconfirmed.contains(&operation) {
                unconfirmed.push(operation);
            }
        }
        unconfirmed.sort();
        unconfirmed.dedup();

        settlement.unconfirmed_operations = unconfirmed.clone();
        if unconfirmed.is_empty() {
            settlement.state = CancellationState::Settled;
            settlement.settled_at_unix_ms = Some(now_unix_ms);
        } else {
            settlement.state = CancellationState::Settling;
        }
        self.write_settlement(&settlement)?;
        Ok(Some(settlement))
    }

    /// Consume the dispatch intents that cancellation has accounted for and
    /// return the operations whose pending intent cannot be settled.
    ///
    /// A recorded receipt means the effect already landed (never repeated); a
    /// cancelled operation — or any terminal execution — must never dispatch, so
    /// its queued intent is dropped. An intent for a still-live, non-cancelled
    /// operation in a non-terminal execution is left pending and reported
    /// unconfirmed, so the settlement can never claim `Settled` over it.
    fn settle_cancelled_intents(
        &mut self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<OperationId>, ContractError> {
        let state = self.load(execution_id)?;
        let mut unconfirmed = Vec::new();
        for intent in self.pending_dispatch()? {
            if &intent.execution_id != execution_id {
                continue;
            }
            let operation_cancelled = state
                .operations
                .get(&intent.operation_id)
                .map(|record| record.state == OperationState::Cancelled)
                .unwrap_or(false);
            let accounted = operation_cancelled
                || state.is_terminal()
                || self.receipt(&intent.operation_id)?.is_some();
            if accounted {
                self.ledger.consume_dispatch_intent(&intent.operation_id)?;
            } else {
                unconfirmed.push(intent.operation_id.clone());
            }
        }
        Ok(unconfirmed)
    }

    /// Operations of `execution_id` whose outcome is unconfirmed: any recorded
    /// termination whose process group was not confirmed gone, plus any still
    /// recorded worker.
    fn unconfirmed_operations(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<OperationId>, ContractError> {
        let mut out: Vec<OperationId> = Vec::new();
        for term in self.termination_evidence(execution_id)? {
            if !term.evidence.process_group_gone && !out.contains(&term.worker.operation_id) {
                out.push(term.worker.operation_id.clone());
            }
        }
        for worker in self.workers(execution_id)? {
            if !out.contains(&worker.operation_id) {
                out.push(worker.operation_id.clone());
            }
        }
        Ok(out)
    }

    fn write_settlement(
        &mut self,
        settlement: &CancellationSettlement,
    ) -> Result<(), ContractError> {
        let key = settlement_key(&settlement.execution_id);
        let raw = serde_json::to_string(settlement).map_err(json_err)?;
        self.ledger.kernel_meta_set(&key, &raw)
    }

    /// Ownership fence: `Some(stale)` when `inputs` carries a lease older than
    /// the task's current lease. Records the rejection durably.
    fn fence_check(
        &mut self,
        state: &KernelState,
        command: &crate::exec_def::Command,
        inputs: &crate::exec_def::Inputs,
    ) -> Result<Option<StaleLease>, ContractError> {
        if inputs.lease_generation == 0 || state.task_id.as_str().is_empty() {
            return Ok(None);
        }
        let current = self.ledger.lease_generation(&state.task_id)?;
        if inputs.lease_generation >= current {
            return Ok(None);
        }
        let stale = StaleLease {
            id: stale_id(command),
            lease: inputs.lease_generation,
            current,
            recorded_at_unix_ms: inputs.now_unix_ms,
        };
        let key = lease_fence_key(&state.task_id);
        let mut log: Vec<StaleLease> = self.read_json_array(&key)?;
        log.push(stale.clone());
        self.write_json_array(&key, &log)?;
        Ok(Some(stale))
    }

    /// Terminate the recorded worker (if any) for `operation_id`, recording
    /// evidence. Without a launcher the termination is recorded as unconfirmed.
    fn terminate_worker_for(
        &mut self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
        now_unix_ms: u64,
    ) -> Result<(), ContractError> {
        let Some(worker) = self
            .workers(execution_id)?
            .into_iter()
            .find(|w| &w.operation_id == operation_id)
        else {
            return Ok(());
        };
        if let Some(launcher) = self.launcher.as_mut() {
            let child = worker.child();
            let alive = launcher.alive(&child)?;
            let evidence = if alive {
                launcher.terminate(&child)?
            } else {
                StopEvidence {
                    process_group_gone: true,
                    detail: "worker already gone".to_string(),
                }
            };
            self.append_termination(&TerminationRecord {
                worker: worker.clone(),
                evidence,
                recorded_at_unix_ms: now_unix_ms,
            })?;
        } else {
            self.append_termination(&TerminationRecord {
                worker: worker.clone(),
                evidence: StopEvidence {
                    process_group_gone: false,
                    detail: "no process launcher bound; termination unconfirmed".to_string(),
                },
                recorded_at_unix_ms: now_unix_ms,
            })?;
        }
        // The worker's operation is cancelled: drop its record.
        let key = workers_key(execution_id);
        let mut workers: Vec<WorkerRecord> = self.read_json_array(&key)?;
        workers.retain(|w| &w.operation_id != operation_id);
        self.write_json_array(&key, &workers)
    }

    fn append_termination(&mut self, termination: &TerminationRecord) -> Result<(), ContractError> {
        let key = stop_evidence_key(&termination.worker.execution_id);
        let mut log: Vec<TerminationRecord> = self.read_json_array(&key)?;
        log.push(termination.clone());
        self.write_json_array(&key, &log)
    }

    /// Park an execution whose reclaim could not confirm termination, durably.
    ///
    /// Parks via the ambiguous-effect path (no probe evidence) so the durable
    /// projection and history stay in agreement; nothing dispatches.
    pub(crate) fn park_blocked(
        &mut self,
        execution_id: &ExecutionId,
        reason: &str,
        now_unix_ms: u64,
    ) -> Result<(), ContractError> {
        let state = self.load(execution_id)?;
        if state.is_terminal() {
            return Ok(());
        }
        // A pending/completed operation is the best anchor; otherwise park via
        // the start-anchor: an unknown-probe delivery on the first operation.
        let anchor = state
            .operations
            .iter()
            .find(|(_, r)| r.state == OperationState::Dispatched)
            .or_else(|| state.operations.iter().next())
            .map(|(id, r)| (id.clone(), r.inputs_hash.clone()));
        let Some((operation_id, inputs_hash)) = anchor else {
            return Ok(());
        };
        let outcome = self.apply(
            execution_id,
            &crate::exec_def::Command::Deliver {
                operation_id,
                inputs_hash,
            },
            &crate::exec_def::Inputs {
                probe: crate::exec_def::Probe::Unknown,
                now_unix_ms,
                ..crate::exec_def::Inputs::default()
            },
        )?;
        if outcome.state.parked_reason.is_none() {
            // Could not park via the transition; record the reason in kernel_meta
            // so the block is not lost across restart.
            self.ledger.kernel_meta_set(
                &format!("reclaim_blocked:{}", execution_id.as_str()),
                reason,
            )?;
        }
        Ok(())
    }

    fn read_json_array<T: DeserializeOwned>(&self, key: &str) -> Result<Vec<T>, ContractError> {
        match self.ledger.kernel_meta_get(key)? {
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|err| ContractError::KnownFailure(format!("kernel meta {key}: {err}"))),
            None => Ok(Vec::new()),
        }
    }

    fn write_json_array<T: Serialize>(
        &mut self,
        key: &str,
        items: &[T],
    ) -> Result<(), ContractError> {
        let raw = serde_json::to_string(items)
            .map_err(|err| ContractError::KnownFailure(format!("kernel meta {key}: {err}")))?;
        self.ledger.kernel_meta_set(key, &raw)
    }
}

/// The `kernel_meta` key holding an execution's cancellation settlement.
fn settlement_key(execution_id: &ExecutionId) -> String {
    format!("{META_CANCELLATION_SETTLEMENT}{execution_id}")
}

/// The signal id a command carries, for fence evidence.
fn stale_id(command: &crate::exec_def::Command) -> String {
    use crate::exec_def::Command as C;
    match command {
        C::Deliver { operation_id, .. } | C::Decision { operation_id, .. } => {
            operation_id.to_string()
        }
        C::Retry { operation_id } => operation_id.to_string(),
        C::TimerFired { timer_id, .. } => timer_id.clone(),
        C::DirectiveUpdate { directive_id, .. } => directive_id.clone(),
        C::Start { task_id, .. } => task_id.to_string(),
        C::Cancel => "cancel".to_string(),
        C::KillLatch { .. } => "kill_latch".to_string(),
    }
}

/// Reconstruct state from history alone by replaying recorded commands through
/// the pure transition. Performs no external effects.
pub fn replay(
    execution_id: &ExecutionId,
    events: &[ExecutionEvent],
) -> Result<KernelState, ContractError> {
    let mut state = KernelState::unstarted(execution_id);
    for event in events {
        let recorded = decode_event(&event.payload).map_err(|err| {
            ContractError::KnownFailure(format!(
                "kernel history: undecodable event #{}-{}: {err}",
                event.sequence, event.kind
            ))
        })?;
        let result = step(&state, &recorded.command, &recorded.inputs);
        state = result.state;
        // The ledger owns the monotonic sequence; the projection revision
        // advances once per recorded event.
        state.revision = event.sequence;
    }
    Ok(state)
}

/// Deterministic content hash pinning a timer's schedule (conflict detector).
fn timer_content_hash(timer: &TimerRecord) -> String {
    content_hash(&serde_json::json!({
        "timer_id": timer.timer_id,
        "execution_id": timer.execution_id.as_str(),
        "kind": timer.kind,
        "fire_at": timer.fire_at_unix_ms,
        "payload": timer.payload,
    }))
}

fn json_err(e: serde_json::Error) -> ContractError {
    ContractError::KnownFailure(format!("kernel json: {e}"))
}
