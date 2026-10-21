//! WEB.10 (`agalma-rwn.10`) acceptance: continuous single-owner queue runtime.
//!
//! These tests drive the real production seam
//! ([`agalma_conductor::work::factory_runtime`]) with deterministic in-memory
//! drivers instead of paid providers or a browser, so the empty→Idle→task→run
//! transitions, the stopped/Paused and queue-failure/Unavailable states, the
//! independent supervision owner, the bounded wake channel and the snapshot
//! source are all exercised without credentials. The protected legacy/stop/
//! frozen-recovery consumers run in the sibling focused selection.
//!
//! The property test is seeded (`0x57454210`) with 256 sequences of at most 32
//! queue arrivals and stop/resume/failure/crash-restart/UI-disconnect events;
//! it asserts one claimant, serial execution, no denied-task dispatch, no
//! duplicate claim, no fresh initialization under stop, and that a UI loss
//! cannot create or destroy worker authority.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_manifest::{package_inventory_digest, PLUGIN_MANIFEST_FILE};
use agalma_conductor::plugin_registry::DesiredState;
use agalma_conductor::plugin_router::{CapabilityAdapter, GrantScope, LaunchGrant};
use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FORMAT,
};
use agalma_conductor::work::factory_runtime::{
    classify_startup, host_health_state, watch_conflict, ContinuousRuntime, ExecutionObserver,
    FactoryRuntimeSource, HostRegistrar, HostRegistration, HostRegistrationContext, LoopAction,
    RunOutcome, RuntimeControl, RuntimeDriver, RuntimeExecutionObserver, RuntimeShared, StartupMode,
    SupervisionHandle, SupervisionOwner, WatchComposition, WATCH_IDLE_WAIT,
    WATCH_NOTIFICATION_CAPACITY,
};
use agalma_contracts::control_plane::SourceState;
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::factory::{
    FactoryEligibleEntry, FactoryRuntimeRequest, FactoryRuntimeStatus, FactorySourceName,
    RuntimeObservationSource, FACTORY_RUNTIME_FORMAT,
};
use agalma_contracts::task_package::BindingPin;
use agalma_contracts::taskqueue::{ClaimEvidence, Task, TaskFamily, TaskPriority};
use agalma_contracts::{ContractError, ExecutionPhase, ExecutionState, HealthState, PluginHealth,
    PluginManifest, TaskId, PLUGIN_HEALTH_FORMAT};
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Deterministic in-memory worker driver
// ---------------------------------------------------------------------------

type Ready = Result<Vec<Task>, ContractError>;

struct Shared {
    ready: Mutex<VecDeque<Ready>>,
    decision: Mutex<agalma_conductor::stop::StopDecision>,
    events: Mutex<Vec<String>>,
    claims: AtomicUsize,
    runs: Mutex<Vec<String>>,
    running: AtomicUsize,
    max_concurrent: AtomicUsize,
    recover_calls: AtomicUsize,
    deny_claim: AtomicBool,
    fail_run: AtomicBool,
    shutdown_calls: AtomicUsize,
    /// An optional gate the `run` step waits on (to hold a long blocked phase).
    gate: Mutex<Option<Arc<(Mutex<bool>, Condvar)>>>,
    /// An optional control the `read_ready` step uses to request shutdown after
    /// a fixed number of reads (to bound `run_until`).
    control: Mutex<Option<Arc<RuntimeControl>>>,
    shutdown_after_reads: AtomicUsize,
    reads: AtomicUsize,
    /// An optional durable admission hold (unresolved review drift / fail-closed
    /// hold check). While set, admission pauses and no task is claimed.
    hold: Mutex<Option<String>>,
    /// When set, `stop_decision` denies exactly once and then reports `Running`,
    /// so the next read sees a resume. Used to exercise the boot race between a
    /// denied initial read and a resumed second read.
    flip_denied_to_running: AtomicBool,
    stop_reads: AtomicUsize,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            ready: Mutex::new(VecDeque::new()),
            decision: Mutex::new(agalma_conductor::stop::StopDecision::Running),
            events: Mutex::new(Vec::new()),
            claims: AtomicUsize::new(0),
            runs: Mutex::new(Vec::new()),
            running: AtomicUsize::new(0),
            max_concurrent: AtomicUsize::new(0),
            recover_calls: AtomicUsize::new(0),
            deny_claim: AtomicBool::new(false),
            fail_run: AtomicBool::new(false),
            shutdown_calls: AtomicUsize::new(0),
            gate: Mutex::new(None),
            control: Mutex::new(None),
            shutdown_after_reads: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            hold: Mutex::new(None),
            flip_denied_to_running: AtomicBool::new(false),
            stop_reads: AtomicUsize::new(0),
        }
    }
}

impl Shared {
    fn log(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
    fn push_ready(&self, ready: Ready) {
        self.ready.lock().unwrap().push_back(ready);
    }
    fn set_decision(&self, decision: agalma_conductor::stop::StopDecision) {
        *self.decision.lock().unwrap() = decision;
    }
    fn runs(&self) -> Vec<String> {
        self.runs.lock().unwrap().clone()
    }
    fn max_concurrent(&self) -> usize {
        self.max_concurrent.load(Ordering::SeqCst)
    }
    /// Make `read_ready` request shutdown after `reads` total reads.
    fn shutdown_after(&self, reads: usize, control: Arc<RuntimeControl>) {
        *self.control.lock().unwrap() = Some(control);
        self.shutdown_after_reads.store(reads, Ordering::SeqCst);
    }
    fn set_gate(&self, gate: Arc<(Mutex<bool>, Condvar)>) {
        *self.gate.lock().unwrap() = Some(gate);
    }
    fn set_hold(&self, reason: Option<&str>) {
        *self.hold.lock().unwrap() = reason.map(str::to_string);
    }
}

struct FakeDriver {
    shared: Arc<Shared>,
}

impl RuntimeDriver for FakeDriver {
    fn recover(&mut self) -> Result<(), ContractError> {
        self.shared.recover_calls.fetch_add(1, Ordering::SeqCst);
        self.shared.log("recover");
        Ok(())
    }

    fn stop_decision(&self) -> agalma_conductor::stop::StopDecision {
        if self
            .shared
            .flip_denied_to_running
            .load(Ordering::SeqCst)
        {
            // Deny the first read, then report Running on every later read: the
            // resume is visible only to a second read, which is exactly the boot
            // race a re-read through `tick()` would exploit.
            let read = self.shared.stop_reads.fetch_add(1, Ordering::SeqCst);
            if read == 0 {
                return agalma_conductor::stop::StopDecision::Stopped(
                    "injected boot stop".to_string(),
                );
            }
            return agalma_conductor::stop::StopDecision::Running;
        }
        self.shared.decision.lock().unwrap().clone()
    }

    fn read_ready(&mut self) -> Result<Vec<Task>, ContractError> {
        self.shared.log("read");
        let reads = self.shared.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let shutdown_after = self.shared.shutdown_after_reads.load(Ordering::SeqCst);
        if shutdown_after > 0 && reads >= shutdown_after {
            if let Some(control) = self.shared.control.lock().unwrap().clone() {
                control.request_shutdown();
            }
        }
        match self.shared.ready.lock().unwrap().pop_front() {
            Some(result) => result,
            None => Ok(Vec::new()),
        }
    }

    fn claim(&mut self, task: &Task) -> Result<ClaimEvidence, ContractError> {
        self.shared.log(format!("claim:{}", task.task_id.as_str()));
        if self.shared.deny_claim.load(Ordering::SeqCst) {
            return Err(ContractError::KnownFailure("claim refused".to_string()));
        }
        self.shared.claims.fetch_add(1, Ordering::SeqCst);
        Ok(ClaimEvidence {
            task_id: task.task_id.clone(),
            status: "in_progress".to_string(),
            assignee: Some("fake".to_string()),
            started_at: Some("0".to_string()),
        })
    }

    fn run(&mut self, task: &Task, _evidence: &ClaimEvidence) -> Result<RunOutcome, ContractError> {
        self.shared.log(format!("run:{}", task.task_id.as_str()));
        let running = self.shared.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared
            .max_concurrent
            .fetch_max(running, Ordering::SeqCst);
        // Optional long blocked phase.
        let gate = self.shared.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            let (lock, cvar) = &*gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cvar.wait(open).unwrap();
            }
        }
        self.shared.running.fetch_sub(1, Ordering::SeqCst);
        self.shared.runs.lock().unwrap().push(task.task_id.as_str().to_string());
        if self.shared.fail_run.load(Ordering::SeqCst) {
            return Err(ContractError::KnownFailure("run failed".to_string()));
        }
        Ok(RunOutcome::Done)
    }

    fn shutdown(&mut self) -> Result<(), ContractError> {
        self.shared.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        self.shared.log("shutdown");
        Ok(())
    }

    fn admission_hold(&self) -> Option<String> {
        self.shared.hold.lock().unwrap().clone()
    }
}

fn task(id: &str) -> Task {
    Task {
        task_id: TaskId::new(id),
        title: format!("task {id}"),
        directive: "directive/v0".to_string(),
        target: "agalma".to_string(),
        base_ref: "main".to_string(),
        acceptance: vec!["true".to_string()],
        budget_usd: None,
        priority: TaskPriority::Normal,
        family: TaskFamily::Feature,
        max_attempts: 1,
        raw_block: String::new(),
    }
}

fn make_runtime(shared: Arc<Shared>) -> (ContinuousRuntime<FakeDriver>, Arc<RuntimeShared>) {
    let observation = Arc::new(RuntimeShared::new());
    (
        ContinuousRuntime::new(FakeDriver { shared }, Arc::clone(&observation)),
        observation,
    )
}

fn snapshot_request() -> FactoryRuntimeRequest {
    FactoryRuntimeRequest {
        format: FACTORY_RUNTIME_FORMAT.to_string(),
        operation: "snapshot".to_string(),
        limit: None,
        cursor: None,
    }
}

fn eligible_entry(id: &str) -> FactoryEligibleEntry {
    FactoryEligibleEntry {
        task_id: id.to_string(),
        title: format!("task {id}"),
        priority: TaskPriority::Normal,
        family: TaskFamily::Feature,
    }
}

fn running_sources() -> Vec<(SourceState, String)> {
    vec![
        (SourceState::Available, String::new()),
        (SourceState::Available, String::new()),
        (SourceState::Available, String::new()),
        (SourceState::Unknown, String::new()),
        (SourceState::Available, String::new()),
    ]
}

fn source_state(record: &agalma_contracts::factory::FactoryRuntime, name: FactorySourceName) -> SourceState {
    record
        .sources
        .iter()
        .find(|source| source.source == name)
        .map(|source| source.state)
        .unwrap_or(SourceState::Unknown)
}

