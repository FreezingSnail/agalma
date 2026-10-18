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
use std::sync::Arc;

use agalma_contracts::digest::{sha256_digest, FailureCode};
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, CancellationState, ClaimEvidence, CommitBatch, ContractError,
    DecisionApi, DecisionAttempt, DecisionKind, DecisionOption, DecisionOutcome, DecisionPin,
    DecisionRequest, DirectiveRef, ExecutionApi, ExecutionEvent, ExecutionId, ExecutionPhase,
    ExecutionRecord, ExecutionState, LedgerApi, OperationId, Role, StepOutcome, Task, TaskFamily,
    TaskId, TaskPackage, TaskPriority, TaskQueueApi, DECISION_API_VERSION,
};
use agalma_decision::{StaticDecider, TRIAGE_PICK_NEXT_BASELINE};
use agalma_execution::{
    Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION, LEDGER_SCHEMA_VERSION,
};
use agalma_taskqueue::{BdTaskQueue, PARKED_LABEL};
use serde_json::json;

use agalma_ipc::TaskQueueOps;

use crate::budget::{bound_reservation_ledger, ExecutionGates, PaidRequestGate, ReservationLedger};
use crate::cli::WorkArgs;
use crate::composition::{toolchain, BUILDER_PROMPT, DEFAULT_TURN_TIMEOUT, DEFAULT_VERIFY_TIMEOUT};
use crate::config::resolve_state_dir;
use crate::harness_binding::{
    harness_handle, plan_binding, resolve_selection, HarnessHandle, HarnessKind,
};
use crate::ledger_process::{LedgerHandle, LedgerTransport};
use crate::phases::{
    activities_with_mode, ActivityMode, PhaseDeps, RepoMode, RunState, VERIFIER_PROMPT,
};
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::routing::{self, RouteSelection};
use crate::scope_guard::ProtectedInputs;
use crate::stop::{BoundStopControl, StopControl};
use crate::task_package::{
    build_package, gather_admission_context, legacy_park, load_admitted_package,
    load_frozen_or_legacy, persist_package, read_artifact, AuthorizationInput, PackageInputs,
    PackageRef, RoleInput, ADMITTED_EVENT_KIND,
};
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
    // SF.8: the builder-scoped selector (`--builder-model`/`AGALMA_BUILDER_MODEL`)
    // overrides the builder class only and never the planner/reviewer/verifier
    // roles. It outranks the legacy global `--model` for the builder.
    let builder_selector = crate::config::resolve_builder_model(args.builder_model.as_deref());
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
    // Entry-aware STOP guard before any ledger open (`agalma-5fy.5`): a present
    // stop (including a dangling symlink or a metadata error) must never
    // initialize a database or start the proxy.
    if let Some(reason) = crate::stop::observe_sentinel(&state_dir).reason() {
        return Err(ContractError::KnownFailure(format!(
            "stop admission refused: {reason}"
        )));
    }
    // Explicit fresh initialization vs established open through the selected
    // binding (`agalma-5fy.5`): a lost/partial established ledger refuses here,
    // never recreated. The same bound control is shared with the sweep, the
    // review gate, the work loop and the phase polls.
    let ledger =
        crate::ledger_process::open_admission_ledger(&state_dir, LedgerTransport::from_env())
            .map_err(ContractError::KnownFailure)?;
    {
        let version = ledger.schema_version()?;
        if version != LEDGER_SCHEMA_VERSION {
            return Err(ContractError::KnownFailure(format!(
                "incompatible ledger schema {version} (expected {LEDGER_SCHEMA_VERSION}) at {}",
                ledger_path.display()
            )));
        }
    }
    let stop: Rc<dyn StopControl> = Rc::new(BoundStopControl::new(&state_dir, ledger.clone()));
    // Typed bound decision BEFORE proxy/workspace/queue/task admission
    // (`agalma-5fy.5`): Running normal; Unavailable immediate refusal (no attempt
    // to infer/read lost state); Stopped bounded existing survivor cleanup over
    // the authoritative bound pending intents, then refusal — no proxy/queue
    // construction or new dispatch.
    match stop.decision() {
        crate::stop::StopDecision::Running => {}
        crate::stop::StopDecision::Unavailable(reason) => {
            return Err(ContractError::KnownFailure(format!(
                "stop state unavailable; refusing work loop: {reason}"
            )));
        }
        crate::stop::StopDecision::Stopped(reason) => {
            crate::composition::reconcile_bound_survivors(&state_dir, &ledger);
            return Err(ContractError::KnownFailure(format!(
                "stop policy active; refusing work loop: {reason}"
            )));
        }
    }

    crate::install_signal_handler(state_dir.clone(), LedgerTransport::from_env());

    // HP.3: assemble the plugin host for this state dir and start configured
    // desired plugins. Best-effort and a no-op with no configured roots; the
    // host is held for the loop's lifetime and shut down when `work` returns.
    let _plugin_host =
        crate::composition::attach_work_plugin_host(&state_dir, &args.plugin_roots, &ledger);

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
    // admitting new work (recovery ordering). Each execution is reconstructed
    // from its frozen `TaskPackage/v1`, never from current bd contents.
    recover_inflight(
        &repo,
        &state_dir,
        &ledger,
        paid_available,
        proxy_url.clone(),
        &mut queue,
        harness.clone(),
        Rc::clone(&stop),
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
    match crate::sweep::run_sweep_with_stop(
        &repo,
        &state_dir,
        paid_available,
        proxy_url.clone(),
        override_model.as_deref(),
        Rc::clone(&stop),
    ) {
        Ok(report) if report.skipped_reason.is_none() => eprintln!(
            "agalma: epic sweep: {} candidate(s), {} approved, {} rejected, {} escalated, {} timed out",
            report.epics_seen.len(),
            report.approved.len(),
            report.rejected.len(),
            report.escalated.len(),
            report.timed_out.len()
        ),
        Ok(_) => {}
        Err(err) => eprintln!("agalma: epic sweep failed: {err}"),
    }

    // Per-epic review gate (CP.5): after an epic's last child lands, run one
    // reviewer session over the merged diff vs the epic design/acceptance. A
    // drift verdict files a P1 `drift-fix` task and holds ordinary dispatch.
    // Best-effort: a review failure must not block the loop, and an unavailable
    // strong route skips the gate (never a silent pass or downgrade).
    match crate::review::run_review_with_stop(
        &repo,
        &state_dir,
        paid_available,
        proxy_url.clone(),
        override_model.as_deref(),
        Rc::clone(&stop),
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

    // SF.8: an explicit builder selector that names an ineligible paid model is
    // refused before claiming any task, so a paid selection is never silently
    // substituted. An unset selector keeps the legacy global/static behavior.
    if let Some(model) = builder_selector.as_deref() {
        if let Err(err) = routing::check_builder_eligibility(
            model,
            routing::allow_paid_from_env(),
            paid_available,
        ) {
            return Err(ContractError::KnownFailure(format!(
                "builder model selector refused: {err}"
            )));
        }
    }

    let mut processed: u32 = 0;
    let mut last: Option<Terminal> = None;
    loop {
        // Stop policy before any claim/dispatch: the shared bound control's typed
        // decision halts the loop before it can mutate the backlog
        // (`agalma-r2l`, `agalma-5fy.5`). Stopped/Unavailable both deny; the
        // latch is never cleared here (only the human `agalma resume` clears it).
        if let Some(reason) = stop.decision().reason() {
            eprintln!("agalma: stop policy active ({reason}); halting work loop");
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
            builder_selector.as_deref(),
            override_model.as_deref(),
            paid_available,
            proxy_url.clone(),
            &task,
            &evidence,
            &mut queue,
            harness.clone(),
            Rc::clone(&stop),
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
    builder_selector: Option<&str>,
    override_model: Option<&str>,
    paid_available: bool,
    proxy_url: Option<String>,
    task: &Task,
    evidence: &ClaimEvidence,
    queue: &mut TaskQueueHandle,
    harness: HarnessHandle,
    stop: Rc<dyn StopControl>,
) -> Result<Terminal, ContractError> {
    let (builder, verifier) = task_routes(
        state_dir,
        task,
        builder_selector,
        override_model,
        paid_available,
    )?;
    let plan = RolePlan {
        builder_model: builder.model.clone(),
        builder_route: builder.label(),
        verifier_model: verifier.model.clone(),
        verifier_route: verifier.label(),
    };

    // Allocate a genuinely non-dispatchable preparation: execution identity and
    // lease are durable, but no dispatch intent is announced until activation.
    let mut lifecycle = Executor::new(
        ledger.clone(),
        ExecutorConfig {
            max_attempts: task.max_attempts,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    let preparation = lifecycle.prepare(&task.task_id)?;
    let execution = preparation.execution_id.clone();
    let generation = preparation.generation;
    let lease = preparation.lease_generation;

    // Freeze and persist the immutable package, then commit the admission
    // reference with activation in one atomic batch. No dispatch intent precedes
    // package admission.
    let (persisted, admission) = admit_task(
        state_dir, repo, task, &execution, generation, lease, &plan, &harness, evidence,
    )?;
    lifecycle.activate(&preparation, admission)?;
    package_crash("after_admit");

    record_claim(&mut lifecycle, &execution, evidence, generation, lease)?;

    // First dispatch consumes the frozen admitted values, not the live task.
    let package = load_admitted_package(state_dir, &persisted.digest)?;
    verify_recovered_pins(state_dir, &package, &harness)?;
    let admitted_task = task_from_package(&package, &task.task_id);
    let admitted_plan = role_plan_from_package(&package)?;
    let builder_prompt = builder_prompt_bytes(state_dir, &package)?;

    // SF.7 + `agalma-c31`: parent-owned budget admission. When a reviewed
    // provider profile is configured, this execution's builder **and** verifier
    // paid traffic runs through a per-role gated proxy, both sharing one durable
    // `ExecutionBudget` over the bound ledger API, so every billable request is
    // reserved before it reaches upstream and attributed to its own role.
    // Without paid dispatch the historical process proxy path is preserved, and
    // a missing/invalid profile denies rather than falling back ungated.
    let role_proxies = task_role_proxies(state_dir, &package, paid_available)?;
    let (task_proxy_url, verifier_proxy_url) =
        executor_proxy_urls(role_proxies.as_ref(), proxy_url.clone());

    let mut executor = build_executor(
        ledger,
        state_dir,
        &admitted_plan,
        paid_available,
        task_proxy_url,
        verifier_proxy_url,
        &admitted_task,
        repo,
        harness,
        &builder_prompt,
        Rc::clone(&stop),
    )?;

    let terminal = drive_task(
        &mut executor,
        &execution,
        queue,
        &task.task_id,
        lease,
        false,
        &*stop,
    )?;
    // CP.5: pin the epic review baseline at the effect boundary. The candidate
    // has already landed (the integrate phase committed it), but the execution is
    // not yet projected closed nor its heavy scratch reaped. Recording here —
    // *before* `finalize` — means a crash between the effect and closure cannot
    // lose the first integrated change from the review; boot recovery also
    // reconciles any landed-but-unpinned execution (see `recover_inflight`). The
    // baseline is the pre-integration base frozen in the admitted package, never
    // the live post-integration `HEAD`. A recording failure on a landed child is
    // fatal (propagated), so a later child can never establish a masked, later
    // baseline over a missing first pin.
    if matches!(terminal, Terminal::Done) {
        review_baseline_crash("after_integrate");
        crate::review::record_start_on_land(
            repo,
            task.task_id.as_str(),
            &package.repository.base_sha,
        )?;
    }
    finalize(
        state_dir,
        queue,
        &execution,
        task,
        &admitted_plan.builder_model,
        lease,
        &terminal,
        false,
    )?;
    Ok(terminal)
}

/// Environment variable naming a reviewed provider request profile (JSON). When
/// set, the work loop runs paid traffic through the SF.7 durable budget gate.
pub const BUDGET_PROFILE_ENV: &str = "AGALMA_BUDGET_PROFILE";

/// Resolve the reviewed paid [`ProviderRequestProfile`] for one task run.
///
/// **Fail closed** (`agalma-c31`): paid dispatch is only ever allowed behind a
/// reviewed profile. When `paid_available` is true and the profile is absent,
/// unreadable, invalid, unsupported (`n != 1`), unknown-priced, or expired this
/// denies instead of falling back to an ungated proxy. When paid dispatch is not
/// available the free path is returned unchanged (`Ok(None)`).
pub fn resolve_paid_profile(
    paid_available: bool,
    profile_path: Option<&str>,
) -> Result<Option<agalma_contracts::budget::ProviderRequestProfile>, ContractError> {
    use agalma_contracts::budget::ProviderRequestProfile;
    if !paid_available {
        return Ok(None);
    }
    let path = profile_path
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            ContractError::KnownFailure(format!(
                "paid dispatch denied: no reviewed provider budget profile \
                 ({BUDGET_PROFILE_ENV} unset); refusing ungated paid admission"
            ))
        })?;
    let text = std::fs::read_to_string(path).map_err(|e| {
        ContractError::KnownFailure(format!(
            "paid dispatch denied: budget profile {path} unreadable: {e}"
        ))
    })?;
    let profile: ProviderRequestProfile = serde_json::from_str(&text).map_err(|e| {
        ContractError::KnownFailure(format!(
            "paid dispatch denied: budget profile {path} invalid: {e}"
        ))
    })?;
    // An unknown/unsupported price bound (`n != 1`, overflow) is unusable.
    profile.reservation_nusd().map_err(|e| {
        ContractError::KnownFailure(format!(
            "paid dispatch denied: budget profile {path} has no valid reservation bound: {e}"
        ))
    })?;
    if !profile.is_valid_at(crate::budget::now_unix_ms()) {
        return Err(ContractError::KnownFailure(format!(
            "paid dispatch denied: budget profile {path} is expired at the current time"
        )));
    }
    Ok(Some(profile))
}

/// The per-role gated provider proxies for one task execution (`agalma-c31`).
///
/// Both roles share one [`ExecutionGates::budget`] so their combined spend
/// cannot exceed the task allowance, while each request is attributed to the
/// proxy's own pinned role — a verifier request is charged to the verifier, not
/// the builder. The fresh dispatch path and recovery both construct proxies
/// through [`execution_role_proxies`], so a recovered execution attributes roles
/// exactly like a freshly admitted one.
pub struct TaskRoleProxies {
    builder: ProviderProxy,
    verifier: ProviderProxy,
    gates: Arc<ExecutionGates>,
}

impl TaskRoleProxies {
    /// The builder-role proxy (bound to its own ephemeral loopback port).
    pub fn builder(&self) -> &ProviderProxy {
        &self.builder
    }

    /// The verifier-role proxy (bound to its own ephemeral loopback port).
    pub fn verifier(&self) -> &ProviderProxy {
        &self.verifier
    }

    /// The shared execution budget both role gates reserve against.
    pub fn budget(&self) -> &Arc<crate::budget::ExecutionBudget> {
        self.gates.budget()
    }

    pub fn builder_url(&self) -> String {
        self.builder.url()
    }
    pub fn verifier_url(&self) -> String {
        self.verifier.url()
    }
}

/// Start one pinned gate proxy for `role`, sharing the execution budget. The
/// `base` config supplies the upstream injection (production: `from_env`); the
/// admission gate is attached per role.
fn start_role_proxy(
    gates: &Arc<ExecutionGates>,
    role: Role,
    base: &ProxyConfig,
) -> Result<ProviderProxy, ContractError> {
    let gate = gates.gate(role).ok_or_else(|| {
        ContractError::KnownFailure(format!("no budget gate constructed for role {role:?}"))
    })?;
    let gate_arc: Arc<crate::budget::BudgetGate> = Arc::clone(gate);
    let dynamic: Arc<dyn PaidRequestGate> = gate_arc;
    let mut config = base.clone();
    config.budget = Some(dynamic);
    ProviderProxy::start(config)
        .map_err(|e| ContractError::KnownFailure(format!("gated provider proxy ({role:?}): {e}")))
}

/// Build the shared builder/verifier role-gated proxies over `ledger` for one
/// `execution`. This is the single production constructor used by **both** fresh
/// dispatch ([`task_role_proxies`]) and recovery (`recover_inflight`,
/// `build_recovery_executor`), so role attribution cannot differ between an
/// admitted and a recovered execution. `base_config` is the upstream/injection
/// template (production passes [`ProxyConfig::from_env`]).
pub fn execution_role_proxies(
    ledger: Box<dyn ReservationLedger>,
    execution_id: &str,
    package_digest: &str,
    allowance_nusd: Option<agalma_contracts::budget::Nusd>,
    profile: agalma_contracts::budget::ProviderRequestProfile,
    base_config: ProxyConfig,
) -> Result<TaskRoleProxies, ContractError> {
    let gates = ExecutionGates::open(
        ledger,
        ExecutionId::new(execution_id),
        package_digest,
        allowance_nusd,
        None,
        profile,
        &[Role::Builder, Role::Verifier],
    )?;
    Ok(TaskRoleProxies {
        builder: start_role_proxy(&gates, Role::Builder, &base_config)?,
        verifier: start_role_proxy(&gates, Role::Verifier, &base_config)?,
        gates,
    })
}

/// The `(builder, verifier)` proxy URLs an executor should use for `package`.
/// When role-gated proxies exist the verifier gets its **own** URL; otherwise
/// both roles fall back to the caller's free-path `fallback`. Recovery and fresh
/// dispatch both route through this so a verifier is never silently sent to the
/// builder proxy.
pub fn executor_proxy_urls(
    role_proxies: Option<&TaskRoleProxies>,
    fallback: Option<String>,
) -> (Option<String>, Option<String>) {
    match role_proxies {
        Some(proxies) => (Some(proxies.builder_url()), Some(proxies.verifier_url())),
        None => (fallback, None),
    }
}

/// Build the builder/verifier gated proxies for `package` when a reviewed
/// profile is configured and paid dispatch is available. Every billable request
/// is durably reserved through the bound ledger **before** upstream contact and
/// attributed to the session's own role. An absent/invalid profile denies (no
/// ungated fallback); when paid dispatch is unavailable this returns `None` and
/// the historical free proxy path is preserved.
fn task_role_proxies(
    state_dir: &Path,
    package: &TaskPackage,
    paid_available: bool,
) -> Result<Option<TaskRoleProxies>, ContractError> {
    let profile_path = std::env::var(BUDGET_PROFILE_ENV).ok();
    let Some(profile) = resolve_paid_profile(paid_available, profile_path.as_deref())? else {
        return Ok(None);
    };
    let ledger = bound_reservation_ledger(state_dir)?;
    Ok(Some(execution_role_proxies(
        ledger,
        &package.execution_id,
        &package.package_digest,
        Some(package.budget.allowance_nusd),
        profile,
        ProxyConfig::from_env(),
    )?))
}

/// Resolve the (builder, verifier) static routes for `task` (H3, SF.8). Both are
/// recorded as `route.model` decisions; recovery reuses the recorded outcome.
///
/// The builder route honors the scoped builder selector (`builder_selector`),
/// which outranks the legacy global `override_model` for the builder only. The
/// verifier route uses only the legacy global override, so the builder selector
/// never changes a non-builder role.
fn task_routes(
    state_dir: &Path,
    task: &Task,
    builder_selector: Option<&str>,
    override_model: Option<&str>,
    paid_available: bool,
) -> Result<(RouteSelection, RouteSelection), ContractError> {
    let allow_paid = routing::allow_paid_from_env();
    let overrides = routing::RoleModelOverride::new(builder_selector, override_model);
    let builder = routing::resolve_builder_route(
        state_dir,
        task.task_id.as_str(),
        task.family,
        allow_paid,
        paid_available,
        overrides,
    )?;
    let verifier = routing::resolve_route(
        state_dir,
        task.task_id.as_str(),
        "verifier",
        task.family,
        allow_paid,
        paid_available,
        overrides.effective_global(),
    )?;
    Ok((builder, verifier))
}

/// Project a terminal outcome to the backlog and record the execution digest
/// (M1.4). The digest summary line rides a `bd comment` on done/parked;
/// `Blocked` records nothing. `best_effort` is set by recovery, where the source
/// issue may have been edited or removed: the durable ledger terminal state is
/// then authoritative and a failed projection is logged, not fatal.
#[allow(clippy::too_many_arguments)]
fn finalize(
    state_dir: &Path,
    queue: &mut TaskQueueHandle,
    execution: &ExecutionId,
    task: &Task,
    model: &str,
    lease: u32,
    terminal: &Terminal,
    best_effort: bool,
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

    let projection = project_terminal(queue, execution, task, &summary, lease, terminal);
    match projection {
        Ok(()) => {}
        Err(err) if best_effort => eprintln!(
            "agalma: recovery projection for {} failed (best-effort): {err}",
            task.task_id.as_str()
        ),
        Err(err) => return Err(err),
    }

    // Reap the execution's heavy scratch now that it is terminal. Covers both
    // fresh paths and executions driven by `recover_inflight`. Best-effort.
    if matches!(terminal, Terminal::Done | Terminal::Parked(_)) {
        let reap = crate::reap::reap_execution(state_dir, execution);
        eprintln!("agalma: reap {}", crate::reap::summary_line(&reap));
    }
    Ok(())
}

/// Apply the terminal projection to the backlog (see [`finalize`]).
fn project_terminal(
    queue: &mut TaskQueueHandle,
    execution: &ExecutionId,
    task: &Task,
    summary: &Option<String>,
    lease: u32,
    terminal: &Terminal,
) -> Result<(), ContractError> {
    match terminal {
        Terminal::Done => {
            // Comment before closing so the digest rides the issue.
            if let Some(summary) = summary {
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
            if let Some(summary) = summary {
                queue.comment(&task.task_id, summary)?;
            }
            queue.comment(&task.task_id, &format!("parked: {reason}"))?;
        }
        Terminal::Blocked => {}
    }
    Ok(())
}

/// Drive one execution to a terminal state, projecting each phase transition to
/// the backlog and honoring the shared bound stop decision. `best_effort`
/// (recovery) makes the phase comment non-fatal when the source issue has been
/// removed. `Unavailable` suppresses projection and dispatch without setting the
/// latch (`agalma-5fy.5`).
#[allow(clippy::too_many_arguments)]
fn drive_task(
    executor: &mut Executor<LedgerHandle>,
    execution: &ExecutionId,
    queue: &mut TaskQueueHandle,
    task_id: &TaskId,
    lease: u32,
    best_effort: bool,
    stop: &dyn StopControl,
) -> Result<Terminal, ContractError> {
    let mut last_phase: Option<ExecutionPhase> = None;
    loop {
        match stop.decision() {
            crate::stop::StopDecision::Running => {}
            crate::stop::StopDecision::Stopped(reason) => {
                executor.set_kill_latch(true)?;
                eprintln!("agalma: stop policy active ({reason}); kill latch set");
                return Ok(Terminal::Blocked);
            }
            crate::stop::StopDecision::Unavailable(reason) => {
                eprintln!("agalma: stop state unavailable ({reason}); halting without projection");
                return Ok(Terminal::Blocked);
            }
        }
        let status = executor.status(execution)?;
        if last_phase != Some(status.phase) {
            let projection = queue.comment(
                task_id,
                &format!(
                    "phase={} execution={} lease={}",
                    phase_str(status.phase),
                    execution,
                    lease
                ),
            );
            if let Err(err) = projection {
                if !best_effort {
                    return Err(err);
                }
                eprintln!(
                    "agalma: recovery phase projection for {} failed (best-effort): {err}",
                    task_id.as_str()
                );
            }
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

/// Recover executions left non-terminal by a previous run. Each execution is
/// reconstructed from its frozen `TaskPackage/v1` (loaded by the digest recorded
/// in its `task.package_admitted` event) — never from the current backlog, which
/// may have been edited or removed. A missing reference or unreadable/corrupt/
/// incompatible package parks the execution explicitly with no new effects.
fn recover_inflight(
    repo: &Path,
    state_dir: &Path,
    ledger: &LedgerHandle,
    paid_available: bool,
    proxy_url: Option<String>,
    queue: &mut TaskQueueHandle,
    harness: HarnessHandle,
    stop: Rc<dyn StopControl>,
) -> Result<(), ContractError> {
    let executions = ledger.executions()?;
    for record in executions {
        // Per-execution cancellation survives factory resume. It is settled
        // BEFORE any terminal/`continue` decision, because a cancellation request
        // (and a retryable settling) must be reconciled even when the coarse row
        // already looks parked. Resume only clears the global latch; it never
        // removes cancellation.
        let cancellation = cancellation_state_of(ledger, &record.execution_id)?;
        match cancellation {
            CancellationState::Requested | CancellationState::Settling => {
                settle_cancelled_execution(
                    repo,
                    state_dir,
                    ledger,
                    paid_available,
                    proxy_url.clone(),
                    queue,
                    harness.clone(),
                    Rc::clone(&stop),
                    &record,
                )?;
                continue;
            }
            CancellationState::Settled => {
                // Already settled and projected on an earlier boot.
                continue;
            }
            CancellationState::None => {}
        }

        let terminal = matches!(
            record.state,
            ExecutionState::Completed | ExecutionState::Parked | ExecutionState::Failed
        ) || matches!(record.phase, ExecutionPhase::Done | ExecutionPhase::Parked);
        if terminal {
            // Reconcile the epic review baseline for a *landed* execution whose
            // fresh run died after the integrate effect but before the baseline
            // was recorded (or between recording and closure). The frozen package
            // — not the live post-merge HEAD and not bd — is the retained durable
            // input; `record_start_on_land` is first-pin-wins and a no-op for a
            // task with no parent epic, so re-running it every boot is safe. Fail
            // closed: a landed child whose pin cannot be reconciled is an error,
            // so `reconcile_projection` cannot close it into a truncated review.
            let landed = matches!(record.state, ExecutionState::Completed)
                || matches!(record.phase, ExecutionPhase::Done);
            if landed {
                // Only an *unpinned* epic needs reconciliation; a landed child of
                // a task with no epic, or of an epic that already carries a valid
                // first pin, is safe to project closed. A landed child whose epic
                // has no pin yet must be pinned from the retained durable frozen
                // package before closure; if that input is missing/corrupt, refuse
                // (do not close and do not run ordinary work) so a later child can
                // never mask the missing first pin.
                match crate::review::review_baseline_status(repo, record.task_id.as_str())? {
                    crate::review::ReviewBaselineStatus::NoEpic
                    | crate::review::ReviewBaselineStatus::Pinned => {}
                    crate::review::ReviewBaselineStatus::Unpinned { epic } => {
                        let reference = admission_digest(ledger, &record.execution_id)?;
                        match load_frozen_or_legacy(
                            state_dir,
                            reference.as_deref(),
                            record.execution_id.as_str(),
                        ) {
                            Ok(package) => {
                                verify_recovered_package_identity(&package, &record)?;
                                crate::review::record_start_on_land(
                                    repo,
                                    record.task_id.as_str(),
                                    &package.repository.base_sha,
                                )?;
                            }
                            Err(err) => {
                                return Err(ContractError::KnownFailure(format!(
                                    "recovery refused: landed execution {} cannot pin the first \
                                     review baseline for epic {epic} — no readable frozen package \
                                     ({err}); refusing to close the task and refusing ordinary \
                                     work until the missing first pin is reconciled",
                                    record.execution_id
                                )));
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Recovery reads the frozen package; it never refreshes the definition
        // from bd. No admission reference -> explicit legacy park.
        let reference = admission_digest(ledger, &record.execution_id)?;
        let package = match load_frozen_or_legacy(
            state_dir,
            reference.as_deref(),
            record.execution_id.as_str(),
        ) {
            Ok(package) => package,
            Err(err) => {
                eprintln!("agalma: recovery parks {}: {err}", record.execution_id);
                park_execution(ledger, &record, &err.to_string(), queue)?;
                continue;
            }
        };

        if package.execution_id != record.execution_id.as_str() {
            let reason = format!(
                "frozen package {} does not match execution {}",
                package.execution_id, record.execution_id
            );
            eprintln!("agalma: recovery parks {}: {reason}", record.execution_id);
            park_execution(ledger, &record, &reason, queue)?;
            continue;
        }

        let plan = match role_plan_from_package(&package) {
            Ok(plan) => plan,
            Err(err) => {
                eprintln!("agalma: recovery parks {}: {err}", record.execution_id);
                park_execution(ledger, &record, &err.to_string(), queue)?;
                continue;
            }
        };

        let task_id = TaskId::new(package.task_id.clone());
        let task = task_from_package(&package, &task_id);
        if let Err(err) = verify_recovered_pins(state_dir, &package, &harness) {
            eprintln!("agalma: recovery parks {}: {err}", record.execution_id);
            park_execution(ledger, &record, &err.to_string(), queue)?;
            continue;
        }
        let builder_prompt = match builder_prompt_bytes(state_dir, &package) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("agalma: recovery parks {}: {err}", record.execution_id);
                park_execution(ledger, &record, &err.to_string(), queue)?;
                continue;
            }
        };
        // Role-gated proxies for the recovered execution (`agalma-c31`): a
        // recovered paid run attributes builder/verifier requests exactly like a
        // fresh admission, so the verifier does not bill the builder's scope.
        // Held for the iteration so the proxies outlive the executor.
        let role_proxies = task_role_proxies(state_dir, &package, paid_available)?;
        let (builder_proxy_url, verifier_proxy_url) =
            executor_proxy_urls(role_proxies.as_ref(), proxy_url.clone());
        let mut executor = build_executor(
            ledger,
            state_dir,
            &plan,
            paid_available,
            builder_proxy_url,
            verifier_proxy_url,
            &task,
            repo,
            harness.clone(),
            &builder_prompt,
            Rc::clone(&stop),
        )?;
        let lease = execution_lease(&executor, &record.execution_id, record.generation);
        // SF.3 execution-scoped recovery gate: replay THIS execution's recorded
        // history, require projection agreement, and reconcile only its pending
        // intents/survivors on the pinned bindings (the activities just
        // registered from its frozen package). A projection that disagrees with
        // preserved history refuses the gate with no effect; refuse dispatch by
        // parking rather than driving `step`.
        if let Err(err) = executor.recover_execution(&record.execution_id) {
            eprintln!("agalma: recovery gate parks {}: {err}", record.execution_id);
            park_execution(ledger, &record, &err.to_string(), queue)?;
            continue;
        }
        let terminal = drive_task(
            &mut executor,
            &record.execution_id,
            queue,
            &task_id,
            lease,
            true,
            &*stop,
        )?;
        finalize(
            state_dir,
            queue,
            &record.execution_id,
            &task,
            &plan.builder_model,
            lease,
            &terminal,
            true,
        )?;
        // CP.5 on recovery: a boot recovery that integrates a child's candidate
        // pins the epic review baseline from the same durable pre-integration
        // base (the frozen package's `repository.base_sha`), so a restart after a
        // crash records the identical baseline a fresh run would have
        // (`agalma-5fy.18`).
        if matches!(terminal, Terminal::Done) {
            if let Err(err) = crate::review::record_start_on_land(
                repo,
                record.task_id.as_str(),
                &package.repository.base_sha,
            ) {
                eprintln!(
                    "agalma: recovery review baseline for {}: {err}",
                    record.task_id.as_str()
                );
            }
        }
    }
    Ok(())
}

/// The cancellation settlement state of one execution, read from the ledger
/// event log through a bare executor (no activities needed).
fn cancellation_state_of(
    ledger: &LedgerHandle,
    execution: &ExecutionId,
) -> Result<CancellationState, ContractError> {
    let bare = Executor::new(
        ledger.clone(),
        ExecutorConfig {
            max_attempts: 2,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    bare.cancellation_state(execution)
}

/// Settle a cancelled execution during recovery and project it to the backlog.
///
/// The settlement probes run on the pinned activities reconstructed from the
/// frozen package so survivor reconciliation uses the right bindings. If the
/// frozen package cannot be reconstructed, a **bare** executor is used: its
/// probes are unconfirmed (conservative) and never dispatch a fresh effect.
#[allow(clippy::too_many_arguments)]
fn settle_cancelled_execution(
    repo: &Path,
    state_dir: &Path,
    ledger: &LedgerHandle,
    paid_available: bool,
    proxy_url: Option<String>,
    queue: &mut TaskQueueHandle,
    harness: HarnessHandle,
    stop: Rc<dyn StopControl>,
    record: &ExecutionRecord,
) -> Result<(), ContractError> {
    let (mut executor, _role_proxies) = match build_recovery_executor(
        ledger,
        state_dir,
        repo,
        paid_available,
        proxy_url,
        harness,
        stop,
        record,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!(
                "agalma: cancellation settle {} uses a bare executor \
                 (probes unconfirmed): {err}",
                record.execution_id
            );
            (
                Executor::new(
                    ledger.clone(),
                    ExecutorConfig {
                        max_attempts: 2,
                        execution_definition_version: EXECUTION_DEFINITION_VERSION,
                    },
                ),
                None,
            )
        }
    };
    let state = executor.settle_cancellation(&record.execution_id)?;
    let label = match state {
        CancellationState::Settled => "settled",
        CancellationState::Settling => "settling",
        CancellationState::Requested => "requested",
        CancellationState::None => "none",
    };
    let _ = queue.comment(
        &record.task_id,
        &format!(
            "phase=parked execution={} cancellation={label}",
            record.execution_id
        ),
    );
    let _ = queue.set_status(&record.task_id, "blocked");
    let _ = queue.add_label(&record.task_id, PARKED_LABEL);
    let _ = queue.comment(&record.task_id, "parked: Cancelled");
    Ok(())
}

/// Reconstruct a recovery executor bound to `record`'s frozen package and its
/// pinned activities. Errors when the package/role pins cannot be resolved; the
/// caller falls back to a bare (unconfirmed-probe) executor rather than driving.
///
/// Returns the role-gated proxies (`agalma-c31`) alongside the executor so the
/// caller can hold them for the executor's lifetime — a cancelled execution
/// whose survivors reconcile a verifier turn must still bill the verifier, not
/// the builder.
#[allow(clippy::too_many_arguments)]
fn build_recovery_executor(
    ledger: &LedgerHandle,
    state_dir: &Path,
    repo: &Path,
    paid_available: bool,
    proxy_url: Option<String>,
    harness: HarnessHandle,
    stop: Rc<dyn StopControl>,
    record: &ExecutionRecord,
) -> Result<(Executor<LedgerHandle>, Option<TaskRoleProxies>), ContractError> {
    let reference = admission_digest(ledger, &record.execution_id)?;
    let package = load_frozen_or_legacy(
        state_dir,
        reference.as_deref(),
        record.execution_id.as_str(),
    )?;
    if package.execution_id != record.execution_id.as_str() {
        return Err(ContractError::KnownFailure(format!(
            "frozen package {} does not match execution {}",
            package.execution_id, record.execution_id
        )));
    }
    let plan = role_plan_from_package(&package)?;
    let task_id = TaskId::new(package.task_id.clone());
    let task = task_from_package(&package, &task_id);
    verify_recovered_pins(state_dir, &package, &harness)?;
    let builder_prompt = builder_prompt_bytes(state_dir, &package)?;
    let role_proxies = task_role_proxies(state_dir, &package, paid_available)?;
    let (builder_proxy_url, verifier_proxy_url) =
        executor_proxy_urls(role_proxies.as_ref(), proxy_url);
    let executor = build_executor(
        ledger,
        state_dir,
        &plan,
        paid_available,
        builder_proxy_url,
        verifier_proxy_url,
        &task,
        repo,
        harness,
        &builder_prompt,
        stop,
    )?;
    Ok((executor, role_proxies))
}

/// The route pins an executor run selected (models and route labels). A fresh
/// admission resolves them from routing; recovery reads the pins from the frozen
/// package so a recovered run reuses its original routes rather than re-deciding
/// them against possibly-changed live inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RolePlan {
    builder_model: String,
    builder_route: String,
    verifier_model: String,
    verifier_route: String,
}

/// Test-only crash injection at the admission boundary. `before_admit` fires
/// once the package objects and document are durable but before the reference
/// commit (leaving harmless unreferenced objects); `after_admit` fires after the
/// reference commit but before any phase handler runs.
const PACKAGE_CRASH_ENV: &str = "AGALMA_PACKAGE_CRASH_AT";

fn package_crash(point: &str) {
    if std::env::var(PACKAGE_CRASH_ENV).ok().as_deref() == Some(point) {
        // SIGABRT: deliberately abnormal so nothing after this point executes.
        std::process::abort();
    }
}

/// Test-only crash injection for the review-baseline window
/// (`agalma-5fy.18`): `after_integrate` fires after the candidate's integrate
/// effect has landed but before the epic review baseline is durably recorded —
/// the window a later child could otherwise mask.
const REVIEW_BASELINE_CRASH_ENV: &str = "AGALMA_REVIEW_BASELINE_CRASH_AT";

fn review_baseline_crash(point: &str) {
    if std::env::var(REVIEW_BASELINE_CRASH_ENV).ok().as_deref() == Some(point) {
        // SIGABRT: deliberately abnormal so nothing after this point executes.
        std::process::abort();
    }
}

/// Build and persist the frozen `TaskPackage/v1` for `task`, returning its
/// reference and the package-admission event the caller commits with activation.
///
/// Ordering follows `docs/self-mutation-contracts.md` §SF: the execution
/// identity/lease exist (allocated by [`Executor::prepare`], a non-dispatchable
/// preparation), source SHA/scope/directive/role pins and every context object
/// are resolved once, objects are written and fsynced, and only then is the
/// package reference committed — atomically with activation and the first
/// dispatch intent, so no dispatch intent precedes package admission. A crash
/// after persistence but before activation leaves only unreferenced objects and
/// a non-dispatchable preparation that recovery parks.
fn admit_task(
    state_dir: &Path,
    repo: &Path,
    task: &Task,
    execution: &ExecutionId,
    generation: u32,
    lease: u32,
    plan: &RolePlan,
    harness: &HarnessHandle,
    evidence: &ClaimEvidence,
) -> Result<(PackageRef, ExecutionEvent), ContractError> {
    let definitions = BdTaskQueue::new(repo);
    let definition = definitions.task_definition(&task.task_id)?;
    let context = gather_admission_context(repo, &definitions, &definition)?;
    let base_sha = resolve_base_sha(repo, &task.base_ref)?;
    // SF.6: the protected verification inputs are operator-supplied in
    // `<state_dir>/protected-inputs.json` and frozen whole as `scope.oracle`.
    // Missing/mismatched inputs stop repository work; a repository file or task
    // text never authorizes an unprotected repository execution.
    let repository_root = repo.to_string_lossy().into_owned();
    let protected = ProtectedInputs::load(state_dir)?;
    let entry = protected.select(task.task_id.as_str(), &repository_root, &base_sha)?;
    let oracle_bytes = entry.freeze()?;
    let allowed_paths = entry.allowed_paths.clone();
    let protected_paths = entry.protected_paths.clone();
    let inputs = PackageInputs {
        execution_id: execution.as_str().to_string(),
        generation: generation as u64,
        lease_generation: lease as u64,
        repository_identity: repo
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "agalma".to_string()),
        base_sha,
        directive: directive_ref(task),
        authorization: authorization_input(task, evidence),
        roles: role_inputs(plan, harness, generation)?,
        instructions: context.instructions,
        shared_design: context.shared_design,
        parent_design: context.parent_design,
        dependency_handoffs: context.dependency_handoffs,
        allowed_paths,
        protected_paths,
        readable_artifacts: Vec::new(),
        oracle_bytes,
        oracle_media_type: "application/json".to_string(),
        max_attempts: task.max_attempts as u64,
        allowance_nusd: budget_nusd(task.budget_usd),
        quota_policy_digest: sha256_digest(b"quota-policy/v1:unknown-usage-deny"),
        recorded_at_unix_ms: now_ms(),
    };
    let built = build_package(&definition, inputs)?;
    let persisted = persist_package(state_dir, &built)?;
    package_crash("before_admit");

    let event = ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: ADMITTED_EVENT_KIND.to_string(),
        payload: json!({
            "package_digest": persisted.digest,
            "package_path": persisted.path,
            "task_id": task.task_id.as_str(),
            "generation": generation,
            "lease_generation": lease,
        }),
        recorded_at_unix_ms: now_ms(),
    };
    Ok((persisted, event))
}

/// The package digest recorded in an execution's `task.package_admitted` event.
fn admission_digest(
    ledger: &LedgerHandle,
    execution: &ExecutionId,
) -> Result<Option<String>, ContractError> {
    let events = ledger.events(execution)?;
    Ok(events
        .iter()
        .rev()
        .find(|event| event.kind == ADMITTED_EVENT_KIND)
        .and_then(|event| {
            event
                .payload
                .get("package_digest")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        }))
}

/// Verify a recovered frozen package's identity matches the execution record
/// whose landed child is about to be reconciled.
///
/// Both the execution and task identities are required. A validly hashed but
/// mismatched package/`task.package_admitted` reference must never be used to pin
/// another execution's (or task's) source base to this task's epic — that would
/// truncate the review (`agalma-5fy.18`). A mismatch is refused (fail closed)
/// before any projection reconciliation.
fn verify_recovered_package_identity(
    package: &TaskPackage,
    record: &ExecutionRecord,
) -> Result<(), ContractError> {
    if package.execution_id != record.execution_id.as_str() {
        return Err(ContractError::KnownFailure(format!(
            "frozen package execution {} does not match recovery record {}; refusing to pin a \
             review baseline from a mismatched package",
            package.execution_id, record.execution_id
        )));
    }
    if package.task_id != record.task_id.as_str() {
        return Err(ContractError::KnownFailure(format!(
            "frozen package task {} does not match recovery record {}; refusing to pin a review \
             baseline from a mismatched package",
            package.task_id, record.task_id
        )));
    }
    Ok(())
}

/// Park an execution that cannot be reconstructed, durably in the ledger, with a
/// best-effort backlog projection (the source issue may be gone).
fn park_execution(
    ledger: &LedgerHandle,
    record: &ExecutionRecord,
    reason: &str,
    queue: &mut TaskQueueHandle,
) -> Result<(), ContractError> {
    let state = ExecutionRecord {
        phase: ExecutionPhase::Parked,
        state: ExecutionState::Parked,
        ..record.clone()
    };
    let event = ExecutionEvent {
        execution_id: record.execution_id.clone(),
        sequence: 0,
        kind: "parked".to_string(),
        payload: json!({ "recovery": true, "reason": reason }),
        recorded_at_unix_ms: now_ms(),
    };
    // A cloned handle shares the same backend; `commit` needs a mutable handle.
    let mut ledger = ledger.clone();
    ledger.commit(CommitBatch {
        expected_revision: Some(record.revision),
        state: Some(state),
        events: vec![event],
        receipts: vec![],
        intents: vec![],
    })?;

    let _ = queue.comment(
        &record.task_id,
        &format!(
            "phase=parked execution={} recovery-park",
            record.execution_id
        ),
    );
    let _ = queue.set_status(&record.task_id, "blocked");
    let _ = queue.add_label(&record.task_id, PARKED_LABEL);
    let _ = queue.comment(&record.task_id, &format!("parked: {reason}"));
    Ok(())
}

/// Read the frozen builder prompt bytes from a package.
fn builder_prompt_bytes(state_dir: &Path, package: &TaskPackage) -> Result<Vec<u8>, ContractError> {
    let builder = package
        .roles
        .iter()
        .find(|pin| pin.role == Role::Builder)
        .ok_or_else(|| {
            legacy_park(
                package.task_id.as_str(),
                "frozen package has no builder role pin",
            )
        })?;
    read_artifact(state_dir, &builder.prompt)
}

/// Extract the builder/verifier route pins from a frozen package.
fn role_plan_from_package(package: &TaskPackage) -> Result<RolePlan, ContractError> {
    let builder = package
        .roles
        .iter()
        .find(|pin| pin.role == Role::Builder)
        .ok_or_else(|| {
            legacy_park(
                package.task_id.as_str(),
                "frozen package has no builder role pin",
            )
        })?;
    let verifier = package
        .roles
        .iter()
        .find(|pin| pin.role == Role::Verifier)
        .ok_or_else(|| {
            legacy_park(
                package.task_id.as_str(),
                "frozen package has no verifier role pin",
            )
        })?;
    Ok(RolePlan {
        builder_model: builder.model.clone(),
        builder_route: builder.route_id.clone(),
        verifier_model: verifier.model.clone(),
        verifier_route: verifier.route_id.clone(),
    })
}

/// Rebuild the phase-facing task view from a frozen package. The base ref is the
/// frozen `repository.base_sha` (a commit-ish `git rev-parse` resolves), so
/// recovery never re-resolves the base from the live repository.
fn task_from_package(package: &TaskPackage, task_id: &TaskId) -> Task {
    Task {
        task_id: task_id.clone(),
        title: package.definition.title.clone(),
        directive: package.directive.id.clone(),
        target: "agalma".to_string(),
        base_ref: package.repository.base_sha.clone(),
        acceptance: package.definition.acceptance.clone(),
        budget_usd: Some(package.budget.allowance_nusd as f64 / 1e9),
        priority: TaskPriority::Normal,
        family: TaskFamily::Feature,
        max_attempts: package.max_attempts as u32,
        raw_block: package.definition.machine_block.clone(),
    }
}

/// Resolve `base_ref` to a commit SHA in `repo` (frozen into the package).
fn resolve_base_sha(repo: &Path, base_ref: &str) -> Result<String, ContractError> {
    let rev = if base_ref.trim().is_empty() {
        "HEAD"
    } else {
        base_ref
    };
    let out = std::process::Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ContractError::KnownFailure(format!("git rev-parse {rev}: {e}")))?;
    if !out.status.success() {
        return Err(ContractError::KnownFailure(format!(
            "git rev-parse {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The directive reference for `task` (id, parsed version, id digest).
fn directive_ref(task: &Task) -> DirectiveRef {
    let version = task
        .directive
        .rsplit_once("/v")
        .and_then(|(_, version)| version.parse::<u64>().ok())
        .unwrap_or(1);
    DirectiveRef {
        id: task.directive.clone(),
        version,
        digest: sha256_digest(task.directive.as_bytes()),
    }
}

/// The authorization input: a human-bootstrap origin with the claim evidence as
/// its receipt. Receipt provenance is recorded, not trusted as authority.
fn authorization_input(task: &Task, evidence: &ClaimEvidence) -> AuthorizationInput {
    let receipt = json!({
        "format": "AuthorizationReceipt/v1",
        "origin": "human_bootstrap",
        "task_id": task.task_id.as_str(),
        "status": evidence.status,
        "assignee": evidence.assignee,
        "started_at": evidence.started_at,
    });
    AuthorizationInput {
        origin: AuthorizationOrigin::HumanBootstrap,
        policy_digest: sha256_digest(b"human-bootstrap/v1"),
        receipt_source: format!("bd:{}/claim", task.task_id.as_str()),
        receipt_media_type: "application/json".to_string(),
        receipt_bytes: serde_json::to_vec(&receipt).unwrap_or_default(),
    }
}

/// The builder/verifier role pins for a fresh admission, carrying the actual
/// selected harness binding (identity, API, API version, implementation version,
/// real prepared generation, and config digest) and the compiled prompt bytes.
fn role_inputs(
    plan: &RolePlan,
    harness: &HarnessHandle,
    generation: u32,
) -> Result<Vec<RoleInput>, ContractError> {
    let kind = harness.borrow().kind();
    Ok(vec![
        RoleInput::new(
            Role::Builder,
            plan.builder_route.clone(),
            plan.builder_model.clone(),
            binding_pin(kind, generation, &plan.builder_model)?,
            BUILDER_PROMPT.as_bytes().to_vec(),
            "text/plain",
        ),
        RoleInput::new(
            Role::Verifier,
            plan.verifier_route.clone(),
            plan.verifier_model.clone(),
            binding_pin(kind, generation, &plan.verifier_model)?,
            VERIFIER_PROMPT.as_bytes().to_vec(),
            "text/plain",
        ),
    ])
}

/// The binding pin for `kind`/`model` at `generation`, taken from the real
/// static binding plan (identity, API/version, implementation version,
/// generation, and config digest) rather than a synthetic placeholder.
pub fn binding_pin(
    kind: HarnessKind,
    generation: u32,
    model: &str,
) -> Result<BindingPin, ContractError> {
    let plan = plan_binding(kind, generation, model)?;
    Ok(BindingPin {
        binding_id: plan.binding.binding_id.as_str().to_string(),
        api: plan.binding.component_api,
        api_version: plan.binding.api_version as u64,
        implementation_version: plan.binding.implementation_version,
        generation: plan.binding.generation,
        config_digest: plan.binding.config_hash,
    })
}

/// Verify a recovered package's pinned roles against the current process
/// selection before any dispatch. Every required pin field is validated
/// (identity, API, API version, implementation version, generation, config
/// digest); a changed harness selector or a changed compiled verifier prompt
/// must refuse dispatch, not silently substitute.
pub fn verify_recovered_pins(
    state_dir: &Path,
    package: &TaskPackage,
    harness: &HarnessHandle,
) -> Result<(), ContractError> {
    let kind = harness.borrow().kind();
    for pin in &package.roles {
        let expected = binding_pin(kind, pin.harness.generation as u32, &pin.model)?;
        if expected != pin.harness {
            return Err(FailureCode::StateUnavailable.failure(
                package.task_id.as_str(),
                format!(
                    "frozen {:?} binding pin {:?} is incompatible with the current harness selection",
                    pin.role, pin.harness.binding_id
                ),
            ));
        }
    }
    let verifier = package
        .roles
        .iter()
        .find(|pin| pin.role == Role::Verifier)
        .ok_or_else(|| {
            legacy_park(
                package.task_id.as_str(),
                "frozen package has no verifier role pin",
            )
        })?;
    if read_artifact(state_dir, &verifier.prompt)? != VERIFIER_PROMPT.as_bytes() {
        return Err(FailureCode::StateUnavailable.failure(
            package.task_id.as_str(),
            "frozen verifier prompt does not match the compiled prompt",
        ));
    }
    Ok(())
}

/// Convert an optional USD budget into the package's nano-USD allowance.
fn budget_nusd(budget_usd: Option<f64>) -> u64 {
    budget_usd
        .map(|usd| (usd * 1e9).round().max(0.0) as u64)
        .unwrap_or(0)
}

/// Build a per-task executor with repo-mode activities and the same ledger.
#[allow(clippy::too_many_arguments)]
fn build_executor(
    ledger: &LedgerHandle,
    state_dir: &Path,
    plan: &RolePlan,
    paid_available: bool,
    proxy_url: Option<String>,
    verifier_proxy_url: Option<String>,
    task: &Task,
    repo: &Path,
    harness: HarnessHandle,
    builder_prompt_bytes: &[u8],
    stop: Rc<dyn StopControl>,
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
        plan,
        paid_available,
        proxy_url,
        verifier_proxy_url,
        task,
        repo,
        harness,
        builder_prompt_bytes,
        stop,
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

/// Assemble repo-mode phase wiring and materialize the base builder prompt under
/// the state dir.
#[allow(clippy::too_many_arguments)]
fn phase_deps(
    state_dir: &Path,
    plan: &RolePlan,
    paid_available: bool,
    proxy_url: Option<String>,
    verifier_proxy_url: Option<String>,
    task: &Task,
    repo: &Path,
    harness: HarnessHandle,
    builder_prompt_bytes: &[u8],
    stop: Rc<dyn StopControl>,
) -> Result<PhaseDeps, ContractError> {
    let prompts_dir = state_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)
        .map_err(|e| ContractError::KnownFailure(format!("cannot create prompts dir: {e}")))?;
    let builder_prompt = prompts_dir.join("builder.md");
    // Always write the pinned prompt bytes: a fresh admission writes the compiled
    // base prompt, recovery writes the frozen prompt bytes, so a mutated file
    // cannot substitute a different prompt.
    std::fs::write(&builder_prompt, builder_prompt_bytes).map_err(|e| {
        ContractError::KnownFailure(format!(
            "cannot write builder prompt {}: {e}",
            builder_prompt.display()
        ))
    })?;

    let (_, toolchain_env) = toolchain()?;
    Ok(PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture: repo.to_path_buf(),
        model: plan.builder_model.clone(),
        verifier_model: plan.verifier_model.clone(),
        route: plan.builder_route.clone(),
        verifier_route: plan.verifier_route.clone(),
        role: "builder".to_string(),
        builder_prompt,
        proxy_url,
        verifier_proxy_url,
        paid_proxy: paid_available,
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
        stop,
    })
}

/// Lease generation pinned in an execution's `execution_prepared` /
/// `execution_created` / `execution_activated` event (fallback: its task
/// generation).
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
                if !matches!(
                    event.kind.as_str(),
                    "execution_prepared" | "execution_created" | "execution_activated"
                ) {
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
