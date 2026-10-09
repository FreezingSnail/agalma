//! Queue-driven work loop (M1, `agalma-52k.3`).
//!
//! `agalma work --repo <path> [--once] [--max-tasks N] [--state-dir] [--model]`
//! turns the M0 single-task conductor into a loop over the `bd` backlog:
//!
//! 1. **intake** — [`BdTaskQueue::ready_tasks`] lists eligible tasks; when more
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
//!    `done` closes the issue, `parked` adds the `parked` label and a reason.
//!
//! The loop keeps the M0 controls: the SIGTERM latch, the `STOP` sentinel, boot
//! recovery ordering, and `--once` for tests. Fencing, duplicate dispatch, and
//! projection reconcile are M1.6; the verifier diagnosis is M1.3.

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
use agalma_ledger::SqliteLedger;
use agalma_sandbox::PRODUCT_PROFILE;
use agalma_taskqueue::{BdTaskQueue, PARKED_LABEL};
use agalma_workspace::Workspace;
use serde_json::json;

use crate::cli::WorkArgs;
use crate::composition::{toolchain, BUILDER_PROMPT, DEFAULT_TURN_TIMEOUT, DEFAULT_VERIFY_TIMEOUT};
use crate::config::{resolve_model, resolve_state_dir};
use crate::phases::{activities_with_mode, ActivityMode, PhaseDeps, RepoMode, RunState};
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
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
    let model = resolve_model(args.model.as_deref());
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
    {
        let ledger = SqliteLedger::open(&ledger_path)?;
        let version = ledger.schema_version()?;
        if version != LEDGER_SCHEMA_VERSION {
            return Err(ContractError::KnownFailure(format!(
                "incompatible ledger schema {version} (expected {LEDGER_SCHEMA_VERSION}) at {}",
                ledger_path.display()
            )));
        }
    }

    crate::install_signal_handler(ledger_path.clone());

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

    let mut queue = BdTaskQueue::new(&repo);
    let mut decider = StaticDecider::new(SqliteLedger::open(&ledger_path)?);

    // Boot recovery: drive any non-terminal execution to a terminal state before
    // admitting new work (recovery ordering).
    recover_inflight(&repo, &state_dir, &model, proxy_url.clone(), &mut queue)?;

    let mut processed: u32 = 0;
    let mut last: Option<Terminal> = None;
    loop {
        if state_dir.join("STOP").exists() {
            eprintln!("agalma: STOP sentinel present; halting work loop");
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
            &model,
            proxy_url.clone(),
            &task,
            &evidence,
            &mut queue,
        )?;
        processed += 1;
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
    decider: &mut StaticDecider<SqliteLedger>,
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
fn run_task(
    repo: &Path,
    state_dir: &Path,
    model: &str,
    proxy_url: Option<String>,
    task: &Task,
    evidence: &ClaimEvidence,
    queue: &mut BdTaskQueue,
) -> Result<Terminal, ContractError> {
    let mut executor = build_executor(state_dir, model, proxy_url, task, repo)?;
    let execution = executor.start(&task.task_id)?;
    let generation = executor
        .ledger()
        .execution(&execution)?
        .map(|record| record.generation)
        .unwrap_or(1);
    record_claim(&mut executor, &execution, evidence, generation)?;

    let terminal = drive_task(
        &mut executor,
        &execution,
        state_dir,
        queue,
        &task.task_id,
        generation,
    )?;
    match &terminal {
        Terminal::Done => {
            queue.close_task(&task.task_id, "done: green")?;
        }
        Terminal::Parked(reason) => {
            queue.comment(
                &task.task_id,
                &format!("phase=parked execution={execution} lease={generation}"),
            )?;
            queue.add_label(&task.task_id, PARKED_LABEL)?;
            queue.comment(&task.task_id, &format!("parked: {reason}"))?;
        }
        Terminal::Blocked => {}
    }
    Ok(terminal)
}

/// Drive one execution to a terminal state, projecting each phase transition to
/// the backlog and honoring the `STOP` sentinel.
fn drive_task(
    executor: &mut Executor<SqliteLedger>,
    execution: &ExecutionId,
    state_dir: &Path,
    queue: &mut BdTaskQueue,
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
    executor: &mut Executor<SqliteLedger>,
    execution: &ExecutionId,
    evidence: &ClaimEvidence,
    generation: u32,
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
            "lease_generation": generation,
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
fn recover_inflight(
    repo: &Path,
    state_dir: &Path,
    model: &str,
    proxy_url: Option<String>,
    queue: &mut BdTaskQueue,
) -> Result<(), ContractError> {
    let executions = {
        let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite"))?;
        ledger.executions()?
    };
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
        let mut executor = build_executor(state_dir, model, proxy_url.clone(), &task, repo)?;
        match drive_task(
            &mut executor,
            &record.execution_id,
            state_dir,
            queue,
            &record.task_id,
            1,
        )? {
            Terminal::Done => {
                let _ = queue.close_task(&record.task_id, "done: green (recovered)");
            }
            Terminal::Parked(reason) => {
                let _ = queue.add_label(&record.task_id, PARKED_LABEL);
                let _ = queue.comment(&record.task_id, &format!("parked: {reason}"));
            }
            Terminal::Blocked => {}
        }
    }
    Ok(())
}

/// Build a per-task executor with repo-mode activities and the same ledger.
fn build_executor(
    state_dir: &Path,
    model: &str,
    proxy_url: Option<String>,
    task: &Task,
    repo: &Path,
) -> Result<Executor<SqliteLedger>, ContractError> {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite"))?;
    let mut executor = Executor::new(
        ledger,
        ExecutorConfig {
            max_attempts: task.max_attempts,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    let deps = phase_deps(state_dir, model, proxy_url, task, repo)?;
    let workspace = Rc::new(RefCell::new(Workspace::new(state_dir.to_path_buf())));
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
fn phase_deps(
    state_dir: &Path,
    model: &str,
    proxy_url: Option<String>,
    task: &Task,
    repo: &Path,
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
        model: model.to_string(),
        role: "builder".to_string(),
        profile_path,
        builder_prompt,
        proxy_url,
        extra_ro_roots,
        toolchain_env,
        turn_timeout: DEFAULT_TURN_TIMEOUT,
        verify_timeout: DEFAULT_VERIFY_TIMEOUT,
        repo: Some(RepoMode {
            origin: repo.to_path_buf(),
            base_ref: task.base_ref.clone(),
            task_title: task.title.clone(),
            acceptance: task.acceptance.clone(),
        }),
    })
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