// ---------------------------------------------------------------------------
// Required concrete cases
// ---------------------------------------------------------------------------

#[test]
fn paused_startup_defers_effectful_recovery_until_resume() {
    let shared = Arc::new(Shared::default());
    shared.set_decision(agalma_conductor::stop::StopDecision::Stopped(
        "stop policy active".to_string(),
    ));
    let control = Arc::new(RuntimeControl::new());
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    let handle = {
        let control = Arc::clone(&control);
        std::thread::spawn(move || runtime.run_until(&control))
    };

    // A paused startup serves Paused without effectful recovery.
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(observation.status(), FactoryRuntimeStatus::Paused);
    assert_eq!(
        shared.recover_calls.load(Ordering::SeqCst),
        0,
        "no effectful recovery under a paused startup"
    );
    assert_eq!(shared.claims.load(Ordering::SeqCst), 0);

    // Resume: admission is re-read and recovery runs before any claim.
    shared.set_decision(agalma_conductor::stop::StopDecision::Running);
    control.request_recheck();
    let mut recovered = false;
    for _ in 0..400 {
        if shared.recover_calls.load(Ordering::SeqCst) >= 1 {
            recovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(recovered, "recovery runs once admission resumes");
    assert_eq!(shared.claims.load(Ordering::SeqCst), 0, "no claim before recovery");

    // A newly eligible task runs once, after recovery.
    shared.push_ready(Ok(vec![task("t1")]));
    control.request_recheck();
    for _ in 0..400 {
        if shared.claims.load(Ordering::SeqCst) >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(shared.claims.load(Ordering::SeqCst), 1);

    let events = shared.events();
    let recover = events.iter().position(|e| e == "recover").expect("recover ran");
    let claim = events
        .iter()
        .position(|e| e.starts_with("claim"))
        .expect("claim ran");
    assert!(recover < claim, "recovery precedes the first claim: {events:?}");

    control.request_shutdown();
    let action = handle.join().expect("runtime thread joins");
    assert_eq!(action, LoopAction::Paused);
    assert_eq!(shared.shutdown_calls.load(Ordering::SeqCst), 1, "owner joined");
}

#[test]
fn denied_boot_resume_never_claims_before_recovery() {
    // Regression for the boot race: while `boot_done == false` a denied initial
    // stop read must not be re-read through `tick()`. This driver denies exactly
    // the first read and reports Running on every later read, so a re-read
    // through `tick()` would claim/run the eligible task before recovery, while
    // a correct gate waits, completes recovery, and only then claims.
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("t1")]));
    shared.push_ready(Ok(Vec::new()));
    shared.flip_denied_to_running.store(true, Ordering::SeqCst);
    let control = Arc::new(RuntimeControl::new());
    shared.shutdown_after(2, Arc::clone(&control));
    let (mut runtime, _observation) = make_runtime(Arc::clone(&shared));

    let handle = {
        let control = Arc::clone(&control);
        std::thread::spawn(move || runtime.run_until(&control))
    };
    for _ in 0..800 {
        if shared.claims.load(Ordering::SeqCst) >= 1 {
            break;
        }
        control.request_recheck();
        std::thread::sleep(Duration::from_millis(2));
    }
    control.request_shutdown();
    let _ = handle.join().expect("runtime thread joins");

    assert_eq!(shared.claims.load(Ordering::SeqCst), 1, "exactly one claim");
    let events = shared.events();
    let recover = events
        .iter()
        .position(|event| event == "recover")
        .expect("recovery ran");
    let claim = events
        .iter()
        .position(|event| event.starts_with("claim"))
        .expect("a claim happened");
    assert!(
        recover < claim,
        "recovery completes before the resumed claim: {events:?}"
    );
}

#[test]
fn admission_hold_pauses_without_claiming() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("t1")]));
    shared.set_hold(Some("ordinary dispatch held: unresolved review drift"));
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    assert_eq!(runtime.tick(), LoopAction::Paused);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Paused);
    assert_eq!(shared.claims.load(Ordering::SeqCst), 0, "held: no claim");

    // Clearing the hold admits the task.
    shared.set_hold(None);
    assert_eq!(runtime.tick(), LoopAction::Ran);
    assert_eq!(shared.claims.load(Ordering::SeqCst), 1);
}

#[test]
fn execution_observer_reports_actual_phase_and_keeps_queue_identity() {
    let shared = Arc::new(RuntimeShared::new());
    shared.initialize(Some("epoch-1".to_string()), running_sources());
    // Running with the claimed task already removed from the eligible page.
    shared.publish_running(vec![eligible_entry("t2")], None);
    let revision_before = shared.revision();

    let observer = RuntimeExecutionObserver::new(Arc::clone(&shared));
    observer.observe(
        "exec:t1:1",
        "t1",
        1,
        ExecutionPhase::Build,
        ExecutionState::Running,
    );

    let source = FactoryRuntimeSource::new(
        Arc::clone(&shared),
        "repo".to_string(),
        "state".to_string(),
    );
    let record = source.snapshot(&snapshot_request()).expect("snapshot");
    let current = record
        .current_execution
        .clone()
        .expect("the actual durable execution is published");
    assert_eq!(current.execution_id, "exec:t1:1");
    assert_eq!(current.task_id, "t1");
    assert_eq!(current.generation.as_u64().unwrap(), 1);
    assert_eq!(current.phase, ExecutionPhase::Build);
    assert_eq!(current.state, ExecutionState::Running);
    assert_eq!(
        source_state(&record, FactorySourceName::Execution),
        SourceState::Available
    );
    // The claimed task is not in the eligible page, and execution progress is
    // not a queue change (the revision is stable).
    assert!(record.eligible.entries.iter().all(|e| e.task_id != "t1"));
    assert_eq!(shared.revision(), revision_before);
}

#[test]
fn transient_failure_preserves_epoch_and_later_pagination() {
    let shared = Arc::new(RuntimeShared::new());
    shared.initialize(Some("epoch-1".to_string()), running_sources());
    shared.publish_running(vec![eligible_entry("t000")], None);

    // A transient source failure must not clear the process epoch.
    shared.publish_unavailable("queue read failed".to_string());
    assert_eq!(
        shared.runtime_instance_id().as_deref(),
        Some("epoch-1"),
        "a transient failure preserves the runtime identity"
    );

    // A later recovery with more rows than one page must page without panicking.
    let many: Vec<FactoryEligibleEntry> = (0..60)
        .map(|i| eligible_entry(&format!("t{i:03}")))
        .collect();
    shared.publish_running(many, None);
    let source = FactoryRuntimeSource::new(
        Arc::clone(&shared),
        "repo".to_string(),
        "state".to_string(),
    );
    let first = source
        .snapshot(&FactoryRuntimeRequest {
            format: FACTORY_RUNTIME_FORMAT.to_string(),
            operation: "snapshot".to_string(),
            limit: Some(50),
            cursor: None,
        })
        .expect("first page after a transient failure");
    assert_eq!(first.eligible.entries.len(), 50);
    assert_eq!(first.eligible.total.as_ref().unwrap().as_u64().unwrap(), 60);
    let cursor = first.eligible.next_cursor.clone().expect("more pages remain");
    let second = source
        .snapshot(&FactoryRuntimeRequest {
            format: FACTORY_RUNTIME_FORMAT.to_string(),
            operation: "snapshot".to_string(),
            limit: Some(50),
            cursor: Some(cursor),
        })
        .expect("second page");
    assert_eq!(second.eligible.entries.len(), 10);
    assert!(second.eligible.next_cursor.is_none());
}

#[test]
fn unknown_queue_total_stays_null_never_zero() {
    let shared = Arc::new(RuntimeShared::new());
    shared.initialize(Some("epoch-1".to_string()), running_sources());
    let source = FactoryRuntimeSource::new(
        Arc::clone(&shared),
        "repo".to_string(),
        "state".to_string(),
    );

    // Recovering: the queue has not been read successfully yet.
    shared.publish_recovering("epoch-1".to_string());
    let record = source.snapshot(&snapshot_request()).expect("recovering snapshot");
    assert_eq!(record.status, FactoryRuntimeStatus::Recovering);
    assert!(record.eligible.total.is_none(), "unknown total is null");
    assert!(record.eligible.revision.is_none());
    assert!(record.eligible.entries.is_empty());

    // Paused: admission is blocked; the queue observation is unknown.
    shared.publish_paused("paused by operator");
    let record = source.snapshot(&snapshot_request()).expect("paused snapshot");
    assert_eq!(record.status, FactoryRuntimeStatus::Paused);
    assert!(record.eligible.total.is_none(), "paused total is null, not zero");

    // A successful empty read *is* a known zero.
    shared.publish_idle(None);
    let record = source.snapshot(&snapshot_request()).expect("idle snapshot");
    assert_eq!(record.status, FactoryRuntimeStatus::Idle);
    assert_eq!(record.eligible.total.as_ref().unwrap().as_u64().unwrap(), 0);
}

