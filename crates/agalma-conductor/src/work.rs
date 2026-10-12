//! Queue-driven work loop (M1, `agalma-52k.3`).
//!
//! `agalma work --repo <path> [--once] [--max-tasks N] [--state-dir] [--model]`
//! turns the M0 single-task conductor into a loop over the `bd` backlog:
//!
//! 1. **intake** — [`TaskQueueHandle::ready_tasks`] lists eligible tasks; when more
//!    than one is eligible the pinned static `triage.pick-next` decision picks
//!    one (a single eligible task needs no decision call). The task is claimed
//!    atomically and its execution identity `exec:<task>:<generation>` is
//!    recorded in the ledger together with the lease generation *before*
//!    dispatch.
//! 2. **phases** — the same M0 phase pipeline, in **repo mode**: checkout clones
//!    the origin (`prepare_repo`), build runs the confined builder session with a
//!    per-attempt prompt, verify runs the task's acceptance commands (each its own
//!    launch), integrate CAS-merges the candidate into the origin and tags it.
//! 3. **projection** — every phase transition appends a `bd comment`; a terminal
//!    `done` closes the issue, `parked` sets status `blocked` and adds the
//!    `parked` label plus a reason.
//!
//! The loop keeps the M0 controls: the SIGTERM latch, the `STOP` sentinel, boot
//! recovery ordering, and `--once` for tests. M1.6 adds **fenced leases** (a
//! monotonic per-task lease generation recorded with the claim and carried by
//! dispatch intents, receipts, and worker records; stale-lease operations are
//! refused before they can produce an effect), **expiry recovery** (already
//! landed merges complete without re-merging, live survivors are terminated with
//! evidence, ambiguous effects park), and a **ledger-vs-projection reconcile**
//! run on boot and via `agalma reconcile`. The verifier diagnosis is M1.3.

use std::cell::RefCell;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;

use agalma_contracts::{
    ClaimEvidence, CommitBatch, ContractError, DecisionApi, DecisionAttempt, DecisionKind,
    DecisionOption, DecisionOutcome, DecisionPin, DecisionRequest, ExecutionApi, ExecutionEvent,
    ExecutionId, ExecutionPhase, ExecutionState, LedgerApi, OperationId, StepOutcome, Task, TaskId,
    TaskPriority, TaskQueueApi, DECISION_API_VERSION,
};
use agalma_decision::{StaticDecider, TRIAGE_PICK_NEXT_BASELINE};
use agalma_execution::{
    Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION, LEDGER_SCHEMA_VERSION,
};
use agalma_sandbox::PRODUCT_PROFILE;
use agalma_taskqueue::{BdTaskQueue, PARKED_LABEL};
use serde_json::json;

use agalma_ipc::TaskQueueOps;

use crate::cli::WorkArgs;
use crate::composition::{toolchain, BUILDER_PROMPT, DEFAULT_TURN_TIMEOUT, DEFAULT_VERIFY_TIMEOUT};
use crate::config::resolve_state_dir;
use crate::harness_binding::{harness_handle, resolve_selection, HarnessHandle};
use crate::ledger_process::{LedgerHandle, LedgerTransport};
use crate::phases::{activities_with_mode, ActivityMode, PhaseDeps, RepoMode, RunState};
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::routing::{self, RouteSelection};
use crate::taskqueue_process::{TaskQueueHandle, TaskQueueTransport};
use crate::workspace_process::{WorkspaceHandle, WorkspaceTransport};
use crate::LockGuard;

/// Terminal outcome of driving one task's execution.
#[derive(Clone, Debug)]
enum Terminal {
    /// The task completed and its candidate integrated.
    Done,
    /// The task parked with a reason.
    Parked(String),
    /// Dispatch was blocked (kill latch / STOP sentinel).
    Blocked,
}