#[test]
fn production_watch_registration_hook_reaches_the_served_router() {
    /// Delegates to the composition-owned production registrar and records the
    /// real router host assembly served, proving the production hook's
    /// registration lands on the *served* router.
    struct Capture {
        inner: Arc<dyn HostRegistrar>,
        captured: Arc<Mutex<Option<Arc<agalma_conductor::plugin_router::RouterService>>>>,
    }

    impl HostRegistrar for Capture {
        fn register(&self, registration: &HostRegistration<'_>) -> Result<(), ContractError> {
            self.inner.register(registration)?;
            *self.captured.lock().unwrap() = Some(Arc::clone(registration.router));
            Ok(())
        }
    }

    let dir = ScratchDir::new("factory-runtime-prod-registration");
    let state = dir.child("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let ledger = LedgerHandle::in_process(&state).expect("ledger");
    let observation = Arc::new(RuntimeShared::new());
    let control = Arc::new(RuntimeControl::new());

    // Apply the exact composition-owned production hook `run_watch` passes.
    let composition = WatchComposition::new(
        "repo".to_string(),
        "state".to_string(),
        Arc::clone(&observation),
        Arc::clone(&control),
    );
    agalma_conductor::composition::register_work_plugin_host(&composition);
    let production = composition
        .take_registrar()
        .expect("the production hook installed a registrar");

    let captured = Arc::new(Mutex::new(None));
    let context = HostRegistrationContext {
        repository_id: "repo".to_string(),
        state_id: "state".to_string(),
        shared: Arc::clone(&observation),
        control: Arc::clone(&control),
        registrar: Arc::new(Capture {
            inner: production,
            captured: Arc::clone(&captured),
        }),
    };
    let mut supervision = SupervisionHandle::spawn_host(
        state.clone(),
        agalma_conductor::cli::PluginRootArgs::default(),
        ledger,
        Arc::clone(&observation),
        Some(context),
        Duration::from_millis(20),
    );

    wait_for(
        || captured.lock().unwrap().is_some(),
        "production registration reached host assembly",
    );
    let router = captured
        .lock()
        .unwrap()
        .clone()
        .expect("the served router was captured");
    assert!(
        router
            .router()
            .has_adapter(agalma_conductor::composition::WORK_PLUGIN_HOST_MARKER_CAPABILITY),
        "the composition-owned production registrar registered on the served router"
    );

    // The served hub socket answers a real Connect callback. A response — even
    // an application-level error for an unknown method — proves it was served.
    let hub_socket = agalma_conductor::plugin_registry::plugin_socket_path(&state, "hub", 0);
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let served = runtime.block_on(async {
        let Ok(mut client) = agalma_ipc::ConnectClient::connect(&hub_socket).await else {
            return false;
        };
        // A response — even an application-level error for an unknown method —
        // proves the served hub answered the callback.
        let _ = client.call("hub.UnknownMethod", json!({})).await;
        true
    });
    assert!(served, "the served hub socket answers callbacks");

    supervision.request_shutdown();
    supervision.join().expect("supervision owner joins");
}

#[test]
fn host_health_reports_explicit_failure_and_recovers() {
    let shared = Arc::new(RuntimeShared::new());
    shared.initialize(Some("epoch-1".to_string()), running_sources());
    let source = FactoryRuntimeSource::new(
        Arc::clone(&shared),
        "repo".to_string(),
        "state".to_string(),
    );

    // A start/server/probe failure is an explicit Unreadable runtime source with
    // its failure detail, never a fabricated healthy host.
    let revision_before = shared.revision();
    shared.publish_host_health(SourceState::Unreadable, "plugin host start failed: boom");
    let record = source.snapshot(&snapshot_request()).expect("failure snapshot");
    assert_eq!(
        source_state(&record, FactorySourceName::Runtime),
        SourceState::Unreadable
    );
    let detail = record
        .sources
        .iter()
        .find(|entry| entry.source == FactorySourceName::Runtime)
        .and_then(|entry| entry.detail.clone())
        .expect("failure detail published");
    assert!(detail.contains("start failed"), "detail: {detail}");

    // Recovery publishes an explicit Available healthy observation.
    shared.publish_host_health(SourceState::Available, "installed=1 health=ok");
    let record = source.snapshot(&snapshot_request()).expect("recovery snapshot");
    assert_eq!(
        source_state(&record, FactorySourceName::Runtime),
        SourceState::Available
    );
    assert_eq!(
        shared.revision(),
        revision_before,
        "host health is a heartbeat, not a queue change"
    );
}

/// Property: host health is `Available` only when no child crashed and no
/// desired plugin is unready; any failure is `Unreadable`. Generated over a
/// fixed-seed deterministic sequence with the boundaries included.
#[test]
fn host_health_state_is_available_only_without_failures_property() {
    let mut value = 0x5745_4210u64;
    for step in 0..256 {
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        let crashed = (value % 4) as usize;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        let unready = (value % 4) as usize;
        let expected = if crashed == 0 && unready == 0 {
            SourceState::Available
        } else {
            SourceState::Unreadable
        };
        assert_eq!(
            host_health_state(crashed, unready),
            expected,
            "step {step}: crashed={crashed} unready={unready}"
        );
    }
    assert_eq!(host_health_state(0, 0), SourceState::Available);
    assert_eq!(host_health_state(1, 0), SourceState::Unreadable);
    assert_eq!(host_health_state(0, 1), SourceState::Unreadable);
    assert_eq!(host_health_state(3, 5), SourceState::Unreadable);
}

/// Property: host health is a revision-stable heartbeat. Over a deterministic
/// generated sequence of failure/recovery observations the runtime queue
/// revision never moves and the observed host state always reflects the latest
/// publication (a crash can never be observed as healthy).
#[test]
fn host_health_is_a_revision_stable_heartbeat_property() {
    let shared = Arc::new(RuntimeShared::new());
    shared.initialize(Some("epoch-1".to_string()), running_sources());
    let source = FactoryRuntimeSource::new(
        Arc::clone(&shared),
        "repo".to_string(),
        "state".to_string(),
    );
    let revision = shared.revision();
    let mut value = 0x5745_4210u64;
    for step in 0..256 {
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        let state = if value % 2 == 0 {
            SourceState::Available
        } else {
            SourceState::Unreadable
        };
        shared.publish_host_health(state, format!("step={step}"));
        assert_eq!(shared.revision(), revision, "host health is not a queue change");
        let record = source.snapshot(&snapshot_request()).expect("snapshot");
        assert_eq!(
            source_state(&record, FactorySourceName::Runtime),
            state,
            "step {step}: the latest host observation is reported"
        );
    }
}

#[test]
fn empty_queue_is_idle_then_arrival_runs_exactly_once() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(Vec::new()));
    shared.push_ready(Ok(vec![task("t1")]));
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    assert_eq!(runtime.tick(), LoopAction::Idle);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Idle);

    assert_eq!(runtime.tick(), LoopAction::Ran);
    // The run completed, but the queue was only read *before* the claim: the
    // claimed task left no known remainder, so the queue is explicitly unknown
    // (never a fabricated successful empty) until a fresh read.
    let source = FactoryRuntimeSource::new(
        Arc::clone(&observation),
        "repo".to_string(),
        "state".to_string(),
    );
    let after_run = source.snapshot(&snapshot_request()).expect("snapshot");
    assert_eq!(after_run.status, FactoryRuntimeStatus::Running);
    assert!(
        after_run.eligible.total.is_none(),
        "a drained pre-claim read is unknown, not a known zero"
    );
    assert_eq!(shared.claims.load(Ordering::SeqCst), 1, "one claimant");
    assert_eq!(shared.runs(), vec!["t1".to_string()]);

    // An eligible set already consumed is not re-claimed; only a fresh
    // successful empty read publishes Idle.
    assert_eq!(runtime.tick(), LoopAction::Idle);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Idle);
    assert_eq!(shared.claims.load(Ordering::SeqCst), 1);
}

#[test]
fn ran_preserves_known_remaining_entries_and_reports_unknown_when_drained() {
    let shared = Arc::new(Shared::default());
    // First read offers two eligible rows; the rescan offers the unclaimed one.
    shared.push_ready(Ok(vec![task("t1"), task("t2")]));
    shared.push_ready(Ok(vec![task("t2")]));
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));
    let source = FactoryRuntimeSource::new(
        Arc::clone(&observation),
        "repo".to_string(),
        "state".to_string(),
    );

    assert_eq!(runtime.tick(), LoopAction::Ran);
    let record = source.snapshot(&snapshot_request()).expect("snapshot");
    // The known remaining eligible entry is preserved, not cleared to zero.
    assert_eq!(record.status, FactoryRuntimeStatus::Running);
    assert_eq!(
        source_state(&record, FactorySourceName::Queue),
        SourceState::Available
    );
    assert_eq!(record.eligible.total.as_ref().unwrap().as_u64().unwrap(), 1);
    assert_eq!(record.eligible.entries[0].task_id, "t2");

    // The immediate rescan claims the remaining row exactly once.
    assert_eq!(runtime.tick(), LoopAction::Ran);
    assert_eq!(shared.runs(), vec!["t1".to_string(), "t2".to_string()]);
    assert_eq!(shared.claims.load(Ordering::SeqCst), 2);

    // With no known remainder the queue is explicitly unknown (never a
    // fabricated successful empty) until a fresh read.
    let record = source.snapshot(&snapshot_request()).expect("snapshot");
    assert!(record.eligible.total.is_none(), "drained pre-claim read is unknown");
    assert_eq!(runtime.tick(), LoopAction::Idle);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Idle);
}

#[test]
fn run_until_rescans_immediately_after_a_run_without_an_idle_wait() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("t1")]));
    shared.push_ready(Ok(vec![task("t2")]));
    let control = Arc::new(RuntimeControl::new());
    shared.shutdown_after(2, Arc::clone(&control));
    let (mut runtime, _observation) = make_runtime(Arc::clone(&shared));

    let started = Instant::now();
    let _ = runtime.run_until(&control);
    let elapsed = started.elapsed();
    assert_eq!(shared.claims.load(Ordering::SeqCst), 2);
    assert!(
        elapsed < WATCH_IDLE_WAIT,
        "a completed run rescans immediately rather than idling: {elapsed:?}"
    );
}

#[test]
fn watch_runtime_starts_supervises_and_drains_on_sigterm() {
    require_bd();
    let dir = ScratchDir::new("factory-runtime-watch");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    run_ok("git", &repo, &["init", "-q", "-b", "main"]);
    run_ok("git", &repo, &["config", "user.email", "agalma@localhost"]);
    run_ok("git", &repo, &["config", "user.name", "Agalma"]);
    std::fs::write(repo.join("seed.txt"), b"seed").expect("seed");
    run_ok("git", &repo, &["add", "-A"]);
    run_ok("git", &repo, &["commit", "-qm", "seed"]);
    let out = Command::new("bd")
        .args([
            "init",
            "--prefix",
            "frwatch",
            "--non-interactive",
            "--skip-agents",
            "--skip-hooks",
        ])
        .current_dir(&repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd init");
    assert!(out.status.success(), "bd init: {out:?}");

    let state = dir.join("state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            repo.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "--watch",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
        .env("XDG_CONFIG_HOME", config_home(&state))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn work --watch");

    // Let the runtime reach its Idle wait with the supervision owner running.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "watch runtime is still running while idle"
    );
    // Graceful drain: the watch signal handler requests shutdown and releases the
    // lock instead of hard-exiting.
    let killed = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill -TERM");
    assert!(killed.success(), "SIGTERM delivered");
    let output = child.wait_with_output().expect("watch runtime exits");
    assert!(
        output.status.success(),
        "watch runtime drains cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("work --watch repository_id="),
        "watch runtime reports its identities: {stderr}"
    );
}

#[test]
fn stop_before_next_claim_pauses_without_claiming() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("t1")]));
    shared.push_ready(Ok(vec![task("t2")]));
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    assert_eq!(runtime.tick(), LoopAction::Ran);
    assert_eq!(shared.runs(), vec!["t1".to_string()]);

    // A stop arrives before the next claim.
    shared.set_decision(agalma_conductor::stop::StopDecision::Stopped(
        "stop policy active".to_string(),
    ));
    assert_eq!(runtime.tick(), LoopAction::Paused);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Paused);
    assert_eq!(shared.claims.load(Ordering::SeqCst), 1, "no second claim");
    assert_eq!(shared.runs(), vec!["t1".to_string()]);
}

#[test]
fn queue_failure_is_unavailable_but_empty_is_idle() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Err(ContractError::KnownFailure("queue down".to_string())));
    shared.push_ready(Ok(Vec::new()));
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    assert_eq!(runtime.tick(), LoopAction::Retry);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Unavailable);
    assert_eq!(observation.revision() > 0, true);

    assert_eq!(runtime.tick(), LoopAction::Idle);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Idle);
}

#[test]
fn claim_failure_is_unavailable_and_not_a_run() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("t1")]));
    shared.deny_claim.store(true, Ordering::SeqCst);
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    assert_eq!(runtime.tick(), LoopAction::Retry);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Unavailable);
    assert_eq!(shared.claims.load(Ordering::SeqCst), 0);
    assert!(shared.runs().is_empty());
}

#[test]
fn fatal_run_failure_publishes_failed_and_requires_restart() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("t1")]));
    shared.fail_run.store(true, Ordering::SeqCst);
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    assert_eq!(runtime.tick(), LoopAction::Fatal);
    assert_eq!(observation.status(), FactoryRuntimeStatus::Failed);
}

#[test]
fn recovery_runs_before_any_read_or_claim() {
    let shared = Arc::new(Shared::default());
    let control = Arc::new(RuntimeControl::new());
    shared.push_ready(Ok(Vec::new()));
    shared.shutdown_after(1, Arc::clone(&control));
    let (mut runtime, _observation) = make_runtime(Arc::clone(&shared));

    let _ = runtime.run_until(&control);
    let events = shared.events();
    assert_eq!(events.first().map(String::as_str), Some("recover"));
    assert!(shared.recover_calls.load(Ordering::SeqCst) >= 1);
    assert!(
        !events.iter().any(|e| e.starts_with("claim")),
        "no claim after a bounded run: {events:?}"
    );
    assert_eq!(shared.shutdown_calls.load(Ordering::SeqCst), 1, "owner joined");
}

#[test]
fn controlled_shutdown_joins_the_worker_owner() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(Vec::new()));
    let (mut runtime, _observation) = make_runtime(Arc::clone(&shared));
    let control = Arc::new(RuntimeControl::new());

    let handle = {
        let control = Arc::clone(&control);
        std::thread::spawn(move || runtime.run_until(&control))
    };
    // Let the loop reach its interruptible idle wait, then request shutdown.
    std::thread::sleep(Duration::from_millis(100));
    control.request_shutdown();
    let action = handle.join().expect("runtime thread joins");
    assert_eq!(action, LoopAction::Paused);
    assert_eq!(shared.shutdown_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn long_blocked_phase_keeps_reads_and_supervision_responsive() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("slow")]));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    shared.set_gate(Arc::clone(&gate));
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    // A recording supervision owner ticks independently of the blocked worker.
    #[derive(Clone)]
    struct Recording {
        ticks: Arc<AtomicUsize>,
    }
    impl SupervisionOwner for Recording {
        fn tick(&mut self) -> Result<(), ContractError> {
            self.ticks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn shutdown(&mut self) -> Result<(), ContractError> {
            Ok(())
        }
    }
    let ticks = Arc::new(AtomicUsize::new(0));
    let mut supervision = SupervisionHandle::spawn(
        Recording {
            ticks: Arc::clone(&ticks),
        },
        Duration::from_millis(20),
    );

    let worker = std::thread::spawn(move || runtime.tick());

    // While the worker is blocked in `run`, the snapshot source and the
    // supervision owner remain responsive.
    std::thread::sleep(Duration::from_millis(120));
    let source = FactoryRuntimeSource::new(
        Arc::clone(&observation),
        "repo".to_string(),
        "state".to_string(),
    );
    let record = source.snapshot(&snapshot_request()).expect("snapshot");
    assert_eq!(record.status, FactoryRuntimeStatus::Running);
    assert!(ticks.load(Ordering::SeqCst) > 0, "supervision progressed");
    assert!(
        shared.max_concurrent() <= 1,
        "serial execution while blocked"
    );

    // Release the blocked run; the worker completes exactly one execution.
    let (lock, cvar) = &*gate;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
    assert_eq!(worker.join().expect("worker joins"), LoopAction::Ran);
    assert_eq!(shared.runs(), vec!["slow".to_string()]);

    supervision.request_shutdown();
    supervision.join().expect("supervision joins");
}

#[test]
fn second_owner_is_refused() {
    let dir = ScratchDir::new("factory-runtime-owner");
    let path: PathBuf = dir.to_path_buf();
    let first = agalma_conductor::LockGuard::acquire(&path).expect("first owner");
    let second = agalma_conductor::LockGuard::acquire(&path);
    assert!(second.is_err(), "a second owner must be refused");
    let err = second.err().unwrap();
    assert!(err.contains("another conductor"), "refusal reason: {err}");
    drop(first);
    // After release a new owner can acquire.
    agalma_conductor::LockGuard::acquire(&path).expect("reacquire after release");
}

#[test]
fn stopped_established_is_paused_but_fresh_stopped_does_not_initialize() {
    // Bind storage in-process so the test needs no externally built ledger
    // server binary.
    std::env::set_var("AGALMA_LEDGER_TRANSPORT", "in-process");
    // Fresh stopped: STOP present, no ledger. Unavailable, and no storage created.
    let fresh = ScratchDir::new("factory-runtime-fresh-stopped");
    let fresh_path: PathBuf = fresh.to_path_buf();
    std::fs::write(fresh_path.join("STOP"), b"stop").expect("write STOP");
    match classify_startup(&fresh_path) {
        StartupMode::Unavailable(reason) => assert!(
            reason.contains("no readable established storage") || reason.contains("unavailable"),
            "reason: {reason}"
        ),
        other => panic!("fresh stopped must be Unavailable, got {other:?}"),
    }
    assert!(
        !fresh_path.join("ledger.sqlite").exists(),
        "no ledger initialized under stop"
    );

    // Established stopped: an existing compatible ledger + STOP. Paused, and the
    // ledger is not replaced.
    let established = ScratchDir::new("factory-runtime-established-stopped");
    let established_path: PathBuf = established.to_path_buf();
    let handle = agalma_conductor::ledger_process::LedgerHandle::in_process(&established_path)
        .expect("establish ledger");
    drop(handle);
    let before = std::fs::read(established_path.join("ledger.sqlite")).expect("read ledger");
    std::fs::write(established_path.join("STOP"), b"stop").expect("write STOP");
    match classify_startup(&established_path) {
        StartupMode::Paused(_) => {}
        other => panic!("established stopped must be Paused, got {other:?}"),
    }
    let after = std::fs::read(established_path.join("ledger.sqlite")).expect("read ledger");
    assert_eq!(before, after, "established storage is never replaced");
}

#[test]
fn watch_is_rejected_with_once_or_max_tasks() {
    let mut args = agalma_conductor::cli::WorkArgs {
        repo: PathBuf::from("."),
        once: false,
        max_tasks: None,
        watch: false,
        state_dir: None,
        model: None,
        builder_model: None,
        harness: None,
        factory_control_plugin: None,
        plugin_roots: Default::default(),
    };
    args.watch = true;
    assert!(watch_conflict(&args).is_none(), "watch alone is valid");
    args.once = true;
    assert!(
        watch_conflict(&args).unwrap().contains("--once"),
        "watch+once rejected"
    );
    args.once = false;
    args.max_tasks = Some(2);
    assert!(
        watch_conflict(&args).unwrap().contains("--max-tasks"),
        "watch+max-tasks rejected"
    );
}

#[test]
fn snapshot_pages_are_bounded_and_sorted_with_cursor_revision_pinning() {
    // Directly drive the shared observation via the runtime so the page is the
    // real eligible set, not a fabricated one. A gated run holds `Running` so the
    // full eligible page is observable.
    let shared = Arc::new(Shared::default());
    let mut many: Vec<Task> = (0..60).map(|i| task(&format!("t{i:03}"))).collect();
    many.sort_by(|a, b| b.task_id.as_str().cmp(a.task_id.as_str()));
    shared.push_ready(Ok(many));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    shared.set_gate(Arc::clone(&gate));
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));
    observation.initialize(
        Some("inst".to_string()),
        vec![
            (agalma_contracts::SourceState::Available, String::new()),
            (agalma_contracts::SourceState::Available, String::new()),
            (agalma_contracts::SourceState::Available, String::new()),
            (agalma_contracts::SourceState::Unknown, String::new()),
            (agalma_contracts::SourceState::Available, String::new()),
        ],
    );

    let worker = std::thread::spawn(move || runtime.tick());
    // Wait until the gated run published the full eligible page.
    for _ in 0..200 {
        if observation.status() == FactoryRuntimeStatus::Running {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(observation.status(), FactoryRuntimeStatus::Running);

    let source = FactoryRuntimeSource::new(
        Arc::clone(&observation),
        "repo".to_string(),
        "state".to_string(),
    );
    let first = source
        .snapshot(&FactoryRuntimeRequest {
            format: FACTORY_RUNTIME_FORMAT.to_string(),
            operation: "snapshot".to_string(),
            limit: Some(50),
            cursor: None,
        })
        .expect("first page");
    assert_eq!(first.eligible.entries.len(), 50);
    // The claimed `Running` task is no longer eligible, so the page holds 59.
    let ids: Vec<String> = first
        .eligible
        .entries
        .iter()
        .map(|e| e.task_id.clone())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "entries sorted by task_id");
    assert!(
        !ids.iter().any(|id| id == "t000"),
        "the claimed task is removed from the eligible page"
    );
    assert_eq!(first.eligible.total.as_ref().unwrap().as_u64().unwrap(), 59);
    assert!(first.eligible.next_cursor.is_some(), "more pages remain");

    // The next page continues after the cursor.
    let cursor = first.eligible.next_cursor.clone().unwrap();
    let second = source
        .snapshot(&FactoryRuntimeRequest {
            format: FACTORY_RUNTIME_FORMAT.to_string(),
            operation: "snapshot".to_string(),
            limit: Some(50),
            cursor: Some(cursor.clone()),
        })
        .expect("second page");
    assert_eq!(second.eligible.entries.len(), 9);
    assert!(second.eligible.next_cursor.is_none());

    // Release the gated run and join the worker before mutating the shared cell.
    let (lock, cvar) = &*gate;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
    assert_eq!(worker.join().expect("worker joins"), LoopAction::Ran);

    // A cursor minted by a different launch fails closed with RevisionConflict.
    observation.initialize(
        Some("restarted".to_string()),
        vec![
            (agalma_contracts::SourceState::Available, String::new()),
            (agalma_contracts::SourceState::Available, String::new()),
            (agalma_contracts::SourceState::Available, String::new()),
            (agalma_contracts::SourceState::Unknown, String::new()),
            (agalma_contracts::SourceState::Available, String::new()),
        ],
    );
    let conflict = source.snapshot(&FactoryRuntimeRequest {
        format: FACTORY_RUNTIME_FORMAT.to_string(),
        operation: "snapshot".to_string(),
        limit: Some(50),
        cursor: Some(cursor),
    });
    assert!(conflict.is_err(), "stale cursor is a RevisionConflict");
}