/// Run the queue-driven work loop; map failures to a non-zero exit.
pub fn run_work(args: WorkArgs) -> ExitCode {
    match work(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("agalma: work failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn work(args: WorkArgs) -> Result<ExitCode, ContractError> {
    let state_dir = resolve_state_dir(args.state_dir.as_deref())
        .map_err(|e| ContractError::KnownFailure(e.to_string()))?;
    // H3 routing: `--model`/`AGALMA_MODEL` override every class; otherwise the
    // static cheapest-adequate route is selected (free-only unless
    // `AGALMA_ALLOW_PAID=1`).
    let override_model = routing::explicit_model(args.model.as_deref());
    std::fs::create_dir_all(&state_dir).map_err(|e| {
        ContractError::KnownFailure(format!(
            "cannot create state dir {}: {e}",
            state_dir.display()
        ))
    })?;
    let state_dir = std::fs::canonicalize(&state_dir).unwrap_or(state_dir);

    let repo = std::fs::canonicalize(&args.repo)
        .map_err(|e| ContractError::KnownFailure(format!("repo {}: {e}", args.repo.display())))?;

    let _lock = LockGuard::acquire(&state_dir).map_err(ContractError::KnownFailure)?;

    let ledger_path = state_dir.join("ledger.sqlite");
    let ledger = LedgerHandle::from_transport(&state_dir, LedgerTransport::from_env())?;
    {
        let version = ledger.schema_version()?;
        if version != LEDGER_SCHEMA_VERSION {
            return Err(ContractError::KnownFailure(format!(
                "incompatible ledger schema {version} (expected {LEDGER_SCHEMA_VERSION}) at {}",
                ledger_path.display()
            )));
        }
    }

    crate::install_signal_handler(state_dir.clone(), LedgerTransport::from_env());

    let mode = ActivityMode::from_env();
    let proxy = if mode == ActivityMode::Live {
        Some(
            ProviderProxy::start(ProxyConfig::from_env())
                .map_err(|e| ContractError::KnownFailure(format!("provider proxy: {e}")))?,
        )
    } else {
        None
    };
    let proxy_url = proxy.as_ref().map(ProviderProxy::url);
    // Paid credential state captured at proxy start. Record the audit event
    // (token-free) and use presence to gate paid route eligibility.
    let paid_available = proxy
        .as_ref()
        .map(|proxy| {
            let auth = proxy.auth_state();
            if let Err(err) = crate::provider_auth::record_audit(&state_dir, &auth) {
                eprintln!(
                    "agalma: provider_auth audit for {}: {err}",
                    state_dir.display()
                );
            } else if auth.present {
                eprintln!(
                    "agalma: paid provider credential present (host={}, fingerprint={})",
                    auth.host,
                    auth.fingerprint.as_deref().unwrap_or("-")
                );
            } else {
                eprintln!(
                    "agalma: paid provider credential absent ({}); paid routes disabled",
                    auth.reason.as_deref().unwrap_or("unknown")
                );
            }
            auth.present
        })
        .unwrap_or(false);

    let mut queue =
        TaskQueueHandle::from_transport(&state_dir, &repo, TaskQueueTransport::from_env())?;
    let mut decider = StaticDecider::new(ledger.clone());

    // Harness binding selection (M1.7): `--harness` flag or `AGALMA_HARNESS`.
    // Resolved once per process; each operation pins its binding at start.
    let harness = harness_handle(resolve_selection(args.harness.as_deref())?);

    // Boot recovery: drive any non-terminal execution to a terminal state before
    // admitting new work (recovery ordering).
    recover_inflight(
        &repo,
        &state_dir,
        &ledger,
        override_model.as_deref(),
        paid_available,
        proxy_url.clone(),
        &mut queue,
        harness.clone(),
    )?;

    // Boot reap sweep: after recovery, delete the heavy scratch of every
    // terminal execution still on disk (crash leftovers and pre-fix state
    // dirs). Best-effort, idempotent.
    crate::reap::reap_terminal(&state_dir, &ledger);

    // Boot projection reconcile: rewrite the bd projection to match the ledger
    // (ledger wins) before admitting new work. Best-effort: a reconcile failure
    // must not block the work loop.
    match crate::reconcile::reconcile_projection(&ledger, &mut queue) {
        Ok(summary) => eprintln!("agalma: {}", summary.render()),
        Err(err) => eprintln!("agalma: boot reconcile failed: {err}"),
    }

    // Epic sweep (CP.3): before ordinary claiming, run one serial designer
    // session per candidate epic and project the decomposition gate. Best-effort:
    // a sweep failure must not block the work loop, and an unavailable planner
    // route skips the sweep (never a silent downgrade). The sweep applies the
    // stop policy (STOP sentinel or kill latch) before any planning dispatch
    // (`agalma-r2l`).
    match crate::sweep::run_sweep(
        &repo,
        &state_dir,
        paid_available,
        proxy_url.clone(),
        override_model.as_deref(),
    ) {
        Ok(report) if report.skipped_reason.is_none() => eprintln!(
            "agalma: epic sweep: {} candidate(s), {} approved, {} rejected, {} escalated",
            report.epics_seen.len(),
            report.approved.len(),
            report.rejected.len(),
            report.escalated.len()
        ),
        Ok(_) => {}
        Err(err) => eprintln!("agalma: epic sweep failed: {err}"),
    }

    // Per-epic review gate (CP.5): after an epic's last child lands, run one
    // reviewer session over the merged diff vs the epic design/acceptance. A
    // drift verdict files a P1 `drift-fix` task and holds ordinary dispatch.
    // Best-effort: a review failure must not block the loop, and an unavailable
    // strong route skips the gate (never a silent pass or downgrade).
    match crate::review::run_review(
        &repo,
        &state_dir,
        paid_available,
        proxy_url.clone(),
        override_model.as_deref(),
    ) {
        Ok(report) if report.skipped_reason.is_none() => eprintln!(
            "agalma: review gate: {} epic(s) reviewed, {} approved, {} drift, {} escalated",
            report.epics_seen.len(),
            report.approved.len(),
            report.drift.len(),
            report.escalated.len()
        ),
        Ok(_) => {}
        Err(err) => eprintln!("agalma: review gate failed: {err}"),
    }

    // Hold ordinary dispatch while any epic's review drift is unresolved: the
    // drift-fix task is rework, and dispatch resumes only once a later review
    // approves (or a human clears the escalation). Best-effort check.
    match crate::review::dispatch_hold(&BdTaskQueue::new(&repo)) {
        Ok(Some(epic)) => {
            eprintln!(
                "agalma: ordinary dispatch held: epic {epic} has an unresolved review drift \
                 (drift-fix pending); no ordinary task will be claimed"
            );
            return Ok(ExitCode::SUCCESS);
        }
        Ok(None) => {}
        Err(err) => eprintln!("agalma: review hold check failed: {err}"),
    }

    let mut processed: u32 = 0;
    let mut last: Option<Terminal> = None;
    loop {
        // Stop policy before any claim/dispatch: the `STOP` sentinel OR the
        // durable kill latch halts the loop before it can mutate the backlog
        // (`agalma-r2l`). The latch is never cleared here; only the human
        // `agalma resume` flow clears it.
        if state_dir.join("STOP").exists() || ledger.kill_latch()? {
            eprintln!(
                "agalma: stop policy active (STOP sentinel or kill latch); halting work loop"
            );
            break;
        }
        if let Some(max) = args.max_tasks {
            if processed >= max {
                break;
            }
        }
        let ready = queue.ready_tasks()?;
        if ready.tasks.is_empty() {
            break;
        }
        let task = if ready.tasks.len() == 1 {
            ready.tasks[0].clone()
        } else {
            select_task(&ready.tasks, &mut decider)?
        };

        let evidence = queue.claim(&task.task_id)?;
        let terminal = run_task(
            &repo,
            &state_dir,
            &ledger,
            override_model.as_deref(),
            paid_available,
            proxy_url.clone(),
            &task,
            &evidence,
            &mut queue,
            harness.clone(),
        )?;
        processed += 1;
        // CP.5: the first child of an epic to land records the epic's review
        // baseline (`review-start:<HEAD>`) so the post-merge review has a diff
        // anchor. Best-effort; a task with no parent epic is a no-op.
        if matches!(terminal, Terminal::Done) {
            if let Err(err) = crate::review::record_start_on_land(&repo, task.task_id.as_str()) {
                eprintln!(
                    "agalma: review baseline record for {}: {err}",
                    task.task_id.as_str()
                );
            }
        }
        last = Some(terminal);
        if args.once {
            break;
        }
    }

    if args.once {
        Ok(match last {
            Some(Terminal::Done) | None => ExitCode::SUCCESS,
            Some(Terminal::Parked(_)) | Some(Terminal::Blocked) => ExitCode::FAILURE,
        })
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Pick one task from an eligible set via the pinned static `triage.pick-next`
/// decision. The operation ID is stable for a given eligible set so recovery
/// reuses a recorded outcome.
fn select_task(
    tasks: &[Task],
    decider: &mut StaticDecider<LedgerHandle>,
) -> Result<Task, ContractError> {
    let mut ids: Vec<String> = tasks
        .iter()
        .map(|t| t.task_id.as_str().to_string())
        .collect();
    ids.sort();
    let operation = OperationId::new(format!("op:triage:{}", short_hash(&ids.join(","))));
    let options: Vec<DecisionOption> = tasks
        .iter()
        .map(|task| DecisionOption {
            option_id: task.task_id.as_str().to_string(),
            attributes: json!({ "priority": priority_weight(task.priority), "age_ms": 0 }),
        })
        .collect();
    let request = DecisionRequest {
        version: DECISION_API_VERSION,
        operation_id: operation,
        kind: DecisionKind::TriagePickNext,
        context: json!({ "eligible": ids }),
        artifacts: vec![],
        options,
        deadline_unix_ms: 0,
        pin: DecisionPin {
            policy: TRIAGE_PICK_NEXT_BASELINE.to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    };

    let chosen = match decider.decide(&request, DecisionAttempt::Baseline)? {
        DecisionOutcome::Applied {
            chosen: Some(id), ..
        }
        | DecisionOutcome::Fallback {
            chosen: Some(id), ..
        } => id,
        DecisionOutcome::Parked { reason } => {
            return Err(ContractError::KnownFailure(format!(
                "triage parked: {reason}"
            )))
        }
        other => {
            return Err(ContractError::KnownFailure(format!(
                "triage produced no choice: {other:?}"
            )))
        }
    };
    tasks
        .iter()
        .find(|task| task.task_id.as_str() == chosen)
        .cloned()
        .ok_or_else(|| ContractError::KnownFailure(format!("triage chose unoffered task {chosen}")))
}

/// Priority attribute for `triage.pick-next` (the static policy picks the
/// *smallest* value first, so higher priority maps to a smaller number).
fn priority_weight(priority: TaskPriority) -> u8 {
    match priority {
        TaskPriority::Critical => 0,
        TaskPriority::High => 1,
        TaskPriority::Normal => 2,
        TaskPriority::Low => 3,
    }
}

/// Claim, execute, and project one task; returns its terminal outcome.
#[allow(clippy::too_many_arguments)]
fn run_task(
    repo: &Path,
    state_dir: &Path,
    ledger: &LedgerHandle,
    override_model: Option<&str>,
    paid_available: bool,
    proxy_url: Option<String>,
    task: &Task,
    evidence: &ClaimEvidence,
    queue: &mut TaskQueueHandle,
    harness: HarnessHandle,
) -> Result<Terminal, ContractError> {
    let (builder, verifier) = task_routes(state_dir, task, override_model, paid_available)?;
    let mut executor = build_executor(
        ledger,
        state_dir,
        &builder,
        &verifier,
        paid_available,
        proxy_url,
        task,
        repo,
        harness,
    )?;
    let execution = executor.start(&task.task_id)?;
    let generation = executor
        .ledger()
        .execution(&execution)?
        .map(|record| record.generation)
        .unwrap_or(1);
    let lease = execution_lease(&executor, &execution, generation);
    record_claim(&mut executor, &execution, evidence, generation, lease)?;

    let terminal = drive_task(
        &mut executor,
        &execution,
        state_dir,
        queue,
        &task.task_id,
        lease,
    )?;
    finalize(
        state_dir,
        queue,
        &execution,
        task,
        &builder.model,
        lease,
        &terminal,
    )?;
    Ok(terminal)
}

/// Resolve the (builder, verifier) static routes for `task` (H3). Both are
/// recorded as `route.model` decisions; recovery reuses the recorded outcome.
fn task_routes(
    state_dir: &Path,
    task: &Task,
    override_model: Option<&str>,
    paid_available: bool,
) -> Result<(RouteSelection, RouteSelection), ContractError> {
    let allow_paid = routing::allow_paid_from_env();
    let builder = routing::resolve_route(
        state_dir,
        task.task_id.as_str(),
        "builder",
        task.family,
        allow_paid,
        paid_available,
        override_model,
    )?;
    let verifier = routing::resolve_route(
        state_dir,
        task.task_id.as_str(),
        "verifier",
        task.family,
        allow_paid,
        paid_available,
        override_model,
    )?;
    Ok((builder, verifier))
}

/// Project a terminal outcome to the backlog and record the execution digest
/// (M1.4). The digest summary line rides a `bd comment` on done/parked;
/// `Blocked` records nothing.
fn finalize(
    state_dir: &Path,
    queue: &mut TaskQueueHandle,
    execution: &ExecutionId,
    task: &Task,
    model: &str,
    lease: u32,
    terminal: &Terminal,
) -> Result<(), ContractError> {
    let verdict = match terminal {
        Terminal::Done => Some(crate::digest::Verdict::Green),
        Terminal::Parked(_) => Some(crate::digest::Verdict::Red),
        Terminal::Blocked => None,
    };
    let mut summary = None;
    if let Some(verdict) = verdict {
        match crate::digest::record(state_dir, execution, task, model, verdict) {
            Ok(digest) => summary = Some(digest.summary_line()),
            Err(err) => eprintln!("agalma: digest for {execution} failed: {err}"),
        }
    }

    match terminal {
        Terminal::Done => {
            // Comment before closing so the digest rides the issue.
            if let Some(summary) = &summary {
                queue.comment(&task.task_id, summary)?;
            }
            queue.close_task(&task.task_id, "done: green")?;
        }
        Terminal::Parked(reason) => {
            queue.comment(
                &task.task_id,
                &format!("phase=parked execution={execution} lease={lease}"),
            )?;
            // Parked is visible but unclaimed: status `blocked` (not
            // `in_progress`) plus the `parked` label (H1 park projection).
            queue.set_status(&task.task_id, "blocked")?;
            queue.add_label(&task.task_id, PARKED_LABEL)?;
            if let Some(summary) = &summary {
                queue.comment(&task.task_id, summary)?;
            }
            queue.comment(&task.task_id, &format!("parked: {reason}"))?;
        }
        Terminal::Blocked => {}
    }

    // Reap the execution's heavy scratch now that it is terminal. Covers both
    // fresh paths and executions driven by `recover_inflight`. Best-effort.
    if matches!(terminal, Terminal::Done | Terminal::Parked(_)) {
        let reap = crate::reap::reap_execution(state_dir, execution);
        eprintln!("agalma: reap {}", crate::reap::summary_line(&reap));
    }
    Ok(())
}

/// Drive one execution to a terminal state, projecting each phase transition to
/// the backlog and honoring the `STOP` sentinel.
fn drive_task(
    executor: &mut Executor<LedgerHandle>,
    execution: &ExecutionId,
    state_dir: &Path,
    queue: &mut TaskQueueHandle,
    task_id: &TaskId,
    lease: u32,
) -> Result<Terminal, ContractError> {
    let stop = state_dir.join("STOP");
    let mut last_phase: Option<ExecutionPhase> = None;
    loop {
        if stop.exists() {
            executor.set_kill_latch(true)?;
            eprintln!("agalma: STOP sentinel present; kill latch set");
            return Ok(Terminal::Blocked);
        }
        let status = executor.status(execution)?;
        if last_phase != Some(status.phase) {
            queue.comment(
                task_id,
                &format!(
                    "phase={} execution={} lease={}",
                    phase_str(status.phase),
                    execution,
                    lease
                ),
            )?;
            last_phase = Some(status.phase);
        }
        match executor.step(execution)? {
            StepOutcome::Dispatched { .. } | StepOutcome::Advanced { .. } => continue,
            StepOutcome::Idle { .. } => {
                let done = status.state == ExecutionState::Completed
                    || status.phase == ExecutionPhase::Done;
                return Ok(if done {
                    Terminal::Done
                } else {
                    Terminal::Parked(format!("idle in phase {}", phase_str(status.phase)))
                });
            }
            StepOutcome::Parked { reason } => return Ok(Terminal::Parked(reason)),
            StepOutcome::Blocked { .. } => return Ok(Terminal::Blocked),
        }
    }
}

/// Record the claim and lease generation durably before dispatch.
fn record_claim(
    executor: &mut Executor<LedgerHandle>,
    execution: &ExecutionId,
    evidence: &ClaimEvidence,
    generation: u32,
    lease: u32,
) -> Result<(), ContractError> {
    let event = ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: "claim".to_string(),
        payload: json!({
            "task_id": evidence.task_id,
            "status": evidence.status,
            "assignee": evidence.assignee,
            "started_at": evidence.started_at,
            "generation": generation,
            "lease_generation": lease,
        }),
        recorded_at_unix_ms: now_ms(),
    };
    executor.ledger_mut().commit(CommitBatch {
        expected_revision: None,
        state: None,
        events: vec![event],
        receipts: vec![],
        intents: vec![],
    })?;
    Ok(())
}

/// Recover executions left non-terminal by a previous run: rebuild the task's
/// repo-mode activities and drive each to a terminal state.
#[allow(clippy::too_many_arguments)]
fn recover_inflight(
    repo: &Path,
    state_dir: &Path,
    ledger: &LedgerHandle,
    override_model: Option<&str>,
    paid_available: bool,
    proxy_url: Option<String>,
    queue: &mut TaskQueueHandle,
    harness: HarnessHandle,
) -> Result<(), ContractError> {
    let executions = ledger.executions()?;
    for record in executions {
        let terminal = matches!(
            record.state,
            ExecutionState::Completed | ExecutionState::Parked | ExecutionState::Failed
        ) || matches!(record.phase, ExecutionPhase::Done | ExecutionPhase::Parked);
        if terminal {
            continue;
        }
        let task = match queue.get(&record.task_id) {
            Ok(task) => task,
            Err(err) => {
                eprintln!(
                    "agalma: cannot recover {}: task {} unavailable: {err}",
                    record.execution_id, record.task_id
                );
                continue;
            }
        };
        let (builder, verifier) = task_routes(state_dir, &task, override_model, paid_available)?;
        let mut executor = build_executor(
            ledger,
            state_dir,
            &builder,
            &verifier,
            paid_available,
            proxy_url.clone(),
            &task,
            repo,
            harness.clone(),
        )?;
        let lease = execution_lease(&executor, &record.execution_id, record.generation);
        let terminal = drive_task(
            &mut executor,
            &record.execution_id,
            state_dir,
            queue,
            &record.task_id,
            lease,
        )?;
        finalize(
            state_dir,
            queue,
            &record.execution_id,
            &task,
            &builder.model,
            lease,
            &terminal,
        )?;
    }
    Ok(())
}

/// Build a per-task executor with repo-mode activities and the same ledger.
#[allow(clippy::too_many_arguments)]
fn build_executor(
    ledger: &LedgerHandle,
    state_dir: &Path,
    builder: &RouteSelection,
    verifier: &RouteSelection,
    paid_available: bool,
    proxy_url: Option<String>,
    task: &Task,
    repo: &Path,
    harness: HarnessHandle,
) -> Result<Executor<LedgerHandle>, ContractError> {
    let mut executor = Executor::new(
        ledger.clone(),
        ExecutorConfig {
            max_attempts: task.max_attempts,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    let deps = phase_deps(
        state_dir,
        builder,
        verifier,
        paid_available,
        proxy_url,
        task,
        repo,
        harness,
    )?;
    let workspace = Rc::new(RefCell::new(WorkspaceHandle::from_transport(
        state_dir,
        WorkspaceTransport::from_env(),
    )?));
    let state = Rc::new(RefCell::new(RunState::default()));
    let acts = activities_with_mode(deps, workspace, state, ActivityMode::from_env());
    executor.add_activity(ExecutionPhase::Intake, acts.intake);
    executor.add_activity(ExecutionPhase::Checkout, acts.checkout);
    executor.add_activity(ExecutionPhase::Build, acts.build);
    executor.add_activity(ExecutionPhase::Verify, acts.verify);
    executor.add_activity(ExecutionPhase::Integrate, acts.integrate);
    Ok(executor)
}

/// Assemble repo-mode phase wiring, materializing the sandbox profile and the
/// base builder prompt under the state dir.
#[allow(clippy::too_many_arguments)]
fn phase_deps(
    state_dir: &Path,
    builder: &RouteSelection,
    verifier: &RouteSelection,
    paid_available: bool,
    proxy_url: Option<String>,
    task: &Task,
    repo: &Path,
    harness: HarnessHandle,
) -> Result<PhaseDeps, ContractError> {
    let profile_path = state_dir.join("worker.sb");
    std::fs::write(&profile_path, PRODUCT_PROFILE).map_err(|e| {
        ContractError::KnownFailure(format!(
            "cannot write sandbox profile {}: {e}",
            profile_path.display()
        ))
    })?;

    let prompts_dir = state_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)
        .map_err(|e| ContractError::KnownFailure(format!("cannot create prompts dir: {e}")))?;
    let builder_prompt = prompts_dir.join("builder.md");
    if !builder_prompt.exists() {
        std::fs::write(&builder_prompt, BUILDER_PROMPT).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot write builder prompt {}: {e}",
                builder_prompt.display()
            ))
        })?;
    }

    let (extra_ro_roots, toolchain_env) = toolchain()?;
    Ok(PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture: repo.to_path_buf(),
        model: builder.model.clone(),
        verifier_model: verifier.model.clone(),
        route: builder.label(),
        verifier_route: verifier.label(),
        role: "builder".to_string(),
        profile_path,
        builder_prompt,
        proxy_url,
        paid_proxy: paid_available,
        extra_ro_roots,
        toolchain_env,
        turn_timeout: DEFAULT_TURN_TIMEOUT,
        verify_timeout: DEFAULT_VERIFY_TIMEOUT,
        idle_timeout: crate::composition::resolve_idle_timeout(),
        repo: Some(RepoMode {
            origin: repo.to_path_buf(),
            base_ref: task.base_ref.clone(),
            task_title: task.title.clone(),
            acceptance: task.acceptance.clone(),
        }),
        harness,
    })
}