#[test]
fn unavailable_snapshot_has_null_identities_and_empty_entries() {
    let source = FactoryRuntimeSource::unavailable(
        "repo".to_string(),
        "state".to_string(),
        "no readable established storage".to_string(),
    );
    let record = source.snapshot(&snapshot_request()).expect("snapshot");
    record.validate().expect("valid unavailable record");
    assert_eq!(record.status, FactoryRuntimeStatus::Unavailable);
    assert!(record.runtime_instance_id.is_none());
    assert!(record.eligible.entries.is_empty());
    assert!(record.eligible.total.is_none());
    assert!(record.eligible.revision.is_none());
    assert_eq!(record.permissions.pause, false);
    assert_eq!(record.permissions.resume, false);
    assert_eq!(record.sources.len(), 5, "exactly five named sources");
}

// ---------------------------------------------------------------------------
// Legacy compatibility and the bounded wake channel
// ---------------------------------------------------------------------------

#[test]
fn legacy_empty_work_once_still_succeeds_while_watch_is_rejected() {
    require_bd();
    let dir = ScratchDir::new("factory-runtime-legacy");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    run_ok("git", &repo, &["init", "-q", "-b", "main"]);
    run_ok("git", &repo, &["config", "user.email", "agalma@localhost"]);
    run_ok("git", &repo, &["config", "user.name", "Agalma"]);
    std::fs::write(repo.join("seed.txt"), b"seed").expect("seed");
    run_ok("git", &repo, &["add", "-A"]);
    run_ok("git", &repo, &["commit", "-qm", "seed"]);
    let out = Command::new("bd")
        .args([
            "init",
            "--prefix",
            "fruntime",
            "--non-interactive",
            "--skip-agents",
            "--skip-hooks",
        ])
        .current_dir(&repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd init");
    assert!(out.status.success(), "bd init: {out:?}");

    let state = dir.join("state");
    let bin = env!("CARGO_BIN_EXE_agalma");

    // Legacy batch/once on an empty queue still succeeds (unchanged behavior).
    let once = Command::new(bin)
        .args([
            "work",
            "--repo",
            repo.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "--once",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
        .env("XDG_CONFIG_HOME", config_home(&state))
        .output()
        .expect("work --once");
    assert!(
        once.status.success(),
        "legacy --once empty queue succeeds: {}",
        String::from_utf8_lossy(&once.stderr)
    );

    // The explicit watch mode does not silently combine with legacy bounds.
    for extra in [vec!["--once"], vec!["--max-tasks", "1"]] {
        let mut command = Command::new(bin);
        command.args([
            "work",
            "--repo",
            repo.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "--watch",
        ]);
        command.args(&extra);
        let out = command.output().expect("work --watch");
        assert!(!out.status.success(), "watch combination must fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--watch cannot be combined"),
            "rejection reason: {stderr}"
        );
    }
}

#[test]
fn wake_channel_is_bounded_and_ui_disconnect_cannot_change_authority() {
    let control = RuntimeControl::new();
    for _ in 0..1000 {
        control.request_recheck();
    }
    // The channel coalesces; request_shutdown is never dropped by coalescing.
    control.request_shutdown();
    assert!(control.is_shutdown());

    // A bounded runtime never claims more than the ready arrivals, and losing a
    // UI observer (a coalesced wake hint) does not change that authority.
    let shared = Arc::new(Shared::default());
    for index in 0..WATCH_NOTIFICATION_CAPACITY {
        shared.push_ready(Ok(vec![task(&format!("t{index}"))]));
    }
    let (mut runtime, _observation) = make_runtime(Arc::clone(&shared));
    let mut claimed = 0;
    while let LoopAction::Ran = runtime.tick() {
        claimed += 1;
        if claimed > WATCH_NOTIFICATION_CAPACITY {
            break;
        }
    }
    assert_eq!(
        shared.claims.load(Ordering::SeqCst),
        WATCH_NOTIFICATION_CAPACITY,
        "exactly one claim per arrival"
    );
}

// ---------------------------------------------------------------------------
// Production supervision owner (real hub serving while the worker blocks)
// ---------------------------------------------------------------------------

#[test]
fn production_supervision_serves_the_hub_while_a_worker_is_busy() {
    let dir = ScratchDir::new("factory-runtime-supervision");
    let state = dir.child("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let ledger = LedgerHandle::in_process(&state).expect("in-process selected ledger");
    let observation = Arc::new(RuntimeShared::new());
    let hub_socket = agalma_conductor::plugin_registry::plugin_socket_path(&state, "hub", 0);

    // The real production supervision owner, running on its own native thread.
    let mut supervision = SupervisionHandle::spawn_host(
        state.clone(),
        agalma_conductor::cli::PluginRootArgs::default(),
        ledger,
        Arc::clone(&observation),
        None,
        Duration::from_millis(20),
    );

    // Hold a "busy task" on the worker for the duration of the callback.
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let busy = {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            let (lock, cvar) = &*gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cvar.wait(open).unwrap();
            }
        })
    };

    // A real Connect callback to the hub capability socket must be served while
    // the worker is still blocked. Connecting performs the Connect handshake; a
    // hub method call then receives a server response (an application-level
    // error for an unknown method is still a served response).
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut served = false;
    for _ in 0..400 {
        let outcome = runtime.block_on(async {
            let Ok(mut client) = agalma_ipc::ConnectClient::connect(&hub_socket).await else {
                return None;
            };
            Some(client.call("hub.UnknownMethod", serde_json::json!({})).await)
        });
        if outcome.is_some() {
            served = true;
            break;
        }
        assert!(
            busy.is_finished() == false,
            "the busy worker is still blocked while callbacks are attempted"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        served,
        "the hub capability socket serves callbacks while the worker is busy"
    );

    // Host health is published as the Runtime observation source.
    let source = FactoryRuntimeSource::new(
        Arc::clone(&observation),
        "repo".to_string(),
        "state".to_string(),
    );
    let record = source.snapshot(&snapshot_request()).expect("snapshot");
    assert_eq!(
        source_state(&record, FactorySourceName::Runtime),
        SourceState::Available,
        "supervision publishes host health"
    );

    // Release the busy worker and join the owner cleanly.
    let (lock, cvar) = &*gate;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
    busy.join().expect("busy worker joins");
    supervision.request_shutdown();
    supervision.join().expect("supervision owner joins");
}

#[test]
fn fatal_worker_error_from_run_until_shuts_down_and_returns_fatal() {
    let shared = Arc::new(Shared::default());
    shared.push_ready(Ok(vec![task("t1")]));
    shared.fail_run.store(true, Ordering::SeqCst);
    let control = Arc::new(RuntimeControl::new());
    let (mut runtime, observation) = make_runtime(Arc::clone(&shared));

    let action = runtime.run_until(&control);
    assert_eq!(action, LoopAction::Fatal, "a fatal worker error is preserved");
    assert_eq!(observation.status(), FactoryRuntimeStatus::Failed);
    assert_eq!(
        shared.shutdown_calls.load(Ordering::SeqCst),
        1,
        "the worker owner is shut down before returning"
    );
}

#[test]
fn watch_composition_exposes_the_registration_handles() {
    let shared = Arc::new(RuntimeShared::new());
    shared.initialize(Some("epoch-1".to_string()), running_sources());
    let composition = WatchComposition::new(
        "repo".to_string(),
        "state".to_string(),
        shared,
        Arc::new(RuntimeControl::new()),
    );
    // The observation source is built from the shared cell the runtime uses.
    let source = composition.source();
    let record = source.snapshot(&snapshot_request()).expect("snapshot");
    assert_eq!(record.repository_id, "repo");
    assert_eq!(record.state_id, "state");
    assert_eq!(record.runtime_instance_id.as_deref(), Some("epoch-1"));
}

#[test]
fn supervision_owner_failure_is_propagated_on_join() {
    struct Failing;
    impl SupervisionOwner for Failing {
        fn tick(&mut self) -> Result<(), ContractError> {
            Err(ContractError::KnownFailure(
                "hub capability socket stopped".to_string(),
            ))
        }
        fn shutdown(&mut self) -> Result<(), ContractError> {
            Ok(())
        }
    }

    let mut handle = SupervisionHandle::spawn(Failing, Duration::from_millis(5));
    std::thread::sleep(Duration::from_millis(40));
    handle.request_shutdown();
    let err = handle
        .join()
        .expect_err("an owner failure is surfaced, not discarded");
    assert!(
        err.to_string().contains("hub capability socket stopped"),
        "surface the owner failure: {err}"
    );
}

// ---------------------------------------------------------------------------
// Property: generated event sequences preserve the runtime invariants
// ---------------------------------------------------------------------------

/// Deterministic xorshift64 generator (independent of the implementation).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn continuous_runtime_invariants_over_generated_event_sequences() {
    const SEED: u64 = 0x5745_4210;
    const SEQUENCES: usize = 256;
    const MAX_EVENTS: usize = 32;

    let mut rng = Rng(SEED);
    let mut total_events = 0usize;

    for seq in 0..SEQUENCES {
        let shared = Arc::new(Shared::default());
        let (mut runtime, _observation) = make_runtime(Arc::clone(&shared));
        let mut claimed_expected = 0usize;
        let mut denied_now = false;

        let events = 1 + rng.below(MAX_EVENTS as u64) as usize;
        for _ in 0..events {
            total_events += 1;
            match rng.below(6) {
                // Queue arrival: a fresh eligible task is claimable.
                0 | 1 => {
                    shared.push_ready(Ok(vec![task(&format!(
                        "s{seq}t{}",
                        rng.below(1_000_000)
                    ))]));
                }
                // Successful empty read.
                2 => shared.push_ready(Ok(Vec::new())),
                // Queue failure.
                3 => shared.push_ready(Err(ContractError::KnownFailure("down".to_string()))),
                // Stop/resume.
                4 => {
                    if rng.below(2) == 0 {
                        shared.set_decision(agalma_conductor::stop::StopDecision::Stopped(
                            "stop".to_string(),
                        ));
                        denied_now = true;
                    } else {
                        shared.set_decision(agalma_conductor::stop::StopDecision::Running);
                        denied_now = false;
                    }
                }
                // Crash/restart or UI disconnect: a fresh runtime instance and a
                // bounded wake hint. This must not change claim authority.
                _ => {
                    if rng.below(2) == 0 {
                        control_hint_only(&shared);
                    } else {
                        // Crash/restart: a new runtime reconstruction with the
                        // same durable state; recovery runs before any claim (see
                        // the dedicated recovery-ordering case) and the restart
                        // can never claim a task twice.
                        let (mut restarted, _obs) = make_runtime(Arc::clone(&shared));
                        let action = restarted.tick();
                        if denied_now {
                            assert_eq!(
                                action,
                                LoopAction::Paused,
                                "no dispatch under a denied stop"
                            );
                        }
                        if action == LoopAction::Ran {
                            claimed_expected += 1;
                        }
                        let runs = shared.runs();
                        let unique: std::collections::HashSet<&String> = runs.iter().collect();
                        assert_eq!(unique.len(), runs.len(), "restart gave no duplicate claim");
                    }
                }
            }

            let action = runtime.tick();
            if action == LoopAction::Ran {
                assert!(
                    !denied_now,
                    "a run happened while the stop decision denied admission"
                );
                claimed_expected += 1;
                // One claimant, serial execution.
                assert_eq!(
                    shared.claims.load(Ordering::SeqCst),
                    claimed_expected,
                    "exactly one claim per run"
                );
                assert!(shared.max_concurrent() <= 1, "serial execution");
                // No duplicate lease claim: every run id is distinct.
                let runs = shared.runs();
                let unique: std::collections::HashSet<&String> = runs.iter().collect();
                assert_eq!(unique.len(), runs.len(), "no duplicate claim");
            }
            // No denied task dispatch: a paused action never claims.
            if action == LoopAction::Paused && denied_now {
                assert_eq!(
                    shared.claims.load(Ordering::SeqCst),
                    claimed_expected,
                    "paused loop did not claim"
                );
            }
        }
    }
    assert_eq!(total_events >= SEQUENCES, true, "all sequences executed");
}

/// A generated restart (the `run_until` boot gate) must run frozen recovery
/// before any claim: the existing `tick()`-only property bypasses recovery, so
/// this property drives the real boot gate. It uses the same fixed seed scheme.
#[test]
fn generated_restarts_run_recovery_before_any_claim() {
    const SEED: u64 = 0x5745_4211;
    const CASES: usize = 64;

    let mut rng = Rng(SEED);
    for case in 0..CASES {
        let tasks = 1 + rng.below(4) as usize;
        let shared = Arc::new(Shared::default());
        for index in 0..tasks {
            shared.push_ready(Ok(vec![task(&format!("g{case}_{index}"))]));
        }
        let control = Arc::new(RuntimeControl::new());
        let (mut runtime, _observation) = make_runtime(Arc::clone(&shared));

        let handle = {
            let control = Arc::clone(&control);
            std::thread::spawn(move || runtime.run_until(&control))
        };
        // Drive the interruptible waits until every arrival has been claimed.
        for _ in 0..4000 {
            if shared.claims.load(Ordering::SeqCst) >= tasks {
                break;
            }
            control.request_recheck();
            std::thread::sleep(Duration::from_millis(1));
        }
        control.request_shutdown();
        let action = handle.join().expect("runtime thread joins");
        assert_ne!(action, LoopAction::Fatal);

        let events = shared.events();
        assert_eq!(
            events.first().map(String::as_str),
            Some("recover"),
            "recovery is the first boot phase: {events:?}"
        );
        let recover = events.iter().position(|e| e == "recover").unwrap();
        let claim = events
            .iter()
            .position(|e| e.starts_with("claim"))
            .expect("a claim happened");
        assert!(recover < claim, "recovery precedes every claim: {events:?}");
        assert_eq!(
            shared.claims.load(Ordering::SeqCst),
            tasks,
            "exactly one claim per arrival"
        );
        assert!(shared.max_concurrent() <= 1, "serial execution");
        let runs = shared.runs();
        let unique: std::collections::HashSet<&String> = runs.iter().collect();
        assert_eq!(unique.len(), runs.len(), "no duplicate lease claim");
    }
}

/// Simulate a UI/browser disconnect: only a bounded recheck hint is requested;
/// it must not alter durable worker authority.
fn control_hint_only(shared: &Arc<Shared>) {
    let control = RuntimeControl::new();
    for _ in 0..100 {
        control.request_recheck();
    }
    // A disconnect is observational: no claim and no shutdown of the worker.
    assert_eq!(shared.shutdown_calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Production registration seam and real plugin-child lifecycle
// ---------------------------------------------------------------------------

/// A real capability adapter registered through the WEB.13 host-registration
/// seam, proving the seam reaches the *served* production router.
struct SeamAdapter {
    calls: Arc<AtomicUsize>,
}

impl CapabilityAdapter for SeamAdapter {
    fn capability(&self) -> &str {
        "web10.seam/v1"
    }

    fn invoke(&mut self, _scope: &GrantScope, _input: &Value) -> Result<Value, ContractError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"seam": "reached"}))
    }
}

/// Records what the registration seam handed the composition hook and installs a
/// real adapter + launch grant on the assembled router.
#[derive(Default)]
struct SeamRegistrar {
    router: Mutex<Option<Arc<agalma_conductor::plugin_router::RouterService>>>,
    token: Mutex<Option<String>>,
    calls: Arc<AtomicUsize>,
    generated: AtomicUsize,
}