/// Lease generation pinned in an execution's `execution_created` event
/// (fallback: its task generation).
fn execution_lease(
    executor: &Executor<LedgerHandle>,
    execution: &ExecutionId,
    fallback: u32,
) -> u32 {
    executor
        .ledger()
        .events(execution)
        .ok()
        .and_then(|events| {
            events.iter().find_map(|event| {
                if event.kind != "execution_created" {
                    return None;
                }
                event
                    .payload
                    .get("lease_generation")
                    .and_then(|v| v.as_u64())
                    .or_else(|| event.payload.get("generation").and_then(|v| v.as_u64()))
            })
        })
        .map(|v| v as u32)
        .unwrap_or(fallback)
}

/// Stable label for an execution phase.
fn phase_str(phase: ExecutionPhase) -> &'static str {
    match phase {
        ExecutionPhase::Intake => "intake",
        ExecutionPhase::Checkout => "checkout",
        ExecutionPhase::Build => "build",
        ExecutionPhase::Verify => "verify",
        ExecutionPhase::Integrate => "integrate",
        ExecutionPhase::Done => "done",
        ExecutionPhase::Parked => "parked",
    }
}

/// Stable 64-bit FNV-1a hash of a string (hex).
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_weight_orders_critical_first() {
        assert!(priority_weight(TaskPriority::Critical) < priority_weight(TaskPriority::High));
        assert!(priority_weight(TaskPriority::High) < priority_weight(TaskPriority::Normal));
        assert!(priority_weight(TaskPriority::Normal) < priority_weight(TaskPriority::Low));
    }

    #[test]
    fn short_hash_is_stable_and_distinct() {
        assert_eq!(short_hash("a,b"), short_hash("a,b"));
        assert_ne!(short_hash("a,b"), short_hash("b,a"));
    }
}