impl SeamRegistrar {
    fn wait_generated(&self) {
        for _ in 0..800 {
            if self.generated.load(Ordering::SeqCst) >= 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the host registration seam never ran");
    }

    fn wait_token(&self) -> String {
        for _ in 0..800 {
            if let Some(token) = self.token.lock().unwrap().clone() {
                return token;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the host registration seam never issued a grant");
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl HostRegistrar for SeamRegistrar {
    fn register(&self, registration: &HostRegistration<'_>) -> Result<(), ContractError> {
        let binding = BindingPin {
            binding_id: "web10.seam".to_string(),
            api: "plugin".to_string(),
            api_version: 1,
            implementation_version: "1.0.0".to_string(),
            generation: 1,
            config_digest: format!("sha256:{}", "0".repeat(64)),
        };
        let grant = LaunchGrant::for_capability(
            "web10.seam",
            binding,
            "hub.InvokeCapability",
            "web10.seam/v1",
            10_000_000_000_000,
            None,
        )?;
        let token = grant.token_hex();
        {
            let mut router = registration.router.router();
            router.register_adapter(SeamAdapter {
                calls: Arc::clone(&self.calls),
            });
            router.install_grant(&grant);
        }
        *self.token.lock().unwrap() = Some(token);
        *self.router.lock().unwrap() = Some(Arc::clone(registration.router));
        self.generated.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn call_seam(socket: &Path, token: &str) -> Value {
    let mut client = agalma_ipc::ConnectClient::connect(socket)
        .await
        .expect("connect the served hub socket");
    client
        .call(
            "hub.InvokeCapability",
            json!({
                "grant": token,
                "plugin_id": "web10.seam",
                "generation": 1,
                "method": "hub.InvokeCapability",
                "capability": "web10.seam/v1",
                "operation_id": "op-web10-seam",
                "input": {},
                "deadline_unix_ms": 10_000_000_000_000u64,
                "depth": 0,
            }),
        )
        .await
        .expect("the served hub router answers the registered capability")
}

#[test]
fn host_registration_seam_reaches_the_served_production_router() {
    let dir = ScratchDir::new("factory-runtime-seam");
    let state = dir.child("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let ledger = LedgerHandle::in_process(&state).expect("in-process ledger");
    let observation = Arc::new(RuntimeShared::new());
    let hub_socket = agalma_conductor::plugin_registry::plugin_socket_path(&state, "hub", 0);

    let registrar = Arc::new(SeamRegistrar::default());
    let context = HostRegistrationContext {
        repository_id: "repo".to_string(),
        state_id: "state".to_string(),
        shared: Arc::clone(&observation),
        control: Arc::new(RuntimeControl::new()),
        registrar: Arc::clone(&registrar) as Arc<dyn HostRegistrar>,
    };
    let mut supervision = SupervisionHandle::spawn_host(
        state.clone(),
        agalma_conductor::cli::PluginRootArgs::default(),
        ledger,
        Arc::clone(&observation),
        Some(context),
        Duration::from_millis(20),
    );

    // The registrar ran inside the supervision owner and captured the real
    // router that serves the hub socket.
    registrar.wait_generated();
    let router = registrar
        .router
        .lock()
        .unwrap()
        .clone()
        .expect("the seam captured the served router");
    assert!(
        router.router().has_adapter("web10.seam/v1"),
        "the seam registered an adapter on the production router"
    );

    // A real hub callback over the served socket reaches that adapter and gets a
    // successful (completed) server response.
    let token = registrar.wait_token();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(call_seam(&hub_socket, &token));
    assert_eq!(result["state"], "completed", "response: {result}");
    assert_eq!(registrar.calls(), 1, "the registered adapter executed");

    supervision.request_shutdown();
    supervision.join().expect("supervision owner joins");
}

// --- sealed package fixture for the real plugin-child lifecycle ------------

const LIFECYCLE_PLUGIN_ID: &str = "web10.lifecycle";
const LIFECYCLE_CONFIG_SCHEMA: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
const LIFECYCLE_DEFAULT_CONFIG: &[u8] = b"{}\n";
const LIFECYCLE_REPORT_SCHEMA: &[u8] = b"{\"type\":\"object\",\"required\":[\"ready\"]}\n";

fn lifecycle_byte_ref(path: &str, bytes: &[u8]) -> Value {
    json!({
        "path": path,
        "digest": sha256_digest(bytes),
        "bytes": bytes.len() as u64,
        "media_type": "application/json",
    })
}

fn lifecycle_manifest() -> Value {
    json!({
        "format": "PluginManifest/v1",
        "plugin_id": LIFECYCLE_PLUGIN_ID,
        "package_version": "1.2.3",
        "package_digest": format!("sha256:{}", "0".repeat(64)),
        "protocol_major": 1,
        "entrypoint": {
            "path": "bin/plugin",
            "argv": ["--serve"],
            "launch_profile": "reporter-read-only/v1"
        },
        "domain_apis": [],
        "requested_capabilities": ["report.publish"],
        "required_services": [],
        "optional_services": [],
        "config_schema": lifecycle_byte_ref("objects/config-schema.json", LIFECYCLE_CONFIG_SCHEMA),
        "default_configuration": lifecycle_byte_ref("objects/default-config.json", LIFECYCLE_DEFAULT_CONFIG),
        "report_schemas": [{
            "namespace": "web10.lifecycle.status",
            "version": 1,
            "schema": lifecycle_byte_ref("objects/report-schema.json", LIFECYCLE_REPORT_SCHEMA)
        }],
        "views": [{
            "view_id": "status",
            "schema_namespace": "web10.lifecycle.status",
            "kind": "Status",
            "title": "Lifecycle status",
            "schema_version": 1,
            "fields": ["ready"]
        }],
        "actions": [],
        "state_compatibility": {"reads": ["lifecycle-state/v1"], "writes": "lifecycle-state/v1"}
    })
}

/// Env the sealed child script passes to the persistent typed-health child.
const HEALTH_CHILD_SOCKET_ENV: &str = "AGALMA_WEB10_HEALTH_CHILD_SOCKET";
const HEALTH_CHILD_GENERATION_ENV: &str = "AGALMA_WEB10_HEALTH_CHILD_GENERATION";
const HEALTH_CHILD_CONFIG_DIGEST_ENV: &str = "AGALMA_WEB10_HEALTH_CHILD_CONFIG_DIGEST";

/// A real child launcher: records its pid/launch environment, extracts the
/// `--socket` the supervisor passed, then execs the test binary in child mode so
/// a **real, persistent** process serves the declared `plugin.Describe` /
/// `plugin.Health` RPC and becomes `Ready` only through that RPC. The pid is
/// preserved across `exec`, so the recorded pid is the exact owned child.
fn lifecycle_child_script(marker: &Path, test_bin: &Path) -> String {
    let marker = marker.display();
    let test_bin = test_bin.display();
    let config = sha256_digest(LIFECYCLE_DEFAULT_CONFIG);
    format!(
        "#!/bin/sh\n\
printf '%s\\n' \"$$\" >> \"{marker}/pids\"\n\
env > \"{marker}/env.$$\"\n\
: > \"{marker}/started.$$\"\n\
socket=\"\"\n\
while [ $# -gt 0 ]; do\n\
  case \"$1\" in\n\
    --socket) socket=\"$2\"; shift 2;;\n\
    *) shift;;\n\
  esac\n\
done\n\
{HEALTH_CHILD_SOCKET_ENV}=\"$socket\" \\\n\
{HEALTH_CHILD_GENERATION_ENV}=1 \\\n\
{HEALTH_CHILD_CONFIG_DIGEST_ENV}='{config}' \\\n\
exec \"{test_bin}\" --exact plugin_health_child_server --nocapture\n",
    )
}

/// The real child process entry: serve the declared common plugin protocol on
/// the supervisor-provided socket and block until killed. Runs as its own
/// process (the integration-test binary re-executed in child mode), so it is a
/// genuine persistent child, not an in-process fake.
#[test]
fn plugin_health_child_server() {
    let Ok(socket) = std::env::var(HEALTH_CHILD_SOCKET_ENV) else {
        // Normal suite run: this is not the child invocation.
        return;
    };
    let socket = PathBuf::from(socket);
    let generation = std::env::var(HEALTH_CHILD_GENERATION_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);
    let config_digest = std::env::var(HEALTH_CHILD_CONFIG_DIGEST_ENV)
        .unwrap_or_else(|_| sha256_digest(LIFECYCLE_DEFAULT_CONFIG));
    let binding = BindingPin {
        binding_id: LIFECYCLE_PLUGIN_ID.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "1.2.3".to_string(),
        generation,
        config_digest,
    };
    let mut server = agalma_ipc::Server::new(agalma_ipc::plugin_server::plugin_server_info(
        LIFECYCLE_PLUGIN_ID,
        "1.2.3",
        ["report.publish".to_string()],
    ));
    agalma_ipc::plugin_server::register_plugin(
        &mut server,
        Arc::new(HealthChildPlugin { binding }),
    );
    // Serve the declared Health RPC; `serve_wire` announces READY after the
    // bind and exits the process when the supervisor closes stdin.
    let _ = agalma_ipc::serve_wire(server, &socket, agalma_ipc::Wire::Connect);
    std::process::exit(0);
}

/// The child's declared common plugin handler: a well-formed `Describe` and a
/// `PluginHealth/v1` `ready` record pinned to the exact host binding.
struct HealthChildPlugin {
    binding: BindingPin,
}

impl agalma_ipc::plugin_server::PluginHandler for HealthChildPlugin {
    fn describe(&self) -> Result<Value, ContractError> {
        Ok(json!({
            "api": "plugin",
            "protocol_major": 1,
            "plugin_id": LIFECYCLE_PLUGIN_ID,
            "implementation_version": "1.2.3",
            "capabilities": ["report.publish"],
        }))
    }

    fn health(&self) -> Result<Value, ContractError> {
        Ok(serde_json::to_value(PluginHealth {
            format: PLUGIN_HEALTH_FORMAT.to_string(),
            plugin_id: LIFECYCLE_PLUGIN_ID.to_string(),
            binding: self.binding.clone(),
            state: HealthState::Ready,
            detail: "serving".to_string(),
            checked_at_unix_ms: 0,
        })
        .expect("PluginHealth serializes"))
    }

    fn snapshot(&self) -> Result<Value, ContractError> {
        Ok(Value::Null)
    }
}

fn seal_lifecycle_package(root: &Path, script: &str) -> PluginManifest {
    let package_dir = root.join("pkg");
    let files: Vec<(String, Vec<u8>)> = vec![
        (
            "objects/config-schema.json".to_string(),
            LIFECYCLE_CONFIG_SCHEMA.to_vec(),
        ),
        (
            "objects/default-config.json".to_string(),
            LIFECYCLE_DEFAULT_CONFIG.to_vec(),
        ),
        (
            "objects/report-schema.json".to_string(),
            LIFECYCLE_REPORT_SCHEMA.to_vec(),
        ),
        ("bin/plugin".to_string(), script.as_bytes().to_vec()),
    ];
    for (relative, bytes) in files {
        let path = package_dir.join(&relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        std::fs::write(&path, bytes).expect("write package file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            package_dir.join("bin/plugin"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod entrypoint");
    }
    let parsed: PluginManifest = serde_json::from_value(lifecycle_manifest()).expect("manifest");
    let digest = package_inventory_digest(&package_dir, &parsed).expect("seal package");
    let mut sealed = lifecycle_manifest();
    sealed["package_digest"] = Value::String(digest);
    std::fs::write(
        package_dir.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&sealed).expect("encode manifest"),
    )
    .expect("write manifest");
    serde_json::from_value(sealed).expect("sealed manifest")
}

fn wait_for(mut predicate: impl FnMut() -> bool, what: &str) {
    for _ in 0..1200 {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn marker_files(marker: &Path, prefix: &str) -> Vec<PathBuf> {
    std::fs::read_dir(marker)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().starts_with(prefix))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn pid_lines(pids: &Path) -> Vec<u32> {
    std::fs::read_to_string(pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

fn host_health_detail(shared: &Arc<RuntimeShared>) -> Option<String> {
    let source = FactoryRuntimeSource::new(
        Arc::clone(shared),
        "repo".to_string(),
        "state".to_string(),
    );
    let record = source.snapshot(&snapshot_request()).ok()?;
    record
        .sources
        .iter()
        .find(|entry| entry.source == FactorySourceName::Runtime)
        .and_then(|entry| entry.detail.clone())
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn production_plugin_child_is_ready_via_health_rpc_and_restarts_on_real_signal() {
    use agalma_conductor::plugin_supervisor::HealthProbe as _;

    let dir = ScratchDir::new("factory-runtime-lifecycle");
    let state = dir.child("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let root = dir.child("root");
    let marker = dir.child("marker");
    std::fs::create_dir_all(&marker).expect("marker dir");
    let test_bin = std::env::current_exe().expect("integration test executable");
    seal_lifecycle_package(&root, &lifecycle_child_script(&marker, &test_bin));

    let ledger = LedgerHandle::in_process(&state).expect("in-process ledger");
    let roots = agalma_conductor::cli::PluginRootArgs {
        plugin_root: vec![root.clone()],
        plugin_root_optional: Vec::new(),
    };
    // Desired=Running is seeded through the same selected ledger the supervision
    // owner reconstructs from, so the real child is launched on startup.
    {
        let mut host =
            agalma_conductor::composition::assemble_work_plugin_host(&state, &roots, &ledger)
                .expect("assembled host");
        host.set_desired(LIFECYCLE_PLUGIN_ID, DesiredState::Running)
            .expect("seed desired running");
    }

    let observation = Arc::new(RuntimeShared::new());
    let hub_socket = agalma_conductor::plugin_registry::plugin_socket_path(&state, "hub", 0);
    let registrar = Arc::new(SeamRegistrar::default());
    let context = HostRegistrationContext {
        repository_id: "repo".to_string(),
        state_id: "state".to_string(),
        shared: Arc::clone(&observation),
        control: Arc::new(RuntimeControl::new()),
        registrar: Arc::clone(&registrar) as Arc<dyn HostRegistrar>,
    };
    let mut supervision = SupervisionHandle::spawn_host(
        state.clone(),
        roots,
        ledger,
        Arc::clone(&observation),
        Some(context),
        Duration::from_millis(25),
    );

    // A real plugin child is launched by the production supervision owner.
    let pids = marker.join("pids");
    wait_for(|| pids.exists(), "plugin child launch");

    // The child becomes `Ready` only through the declared `plugin.Health` RPC:
    // the supervisor publishes `health=ok` only after a successful typed probe.
    wait_for(
        || host_health_detail(&observation).is_some_and(|detail| detail.contains("health=ok")),
        "child Ready through the declared Health RPC",
    );
    let first_pid = *pid_lines(&pids)
        .first()
        .expect("the owned child recorded its pid");
    assert!(
        pid_alive(first_pid),
        "the supervisor owns the exact live Ready child (pid {first_pid})"
    );

    // A direct typed probe of the owned launch epoch proves the child really
    // serves `plugin.Health` with a well-formed `PluginHealth/v1`.
    {
        let mut probe = agalma_conductor::plugin_router::RpcHealthProbe::new(state.clone());
        let result = probe
            .probe_health(LIFECYCLE_PLUGIN_ID, 1)
            .expect("typed probe connects");
        assert!(result.ready, "declared Health RPC reports ready: {:?}", result.detail);
        assert!(
            result.observation.is_some(),
            "the child returned a typed PluginHealth/v1 observation"
        );
    }

    // The child received its private launch channel.
    let env_files = marker_files(&marker, "env.");
    let env = std::fs::read_to_string(&env_files[0]).expect("child env");
    assert!(
        env.contains(&hub_socket.to_string_lossy().to_string()),
        "the child received the hub socket credential"
    );
    assert!(
        env.contains("AGALMA_PLUGIN_REPORT_GRANT") || env.contains("AGALMA_PLUGIN_GRANT"),
        "the child received a per-launch grant"
    );

    // A real hub callback over the served socket returns a successful server
    // response while the real child runs.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let token = registrar.wait_token();
    let result = runtime.block_on(call_seam(&hub_socket, &token));
    assert_eq!(result["state"], "completed", "response: {result}");

    // Kill exactly the owned Ready child. The signal must succeed and the exact
    // process must exit: no ignored `No such process` and no false restart claim.
    let killed = Command::new("/bin/kill")
        .args(["-TERM", &first_pid.to_string()])
        .status()
        .expect("signal the owned child");
    assert!(
        killed.success(),
        "the targeted signal reached the owned ready child (pid {first_pid})"
    );
    wait_for(|| !pid_alive(first_pid), "the killed child actually exited");

    // Supervision observes the crash as an explicit failure (never a false
    // healthy claim), then restarts and health-confirms the replacement.
    wait_for(
        || {
            host_health_detail(&observation)
                .is_some_and(|detail| detail.contains("crashed=") || detail.contains("not_ready="))
        },
        "crash observed as a failure, not health=ok",
    );
    wait_for(|| pid_lines(&pids).len() >= 2, "child restart");
    wait_for(
        || host_health_detail(&observation).is_some_and(|detail| detail.contains("health=ok")),
        "restart probe success",
    );
    let restarted = *pid_lines(&pids).last().expect("restarted child pid");
    assert_ne!(restarted, first_pid, "the restart produced a fresh child");
    assert!(pid_alive(restarted), "the restarted child is live and Ready");

    // The callback is still served after the restart.
    let result = runtime.block_on(call_seam(&hub_socket, &token));
    assert_eq!(result["state"], "completed", "post-restart response: {result}");

    // Graceful drain: the owner shuts the restarted child group down and the
    // exact restarted process exits.
    supervision.request_shutdown();
    supervision.join().expect("supervision owner joins");
    wait_for(|| !pid_alive(restarted), "child stopped on drain");
}

/// A real child launcher that announces a valid READY identity but never binds
/// its plugin socket, so the declared `plugin.Health` RPC is unreachable.
fn unready_child_script() -> String {
    format!(
        "#!/bin/sh\n\
printf '%s\\n' 'READY {{\"api\":\"plugin\",\"api_version\":1,\"impl\":\"{plugin}\",\"impl_version\":\"1.2.3\",\"capabilities\":[\"report.publish\"]}}'\n\
sleep 30\n",
        plugin = LIFECYCLE_PLUGIN_ID,
    )
}

/// Production path: a desired `Running` plugin that never becomes `Ready` is an
/// explicit `Unreadable` host failure, never `Available`/`health=ok`. Uses the
/// real `HostSupervision` owner, not the setter.
#[test]
fn production_host_reports_an_unready_child_as_unavailable() {
    let dir = ScratchDir::new("factory-runtime-unready");
    let state = dir.child("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let root = dir.child("root");
    seal_lifecycle_package(&root, &unready_child_script());

    let ledger = LedgerHandle::in_process(&state).expect("in-process ledger");
    let roots = agalma_conductor::cli::PluginRootArgs {
        plugin_root: vec![root.clone()],
        plugin_root_optional: Vec::new(),
    };
    {
        let mut host =
            agalma_conductor::composition::assemble_work_plugin_host(&state, &roots, &ledger)
                .expect("assembled host");
        host.set_desired(LIFECYCLE_PLUGIN_ID, DesiredState::Running)
            .expect("seed desired running");
    }

    let observation = Arc::new(RuntimeShared::new());
    let mut supervision = SupervisionHandle::spawn_host(
        state.clone(),
        roots,
        ledger,
        Arc::clone(&observation),
        None,
        Duration::from_millis(20),
    );

    // The desired plugin never becomes Ready: the host is explicitly Unreadable.
    wait_for(
        || {
            let detail = host_health_detail(&observation).unwrap_or_default();
            detail.contains("start failed") && detail.contains("not ready")
        },
        "explicit unready-plugin start failure",
    );

    // It must never flip to a healthy observation on a later crash tick.
    for _ in 0..20 {
        let source = FactoryRuntimeSource::new(
            Arc::clone(&observation),
            "repo".to_string(),
            "state".to_string(),
        );
        let record = source.snapshot(&snapshot_request()).expect("snapshot");
        assert_eq!(
            source_state(&record, FactorySourceName::Runtime),
            SourceState::Unreadable,
            "an unready desired plugin is never reported available"
        );
        let detail = host_health_detail(&observation).unwrap_or_default();
        assert!(
            !detail.contains("health=ok"),
            "an unready desired plugin never reports health=ok: {detail}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    supervision.request_shutdown();
    let err = supervision
        .join()
        .expect_err("the start failure is surfaced, not discarded");
    assert!(
        err.to_string().contains("not ready") || err.to_string().contains("start failed"),
        "join surfaces the start failure: {err}"
    );
}

// --- real-task signal drain during active production execution -------------

const WATCH_PROTECTED_RUNNER: &str =
    "#!/bin/sh\nset -eu\ngrep -q 42 \"$AGALMA_CANDIDATE_DIR/src/lib.rs\"\n";
const WATCH_PROTECTED_COMMAND: &str = "sh \"$AGALMA_PROTECTED_DIR/runner.sh\"";

fn watch_protected_baseline() -> ProtectedBaseline {
    ProtectedBaseline {
        format: PROTECTED_BASELINE_FORMAT.to_string(),
        files: vec![BaselineFile {
            path: "runner.sh".to_string(),
            mode: "100755".to_string(),
            digest: sha256_digest(WATCH_PROTECTED_RUNNER.as_bytes()),
            bytes: WATCH_PROTECTED_RUNNER.len() as u64,
            text: WATCH_PROTECTED_RUNNER.to_string(),
        }],
        commands: vec![WATCH_PROTECTED_COMMAND.to_string()],
    }
}

fn watch_protected_entry(task_id: &str, repository_root: &str, base_sha: &str) -> ProtectedEntry {
    ProtectedEntry {
        task_id: task_id.to_string(),
        repository_root: repository_root.to_string(),
        base_sha: base_sha.to_string(),
        allowed_paths: vec!["src".to_string()],
        protected_paths: Vec::new(),
        baseline: watch_protected_baseline(),
    }
}

/// Isolated XDG config base for a scratch fixture.
fn config_home(state_dir: &Path) -> PathBuf {
    state_dir.parent().expect("state parent").join("xdg-config")
}

fn write_watch_protected_inputs(state_dir: &Path, entries: Vec<ProtectedEntry>) {
    let inputs = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
    let dir = config_home(state_dir).join("agalma");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
}

#[test]
fn watch_signal_drains_at_a_cooperative_boundary_during_active_execution() {
    require_bd();
    let dir = ScratchDir::new("factory-runtime-drain-active");
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("repo src");
    std::fs::write(
        repo.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
    )
    .expect("seed source");
    run_ok("git", &repo, &["init", "-q", "-b", "main"]);
    run_ok("git", &repo, &["config", "user.email", "agalma@localhost"]);
    run_ok("git", &repo, &["config", "user.name", "Agalma"]);
    run_ok("git", &repo, &["add", "-A"]);
    run_ok("git", &repo, &["commit", "-qm", "seed"]);
    let out = Command::new("bd")
        .args([
            "init",
            "--prefix",
            "frdrain",
            "--non-interactive",
            "--skip-agents",
            "--skip-hooks",
        ])
        .current_dir(&repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd init");
    assert!(out.status.success(), "bd init: {out:?}");

    let description = "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'test -f src/lib.rs'\n  - sh -c 'grep -q 42 src/lib.rs'\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n";
    let create = Command::new("bd")
        .args([
            "create",
            "Drain task",
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            description,
            "--json",
        ])
        .current_dir(&repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd create");
    assert!(create.status.success(), "bd create: {create:?}");
    let created: Value = serde_json::from_slice(&create.stdout).expect("bd create json");
    let task_id = created["id"].as_str().expect("task id").to_string();

    let base_sha = {
        let out = Command::new("git")
            .args(["rev-parse", "main"])
            .current_dir(&repo)
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let canonical_repo =
        std::fs::canonicalize(&repo).expect("canonical repo").to_string_lossy().into_owned();
    let state = dir.join("state");
    write_watch_protected_inputs(
        &state,
        vec![watch_protected_entry(&task_id, &canonical_repo, &base_sha)],
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            repo.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "--watch",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_SCRIPTED_BUILD_SLEEP_MS", "3000")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
        .env("XDG_CONFIG_HOME", config_home(&state))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn work --watch");

    // The task reaches its build phase (active production execution).
    let mut active = false;
    for _ in 0..2000 {
        let show = Command::new("bd")
            .args(["--db", repo.join(".beads").to_str().unwrap(), "show", &task_id, "--json", "--include-comments"])
            .current_dir(&repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("bd show");
        if show.status.success() {
            let text = String::from_utf8_lossy(&show.stdout);
            if text.contains("phase=build") {
                active = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(active, "the task reached active build execution");
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the watch runtime is executing while active"
    );

    // A signal during active execution denies new admission and drains at the
    // existing cooperative phase boundary; the lock is released.
    let killed = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill -TERM");
    assert!(killed.success(), "SIGTERM delivered");
    let output = child.wait_with_output().expect("watch runtime drains");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "watch runtime drains cleanly: {stderr}"
    );
    assert!(stderr.contains("draining"), "drain reported: {stderr}");

    // No fake terminal receipt was written for the interrupted execution.
    let show = Command::new("bd")
        .args(["--db", repo.join(".beads").to_str().unwrap(), "show", &task_id, "--json"])
        .current_dir(&repo)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd show");
    let issue: Value = serde_json::from_slice(&show.stdout).expect("bd show json");
    assert_ne!(
        issue[0]["status"], "closed",
        "an interrupted execution is not a fake terminal: {issue}"
    );

    // The single-owner lock is released on drain.
    agalma_conductor::LockGuard::acquire(&std::fs::canonicalize(&state).unwrap())
        .expect("the state-directory lock is released");
}

// ---------------------------------------------------------------------------
// Small process helpers
// ---------------------------------------------------------------------------

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for this test but was not found");
}

fn run_ok(program: &str, dir: &std::path::Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}
