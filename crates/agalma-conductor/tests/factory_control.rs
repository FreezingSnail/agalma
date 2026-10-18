//! WEB.11 (`agalma-rwn.11`) acceptance for the authenticated factory runtime
//! observation and conservative pause/resume adapters.
//!
//! Covers: truthful injected runtime observations (all statuses/source states,
//! unavailable-with-null identity, queue pages); trusted permission composition
//! where a raw source flag is never authority; read-only lookup and zero-effect
//! reads while admission is stopped; narrow authenticated stop-exempt
//! pause/resume; missing/revoked/expired authority denial; durable
//! intent/result records with exact replay, orphan-intent `EffectUnconfirmed`
//! and conflict refusal; intent/effect/result fault injection across a ledger
//! reopen (restart); serialized concurrent duplicate ordering; truthful STOP
//! removal uncertainty; route-exemption scoped to the declared operations;
//! legacy CLI/TUI control regression; and independent deterministic property
//! generators (seed `0x57454211`).
//!
//! The real selected ledger transport (`LedgerHandle`, an in-process
//! `LedgerApi` binding) is used for persistence/reopen; a delegating fault
//! double injects boundary failures without reimplementing the ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use agalma_contracts::digest::{failure_code, FailureCode};
use agalma_contracts::task_package::BindingPin;
use agalma_contracts::{
    ControlReceipt, ContractError, ExactU64, FactoryActionRequest, FactoryControlOutcome,
    FactoryControlResponse, FactoryEligibleEntry, FactoryEligiblePage, FactoryRuntime,
    FactoryRuntimePermissions, FactoryRuntimeRequest, FactoryRuntimeSource, FactoryRuntimeStatus,
    FactorySourceName, FactoryStopObservation, FactoryUnconfirmedCode, LedgerApi,
    RuntimeObservationSource, SourceState, TaskFamily, TaskPriority, FACTORY_CONFIRMATION_MAX_BYTES,
    FACTORY_CONTROL_CAPABILITY, FACTORY_READ_CAPABILITY, FACTORY_REASON_MAX_BYTES,
    FACTORY_RUNTIME_FORMAT,
};
use agalma_conductor::control::{
    Actor, ControlAction, ControlOutcome, ControlPlane, ControlRequest, FactoryControlLedger,
    NoCancel,
};
use agalma_conductor::factory_control::{
    ApprovedControlBinding, ControlBindingResolver, ControlOwnership, FactoryControlService,
};
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_router::{
    wall_clock_now_ms, CapabilityCallRequest, CapabilityRouter, InMemoryOperationJournal,
    LaunchGrant, RunningStop, RouterService, StopAdmission,
};
use agalma_conductor::stop::{BoundStopControl, StopControl, UnavailableStopControl};
use agalma_ipc::plugin_server::{HubHandler, METHOD_INVOKE_CAPABILITY};
use agalma_testkit::ScratchDir;

const PLUGIN: &str = "web.dashboard";
const GENERATION: u64 = 3;
const START_MS: u64 = 1_000_000;
const FAR_FUTURE_MS: u64 = 10_000_000_000_000;
const REPOSITORY_ID: &str = "repo-opaque";
const STATE_ID: &str = "state-opaque";

// ---------------------------------------------------------------------------
// Fixtures and doubles
// ---------------------------------------------------------------------------

fn exact(value: u64) -> ExactU64 {
    ExactU64::parse(&value.to_string()).expect("exact u64")
}

fn binding(plugin_id: &str, generation: u64) -> BindingPin {
    BindingPin {
        binding_id: plugin_id.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "1.2.3".to_string(),
        generation,
        config_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

fn grant(capability: &str, expiry_unix_ms: u64) -> LaunchGrant {
    LaunchGrant::for_capability(
        PLUGIN,
        binding(PLUGIN, GENERATION),
        METHOD_INVOKE_CAPABILITY,
        capability,
        expiry_unix_ms,
        None,
    )
    .expect("OS randomness available")
}

fn call(
    grant: &LaunchGrant,
    capability: &str,
    operation_id: &str,
    input: Value,
    deadline_unix_ms: u64,
) -> CapabilityCallRequest {
    CapabilityCallRequest {
        grant: grant.token_hex(),
        plugin_id: PLUGIN.to_string(),
        generation: GENERATION,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: capability.to_string(),
        operation_id: operation_id.to_string(),
        input,
        deadline_unix_ms,
        depth: 0,
    }
}

fn snapshot_input() -> Value {
    json!({ "format": "FactoryRuntime/v1", "operation": "snapshot" })
}

fn lookup_input(action_id: &str) -> Value {
    json!({ "format": "FactoryControl/v1", "operation": "lookup", "action_id": action_id })
}

fn action_input(operation: &str, action_id: &str, confirmation: Option<&str>) -> Value {
    json!({
        "format": "FactoryControl/v1",
        "operation": operation,
        "action_id": action_id,
        "confirmation": confirmation,
    })
}

/// Control-ownership guard double.
struct TestOwner(AtomicBool);

impl TestOwner {
    fn new(owned: bool) -> Self {
        TestOwner(AtomicBool::new(owned))
    }

    fn set(&self, owned: bool) {
        self.0.store(owned, Ordering::SeqCst);
    }
}

impl ControlOwnership for TestOwner {
    fn owned(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Authoritative live control-binding resolver double.
struct TestAuthority(Mutex<Option<ApprovedControlBinding>>);

impl TestAuthority {
    fn approved(expires_at_unix_ms: u64) -> Self {
        TestAuthority(Mutex::new(Some(ApprovedControlBinding {
            plugin_id: PLUGIN.to_string(),
            capability: FACTORY_CONTROL_CAPABILITY.to_string(),
            generation: GENERATION,
            expires_at_unix_ms,
        })))
    }

    fn set(&self, binding: Option<ApprovedControlBinding>) {
        *self.0.lock().unwrap() = binding;
    }
}

impl ControlBindingResolver for TestAuthority {
    fn resolve(&self, _plugin_id: &str) -> Option<ApprovedControlBinding> {
        // Deliberately unfiltered: the adapter must itself reject a binding
        // whose `plugin_id` does not name the authenticated caller.
        self.0.lock().unwrap().clone()
    }
}

/// A configurable injected runtime observation source.
struct SourceRuntime {
    repository_id: String,
    state_id: String,
    status: FactoryRuntimeStatus,
    source_state: SourceState,
    stopped: Option<bool>,
    raw_permissions: FactoryRuntimePermissions,
    entries: Vec<FactoryEligibleEntry>,
    fault: Option<String>,
}

impl SourceRuntime {
    fn healthy() -> Self {
        SourceRuntime {
            repository_id: REPOSITORY_ID.to_string(),
            state_id: STATE_ID.to_string(),
            status: FactoryRuntimeStatus::Idle,
            source_state: SourceState::Available,
            stopped: Some(false),
            raw_permissions: FactoryRuntimePermissions::default(),
            entries: Vec::new(),
            fault: None,
        }
    }

    fn build(&self) -> FactoryRuntime {
        let now = exact(START_MS);
        FactoryRuntime {
            format: FACTORY_RUNTIME_FORMAT.to_string(),
            repository_id: self.repository_id.clone(),
            state_id: self.state_id.clone(),
            runtime_instance_id: Some("runtime-1".to_string()),
            observed_at_unix_ms: now.clone(),
            status: self.status,
            reason: None,
            sources: FactorySourceName::ALL
                .iter()
                .map(|name| FactoryRuntimeSource {
                    source: *name,
                    state: self.source_state,
                    detail: None,
                    observed_at_unix_ms: Some(now.clone()),
                })
                .collect(),
            stop: self.stopped.map(|stopped| FactoryStopObservation {
                stopped,
                reason: stopped.then(|| "latch".to_string()),
                observed_at_unix_ms: now.clone(),
            }),
            current_execution: None,
            eligible: FactoryEligiblePage {
                entries: self.entries.clone(),
                revision: Some("rev-1".to_string()),
                total: Some(exact(self.entries.len() as u64)),
                next_cursor: None,
            },
            permissions: self.raw_permissions,
        }
    }
}

impl RuntimeObservationSource for SourceRuntime {
    fn snapshot(&self, _request: &FactoryRuntimeRequest) -> Result<FactoryRuntime, ContractError> {
        if let Some(message) = &self.fault {
            return Err(ContractError::KnownFailure(message.clone()));
        }
        Ok(self.build())
    }
}

/// A delegating ledger double that injects boundary faults over a real handle.
///
/// `fail_put_number` fails **before** persisting (a pre-write fault);
/// `lose_ack_number` persists the row and then fails (a commit-then-error/lost
/// acknowledgment).
struct FaultLedger {
    inner: LedgerHandle,
    fail_set_latch: AtomicBool,
    fail_kill_latch: AtomicBool,
    fail_put_number: AtomicU64,
    lose_ack_number: AtomicU64,
    puts: AtomicU64,
}

impl FaultLedger {
    fn new(inner: LedgerHandle) -> Self {
        FaultLedger {
            inner,
            fail_set_latch: AtomicBool::new(false),
            fail_kill_latch: AtomicBool::new(false),
            fail_put_number: AtomicU64::new(0),
            lose_ack_number: AtomicU64::new(0),
            puts: AtomicU64::new(0),
        }
    }
}

impl FactoryControlLedger for FaultLedger {
    fn kill_latch(&self) -> Result<bool, ContractError> {
        if self.fail_kill_latch.load(Ordering::SeqCst) {
            Err(ContractError::KnownFailure(
                "injected kill_latch fault".to_string(),
            ))
        } else {
            LedgerApi::kill_latch(&self.inner)
        }
    }

    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError> {
        if self.fail_set_latch.load(Ordering::SeqCst) {
            Err(ContractError::KnownFailure(
                "injected set_kill_latch fault".to_string(),
            ))
        } else {
            LedgerApi::set_kill_latch(&mut self.inner, latched)
        }
    }

    fn put_control_receipt(&mut self, receipt: &ControlReceipt) -> Result<(), ContractError> {
        let number = self.puts.fetch_add(1, Ordering::SeqCst) + 1;
        let target = self.fail_put_number.load(Ordering::SeqCst);
        if target == number {
            return Err(ContractError::KnownFailure(format!(
                "injected receipt write fault at put {number}"
            )));
        }
        LedgerApi::record_control_receipt(&mut self.inner, receipt)?;
        // Commit-then-error: the row is durable, but the caller never receives
        // the (lost) acknowledgment.
        if self.lose_ack_number.load(Ordering::SeqCst) == number {
            return Err(ContractError::KnownFailure(format!(
                "injected lost receipt acknowledgment at put {number}"
            )));
        }
        Ok(())
    }

    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        LedgerApi::control_receipts(&self.inner)
    }
}

/// A ledger that blocks on its first control-receipt write (the durable intent)
/// until released, so a router timeout can be observed strictly after effect
/// admission.
struct BlockingLedger {
    inner: LedgerHandle,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

impl FactoryControlLedger for BlockingLedger {
    fn kill_latch(&self) -> Result<bool, ContractError> {
        LedgerApi::kill_latch(&self.inner)
    }

    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError> {
        LedgerApi::set_kill_latch(&mut self.inner, latched)
    }

    fn put_control_receipt(&mut self, receipt: &ControlReceipt) -> Result<(), ContractError> {
        if let Some(sender) = self.entered.lock().unwrap().take() {
            let _ = sender.send(());
            let release = self
                .release
                .lock()
                .unwrap()
                .take()
                .expect("release channel present");
            let _ = release.recv();
        }
        LedgerApi::record_control_receipt(&mut self.inner, receipt)
    }

    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        LedgerApi::control_receipts(&self.inner)
    }
}

/// A ledger that counts effect invocations (`set_kill_latch`) across restarts and
/// can inject an effect-write fault.
struct CountingLedger {
    inner: LedgerHandle,
    effects: Arc<AtomicU64>,
    fail_set: Arc<AtomicBool>,
}

impl FactoryControlLedger for CountingLedger {
    fn kill_latch(&self) -> Result<bool, ContractError> {
        LedgerApi::kill_latch(&self.inner)
    }

    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError> {
        if self.fail_set.load(Ordering::SeqCst) {
            return Err(ContractError::KnownFailure(
                "injected set_kill_latch fault".to_string(),
            ));
        }
        self.effects.fetch_add(1, Ordering::SeqCst);
        LedgerApi::set_kill_latch(&mut self.inner, latched)
    }

    fn put_control_receipt(&mut self, receipt: &ControlReceipt) -> Result<(), ContractError> {
        LedgerApi::record_control_receipt(&mut self.inner, receipt)
    }

    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        LedgerApi::control_receipts(&self.inner)
    }
}

/// The per-test fixture: a real in-process ledger plus the injected authority,
/// ownership, stop, and clock seams.
struct Fixture {
    dir: ScratchDir,
    ledger: LedgerHandle,
    owner: Arc<TestOwner>,
    authority: Arc<TestAuthority>,
    stop: Arc<dyn StopControl + Send + Sync>,
    clock: Arc<AtomicU64>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = ScratchDir::new(name);
        let ledger = LedgerHandle::in_process(&dir).expect("bind in-process ledger");
        let stop: Arc<dyn StopControl + Send + Sync> =
            Arc::new(BoundStopControl::new(dir.path(), ledger.clone()));
        Fixture {
            dir,
            ledger,
            owner: Arc::new(TestOwner::new(true)),
            authority: Arc::new(TestAuthority::approved(FAR_FUTURE_MS)),
            stop,
            clock: Arc::new(AtomicU64::new(START_MS)),
        }
    }

    fn clock_arc(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let clock = Arc::clone(&self.clock);
        Arc::new(move || clock.load(Ordering::SeqCst))
    }

    fn service(&self, runtime: Option<Arc<dyn RuntimeObservationSource>>) -> FactoryControlService {
        self.service_over(Box::new(self.ledger.clone()), runtime)
    }

    fn service_over(
        &self,
        ledger: Box<dyn FactoryControlLedger>,
        runtime: Option<Arc<dyn RuntimeObservationSource>>,
    ) -> FactoryControlService {
        self.service_full(ledger, Arc::clone(&self.stop), runtime)
    }

    /// Build a service over an explicit ledger and stop control (used by the
    /// genuine-reopen and fault-injection cases).
    fn service_full(
        &self,
        ledger: Box<dyn FactoryControlLedger>,
        stop: Arc<dyn StopControl + Send + Sync>,
        runtime: Option<Arc<dyn RuntimeObservationSource>>,
    ) -> FactoryControlService {
        let mut service = FactoryControlService::new(
            REPOSITORY_ID,
            STATE_ID,
            ledger,
            self.dir.path(),
        )
        .with_stop(stop);
        let authority: Arc<dyn ControlBindingResolver> = self.authority.clone();
        let ownership: Arc<dyn ControlOwnership> = self.owner.clone();
        service = service
            .with_authority(authority)
            .with_ownership(ownership)
            .with_clock(self.clock_arc());
        if let Some(runtime) = runtime {
            service = service.with_runtime(runtime);
        }
        service
    }

    fn healthy_runtime() -> Arc<SourceRuntime> {
        Arc::new(SourceRuntime::healthy())
    }

    fn receipts(&self) -> Vec<ControlReceipt> {
        LedgerApi::control_receipts(&self.ledger).expect("read receipts")
    }

    fn latch(&self) -> bool {
        LedgerApi::kill_latch(&self.ledger).expect("read latch")
    }
}

/// Build a service from explicit seams. Used after a genuine ledger
/// close/reopen, where the original [`Fixture`] (and its ledger handles) has
/// been dropped.
fn service_over_parts(
    state_dir: &std::path::Path,
    ledger: Box<dyn FactoryControlLedger>,
    stop: Arc<dyn StopControl + Send + Sync>,
    owner: Arc<TestOwner>,
    authority: Arc<TestAuthority>,
    clock: Arc<AtomicU64>,
) -> FactoryControlService {
    let authority: Arc<dyn ControlBindingResolver> = authority;
    let ownership: Arc<dyn ControlOwnership> = owner;
    FactoryControlService::new(REPOSITORY_ID, STATE_ID, ledger, state_dir)
        .with_stop(stop)
        .with_authority(authority)
        .with_ownership(ownership)
        .with_clock(Arc::new(move || clock.load(Ordering::SeqCst)))
        .with_runtime(Fixture::healthy_runtime())
}

/// Register the service and install a read and a control grant on a router.
fn router_with(service: FactoryControlService) -> (CapabilityRouter, LaunchGrant, LaunchGrant) {    let mut router = CapabilityRouter::new(
        Box::new(InMemoryOperationJournal::new()),
        Arc::new(RunningStop) as Arc<dyn StopAdmission>,
    );
    service.register(&mut router);
    let read_grant = grant(FACTORY_READ_CAPABILITY, FAR_FUTURE_MS);
    let control_grant = grant(FACTORY_CONTROL_CAPABILITY, FAR_FUTURE_MS);
    router.install_grant(&read_grant);
    router.install_grant(&control_grant);
    (router, read_grant, control_grant)
}

fn invoke(
    router: &mut CapabilityRouter,
    grant: &LaunchGrant,
    capability: &str,
    operation_id: &str,
    input: Value,
    deadline_unix_ms: u64,
) -> Result<Value, ContractError> {
    // A `0` deadline means "no explicit bound" in these cases; the real
    // deadline-gate tests pass an explicit near/past value.
    let deadline_unix_ms = if deadline_unix_ms == 0 {
        FAR_FUTURE_MS
    } else {
        deadline_unix_ms
    };
    router.invoke_direct(&call(
        grant,
        capability,
        operation_id,
        input,
        deadline_unix_ms,
    ))
}

fn runtime_from(value: Value) -> FactoryRuntime {
    serde_json::from_value(value).expect("FactoryRuntime/v1 decodes")
}

fn response_from(value: Value) -> FactoryControlResponse {
    FactoryControlResponse::from_value(value).expect("FactoryControl/v1 response decodes")
}

fn outcome_of(response: &FactoryControlResponse) -> Option<FactoryControlOutcome> {
    match response {
        FactoryControlResponse::Receipt(receipt) => Some(receipt.receipt.outcome),
        _ => None,
    }
}

/// Build a raw `factory_control_v1` ledger row (malformed-state tests).
#[allow(clippy::too_many_arguments)]
fn raw_control_row(
    row_id: &str,
    action: &str,
    request: &FactoryActionRequest,
    digest: &str,
    stage: &str,
    outcome: Option<&str>,
    reason: Option<&str>,
) -> ControlReceipt {
    ControlReceipt {
        action_id: row_id.to_string(),
        action: action.to_string(),
        target: String::new(),
        actor: "human".to_string(),
        request: json!({
            "factory_control_v1": {
                "stage": stage,
                "request": request,
                "request_digest": digest,
                "outcome": outcome,
                "reason": reason,
            }
        }),
        recorded_at_unix_ms: START_MS,
    }
}

/// Parse the public action object from its JSON body.
fn action_request(operation: &str, action_id: &str, confirmation: Option<&str>) -> FactoryActionRequest {
    serde_json::from_value(action_input(operation, action_id, confirmation))
        .expect("action request decodes")
}

/// Build an action request that bypasses the public validator, so tests can
/// persist a structurally decodable but contract-invalid embedded request.
fn invalid_action(
    format: &str,
    operation: &str,
    action_id: &str,
    confirmation: Option<String>,
) -> FactoryActionRequest {
    FactoryActionRequest {
        format: format.to_string(),
        operation: operation.to_string(),
        action_id: action_id.to_string(),
        confirmation,
    }
}

// ---------------------------------------------------------------------------
// Runtime observation
// ---------------------------------------------------------------------------

#[test]
fn snapshot_without_a_source_is_a_bounded_unavailable_observation() {
    let fixture = Fixture::new("fc-unavailable");
    let (mut router, read_grant, _) = router_with(fixture.service(None));
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "read-unavailable",
        snapshot_input(),
        0,
    )
    .expect("unavailable snapshot is served");
    let runtime = runtime_from(value);
    runtime.validate().expect("snapshot validates");
    assert_eq!(runtime.status, FactoryRuntimeStatus::Unavailable);
    assert!(runtime.runtime_instance_id.is_none(), "nullable launch identity");
    assert!(runtime.eligible.revision.is_none(), "nullable revision");
    assert!(runtime.eligible.total.is_none(), "nullable total");
    assert!(runtime.eligible.entries.is_empty(), "no claimed empty queue");
    assert!(runtime.current_execution.is_none());
    assert_eq!(runtime.sources.len(), 5, "exactly five named sources");
    assert!(
        runtime
            .sources
            .iter()
            .filter(|source| source.source != FactorySourceName::Stop)
            .all(|source| source.state == SourceState::Missing),
        "every non-stop source is explicitly missing"
    );
    assert_eq!(runtime.permissions, FactoryRuntimePermissions::default());
}

#[test]
fn snapshot_validates_scope_and_surfaces_source_faults() {
    let fixture = Fixture::new("fc-scope");
    let mut foreign = SourceRuntime::healthy();
    foreign.repository_id = "another-repo".to_string();
    let (mut router, read_grant, _) =
        router_with(fixture.service(Some(Arc::new(foreign))));
    let error = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "read-foreign",
        snapshot_input(),
        0,
    )
    .expect_err("foreign scope refused");
    assert_eq!(failure_code(&error), Some(FailureCode::InvalidRecord));

    let mut faulted = SourceRuntime::healthy();
    faulted.fault = Some("injected source fault".to_string());
    let (mut router, read_grant, _) = router_with(fixture.service(Some(Arc::new(faulted))));
    let error = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "read-fault",
        snapshot_input(),
        0,
    )
    .expect_err("source fault surfaces");
    assert!(matches!(error, ContractError::KnownFailure(_)));
}

#[test]
fn trusted_permissions_require_owner_binding_and_an_observed_stop_state() {
    let fixture = Fixture::new("fc-permissions");
    let mut runtime = SourceRuntime::healthy();
    // The source claims both flags; WEB.11 must ignore them.
    runtime.raw_permissions = FactoryRuntimePermissions {
        pause: true,
        resume: true,
    };
    let (mut router, read_grant, _) =
        router_with(fixture.service(Some(Arc::new(runtime))));

    // Running, owner present, live binding present: pause only.
    let runtime = runtime_from(
        invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            "read-1",
            snapshot_input(),
            0,
        )
        .expect("running snapshot"),
    );
    assert_eq!(
        runtime.permissions,
        FactoryRuntimePermissions {
            pause: true,
            resume: false
        }
    );

    // Missing owner disables both flags despite a healthy source.
    fixture.owner.set(false);
    let runtime = runtime_from(
        invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            "read-2",
            snapshot_input(),
            0,
        )
        .expect("no-owner snapshot"),
    );
    assert_eq!(runtime.permissions, FactoryRuntimePermissions::default());
    fixture.owner.set(true);

    // A revoked/absent control binding disables both flags.
    fixture.authority.set(None);
    let runtime = runtime_from(
        invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            "read-3",
            snapshot_input(),
            0,
        )
        .expect("no-binding snapshot"),
    );
    assert_eq!(runtime.permissions, FactoryRuntimePermissions::default());
}

#[test]
fn unknown_stop_authority_disables_controls() {
    let fixture = Fixture::new("fc-stop-unknown");
    let mut service = fixture.service(Some(Fixture::healthy_runtime()));
    service = service.with_stop(Arc::new(UnavailableStopControl::new(
        fixture.dir.path(),
        "injected stop unavailability",
    )));
    let (mut router, read_grant, _) = router_with(service);
    let runtime = runtime_from(
        invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            "read-unknown-stop",
            snapshot_input(),
            0,
        )
        .expect("snapshot with unknown stop"),
    );
    assert_eq!(runtime.permissions, FactoryRuntimePermissions::default());
    assert!(runtime.stop.is_none(), "unknown stop is not a green running");
}

#[test]
fn unknown_stop_authority_refuses_new_control_effects() {
    let fixture = Fixture::new("fc-unknown-stop-control");
    // Owner and live binding are present, but the stop authority is
    // unavailable: no new durable intent/effect may be admitted.
    let service = fixture
        .service(Some(Fixture::healthy_runtime()))
        .with_stop(Arc::new(UnavailableStopControl::new(
            fixture.dir.path(),
            "injected stop unavailability",
        )));
    let (mut router, _read, control_grant) = router_with(service);
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-unknown-stop",
        action_input("pause", "op-us", None),
        0,
    )
    .expect_err("unknown stop authority refuses a new effect");
    assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));
    assert!(fixture.receipts().is_empty(), "no intent was persisted");
    assert!(!fixture.latch(), "no effect was applied");

    // No stop control bound at all is equally fail-closed.
    let authority: Arc<dyn ControlBindingResolver> = fixture.authority.clone();
    let ownership: Arc<dyn ControlOwnership> = fixture.owner.clone();
    let bare = FactoryControlService::new(
        REPOSITORY_ID,
        STATE_ID,
        Box::new(fixture.ledger.clone()),
        fixture.dir.path(),
    )
    .with_authority(authority)
    .with_ownership(ownership)
    .with_clock(fixture.clock_arc());
    let (mut router, _read, control_grant) = router_with(bare);
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-no-stop",
        action_input("pause", "op-ns", None),
        0,
    )
    .expect_err("no stop authority refuses a new effect");
    assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));
    assert!(fixture.receipts().is_empty());
    assert!(!fixture.latch());
}

#[test]
fn unknown_stop_authority_still_replays_a_completed_result() {
    let fixture = Fixture::new("fc-unknown-stop-replay");
    // Establish a completed pause with a healthy stop.
    let (mut router, _read, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-replay-unknown-stop",
        action_input("pause", "op-rs", None),
        0,
    )
    .expect("pause applies");
    let recorded_at = match response_from(value) {
        FactoryControlResponse::Receipt(receipt) => receipt.receipt.recorded_at_unix_ms,
        other => panic!("expected a receipt, got {other:?}"),
    };

    // A fresh service over the same durable records with an unavailable stop:
    // the completed result still replays exactly (replay is preserved).
    let service = fixture
        .service(Some(Fixture::healthy_runtime()))
        .with_stop(Arc::new(UnavailableStopControl::new(
            fixture.dir.path(),
            "injected stop unavailability",
        )));
    let (mut router, read_grant, control_grant) = router_with(service);
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-replay-unknown-stop-2",
        action_input("pause", "op-rs", None),
        0,
    )
    .expect("completed result replays");
    match response_from(value) {
        FactoryControlResponse::Receipt(receipt) => {
            assert_eq!(receipt.receipt.recorded_at_unix_ms, recorded_at);
        }
        other => panic!("expected a receipt, got {other:?}"),
    }
    // Lookup is observational and equally preserved.
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-unknown-stop",
        lookup_input("op-rs"),
        0,
    )
    .expect("lookup replays");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Receipt(_)
    ));
}

#[test]
fn resolver_binding_for_a_different_plugin_is_refused() {
    let fixture = Fixture::new("fc-resolver-plugin");
    // The resolver hands back a live binding whose plugin id is not the
    // authenticated caller; the adapter must refuse it.
    fixture.authority.set(Some(ApprovedControlBinding {
        plugin_id: "another.plugin".to_string(),
        capability: FACTORY_CONTROL_CAPABILITY.to_string(),
        generation: GENERATION,
        expires_at_unix_ms: FAR_FUTURE_MS,
    }));
    let (mut router, _read, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-wrong-plugin",
        action_input("pause", "op-wp", None),
        0,
    )
    .expect_err("a binding for another plugin does not authorize this caller");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert!(fixture.receipts().is_empty());
    assert!(!fixture.latch());
}

#[test]
fn eligible_page_is_preserved_and_bounded() {
    let fixture = Fixture::new("fc-page");
    let mut runtime = SourceRuntime::healthy();
    runtime.entries = vec![FactoryEligibleEntry {
        task_id: "agalma-a1".to_string(),
        title: "queued work".to_string(),
        priority: TaskPriority::Normal,
        family: TaskFamily::Feature,
    }];
    let (mut router, read_grant, _) = router_with(fixture.service(Some(Arc::new(runtime))));
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "read-page",
        snapshot_input(),
        0,
    )
    .expect("paged snapshot");
    let runtime = runtime_from(value);
    assert_eq!(runtime.eligible.entries.len(), 1);
    assert_eq!(runtime.eligible.entries[0].task_id, "agalma-a1");
    assert_eq!(runtime.eligible.total.as_ref().map(|t| t.as_str()), Some("1"));
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

#[test]
fn lookup_reports_not_found_without_mutating_state() {
    let fixture = Fixture::new("fc-lookup-missing");
    let (mut router, read_grant, _) = router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-missing",
        lookup_input("never-submitted"),
        0,
    )
    .expect("lookup is observational");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::NotFound(_)
    ));
    assert!(fixture.receipts().is_empty(), "lookup writes nothing");
    assert!(!fixture.latch(), "lookup mutates no stop state");
}

// ---------------------------------------------------------------------------
// Control durability and replay
// ---------------------------------------------------------------------------

#[test]
fn pause_persists_intent_result_and_replays_exactly() {
    let fixture = Fixture::new("fc-pause-replay");
    let (mut router, read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-pause",
        action_input("pause", "op-1", None),
        0,
    )
    .expect("pause applies");
    let FactoryControlResponse::Receipt(receipt) = response_from(value) else {
        panic!("expected a terminal receipt");
    };
    assert_eq!(receipt.receipt.outcome, FactoryControlOutcome::Applied);
    assert_eq!(receipt.receipt.actor, agalma_contracts::FactoryControlActor::Human);
    assert_eq!(receipt.receipt.action_id, "op-1");
    assert!(fixture.latch(), "pause set the latch");
    assert_eq!(fixture.receipts().len(), 2, "one intent + one result");
    let recorded_at = receipt.receipt.recorded_at_unix_ms.clone();

    // Lookup replays the exact persisted terminal result (immutable timestamp).
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-op-1",
        lookup_input("op-1"),
        0,
    )
    .expect("lookup replays");
    let FactoryControlResponse::Receipt(looked_up) = response_from(value) else {
        panic!("expected a terminal receipt");
    };
    assert_eq!(looked_up.receipt.recorded_at_unix_ms, recorded_at);

    // Re-submitting the identical input replays; it does not re-run the effect.
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-pause-again",
        action_input("pause", "op-1", None),
        0,
    )
    .expect("identical replay");
    let FactoryControlResponse::Receipt(replayed) = response_from(value) else {
        panic!("expected a terminal receipt");
    };
    assert_eq!(replayed.receipt.recorded_at_unix_ms, recorded_at);
    assert_eq!(fixture.receipts().len(), 2, "no second effect or record");
}

#[test]
fn resume_requires_the_typed_confirmation() {
    let fixture = Fixture::new("fc-resume-confirm");
    let (mut router, _read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    // Establish a stop first.
    invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-pause",
        action_input("pause", "op-p", None),
        0,
    )
    .expect("pause applies");

    // Resume with no confirmation is a denied (receipted) outcome.
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-resume-unconfirmed",
        action_input("resume", "op-r1", None),
        0,
    )
    .expect("resume denied is a normal outcome");
    assert_eq!(
        outcome_of(&response_from(value)),
        Some(FactoryControlOutcome::Denied)
    );
    assert!(fixture.latch(), "latch untouched by unconfirmed resume");

    // The literal confirmation applies and clears the latch.
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-resume",
        action_input("resume", "op-r2", Some("resume")),
        0,
    )
    .expect("confirmed resume applies");
    assert_eq!(
        outcome_of(&response_from(value)),
        Some(FactoryControlOutcome::Applied)
    );
    assert!(!fixture.latch(), "latch cleared");
}

#[test]
fn conflicting_input_for_a_completed_action_is_refused_with_zero_effects() {
    let fixture = Fixture::new("fc-conflict");
    let (mut router, _read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-1",
        action_input("pause", "op-c", None),
        0,
    )
    .expect("first pause applies");
    let records = fixture.receipts().len();

    // Same action id, different submitted input (a resume) conflicts.
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-2",
        action_input("resume", "op-c", Some("resume")),
        0,
    )
    .expect_err("conflicting input refused");
    assert_eq!(failure_code(&error), Some(FailureCode::IdentityConflict));
    assert_eq!(fixture.receipts().len(), records, "no new record written");
    assert!(fixture.latch(), "no second effect");
}

#[test]
fn malformed_durable_records_are_refused() {
    let fixture = Fixture::new("fc-malformed");
    let (mut router, read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    let request = action_request("pause", "op-bad", None);
    let good_digest = request.request_digest().expect("digest");

    // (a) A result row carrying an intent stage: the row id and stage disagree.
    let mut ledger = fixture.ledger.clone();
    LedgerApi::record_control_receipt(
        &mut ledger,
        &raw_control_row(
            "factory-control.result/op-bad",
            "pause",
            &request,
            &good_digest,
            "intent",
            None,
            None,
        ),
    )
    .expect("write malformed row a");

    // (b) Action name disagreeing with the embedded request operation.
    let request_b = action_request("pause", "op-bad2", None);
    LedgerApi::record_control_receipt(
        &mut ledger,
        &raw_control_row(
            "factory-control.result/op-bad2",
            "resume",
            &request_b,
            &request_b.request_digest().expect("digest"),
            "result",
            Some("applied"),
            None,
        ),
    )
    .expect("write malformed row b");

    // (c) A non-canonical request digest.
    let request_c = action_request("pause", "op-bad3", None);
    LedgerApi::record_control_receipt(
        &mut ledger,
        &raw_control_row(
            "factory-control.result/op-bad3",
            "pause",
            &request_c,
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "result",
            Some("applied"),
            None,
        ),
    )
    .expect("write malformed row c");

    for action_id in ["op-bad", "op-bad2", "op-bad3"] {
        let error = invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            &format!("lookup-{action_id}"),
            lookup_input(action_id),
            0,
        )
        .expect_err("a malformed durable record is refused");
        assert_eq!(
            failure_code(&error),
            Some(FailureCode::StateUnavailable),
            "{action_id} is malformed state: {error}"
        );
    }

    // A control action over a malformed durable row is refused before any
    // effect: it never trusts a malformed result and never applies one.
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-bad-replay",
        action_input("pause", "op-bad", None),
        0,
    )
    .expect_err("a malformed durable record refuses control");
    assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));
    assert!(!fixture.latch(), "zero effects over malformed state");
}

#[test]
fn malformed_embedded_request_and_terminal_reason_are_state_unavailable() {
    let fixture = Fixture::new("fc-malformed-embedded");
    let (mut router, read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    // The embedded request is invalid, so no canonical digest can be computed;
    // the placeholder only has to be syntactically digest-shaped. The terminal
    // reason case uses a genuinely canonical request digest.
    let placeholder =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let mut rows: Vec<(&str, ControlReceipt)> = Vec::new();

    // (a) Unknown embedded operation.
    let invalid_op = invalid_action("FactoryControl/v1", "cancel", "op-e-op", None);
    rows.push((
        "op-e-op",
        raw_control_row(
            "factory-control.result/op-e-op",
            "cancel",
            &invalid_op,
            placeholder,
            "result",
            Some("applied"),
            None,
        ),
    ));

    // (b) Unsupported embedded format version.
    let invalid_version = invalid_action("FactoryControl/v2", "pause", "op-e-ver", None);
    rows.push((
        "op-e-ver",
        raw_control_row(
            "factory-control.result/op-e-ver",
            "pause",
            &invalid_version,
            placeholder,
            "result",
            Some("applied"),
            None,
        ),
    ));

    // (c) Over-bound embedded confirmation.
    let invalid_confirmation = invalid_action(
        "FactoryControl/v1",
        "pause",
        "op-e-conf",
        Some("x".repeat(FACTORY_CONFIRMATION_MAX_BYTES + 1)),
    );
    rows.push((
        "op-e-conf",
        raw_control_row(
            "factory-control.result/op-e-conf",
            "pause",
            &invalid_confirmation,
            placeholder,
            "result",
            Some("applied"),
            None,
        ),
    ));

    // (d) Valid embedded request and digest, but an over-bound terminal reason.
    let valid = action_request("pause", "op-e-reason", None);
    let digest = valid.request_digest().expect("digest");
    let long_reason = "r".repeat(FACTORY_REASON_MAX_BYTES + 1);
    rows.push((
        "op-e-reason",
        raw_control_row(
            "factory-control.result/op-e-reason",
            "pause",
            &valid,
            &digest,
            "result",
            Some("applied"),
            Some(&long_reason),
        ),
    ));

    let mut ledger = fixture.ledger.clone();
    for (_, row) in &rows {
        LedgerApi::record_control_receipt(&mut ledger, row).expect("write malformed row");
    }

    for (action_id, _) in &rows {
        let error = invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            &format!("lookup-{action_id}"),
            lookup_input(action_id),
            0,
        )
        .expect_err("malformed durable data is refused");
        assert_eq!(
            failure_code(&error),
            Some(FailureCode::StateUnavailable),
            "{action_id}: malformed durable data must be StateUnavailable, got {error}"
        );

        // Control over the same malformed row is refused before any effect.
        let error = invoke(
            &mut router,
            &control_grant,
            FACTORY_CONTROL_CAPABILITY,
            &format!("ctrl-{action_id}"),
            action_input("pause", action_id, None),
            0,
        )
        .expect_err("malformed durable data refuses control");
        assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));
    }
    assert!(
        !fixture.latch(),
        "malformed durable data never applied an effect"
    );
    assert_eq!(
        fixture.receipts().len(),
        rows.len(),
        "no new records written over malformed state"
    );

    // Bad *caller* input keeps its input-error classification: durable-state
    // normalization never swallows a caller's malformed request.
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-bad-input-op",
        json!({
            "format": "FactoryControl/v1",
            "operation": "cancel",
            "action_id": "op-input",
            "confirmation": null,
        }),
        0,
    )
    .expect_err("unknown caller operation is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::InvalidRecord));

    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-bad-input-version",
        json!({
            "format": "FactoryControl/v2",
            "operation": "pause",
            "action_id": "op-input",
            "confirmation": null,
        }),
        0,
    )
    .expect_err("unsupported caller version is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::UnsupportedVersion));
}

#[test]
fn conflicting_intent_and_result_pair_is_refused_before_replay() {
    let fixture = Fixture::new("fc-pair-conflict");
    let (mut router, read_grant, _control) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    // A present intent and result whose embedded requests disagree.
    let intent_req = action_request("pause", "op-pair", None);
    let result_req = action_request("resume", "op-pair", Some("resume"));
    let mut ledger = fixture.ledger.clone();
    LedgerApi::record_control_receipt(
        &mut ledger,
        &raw_control_row(
            "factory-control.intent/op-pair",
            "pause",
            &intent_req,
            &intent_req.request_digest().expect("digest"),
            "intent",
            None,
            None,
        ),
    )
    .expect("write intent row");
    LedgerApi::record_control_receipt(
        &mut ledger,
        &raw_control_row(
            "factory-control.result/op-pair",
            "resume",
            &result_req,
            &result_req.request_digest().expect("digest"),
            "result",
            Some("applied"),
            None,
        ),
    )
    .expect("write result row");

    let error = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-pair",
        lookup_input("op-pair"),
        0,
    )
    .expect_err("a conflicting intent/result pair is refused, never replayed");
    assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));
}

#[test]
fn missing_owner_revoked_binding_and_stale_generation_disable_control() {
    let fixture = Fixture::new("fc-authority");
    let (mut router, _read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    fixture.owner.set(false);
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-no-owner",
        action_input("pause", "op-a", None),
        0,
    )
    .expect_err("no owner refuses control");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert!(fixture.receipts().is_empty(), "zero effects and zero records");
    fixture.owner.set(true);

    fixture.authority.set(None);
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-revoked",
        action_input("pause", "op-b", None),
        0,
    )
    .expect_err("revoked binding refuses control");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert!(fixture.receipts().is_empty());

    // A live binding pinned to another generation does not authorize this grant.
    fixture.authority.set(Some(ApprovedControlBinding {
        plugin_id: PLUGIN.to_string(),
        capability: FACTORY_CONTROL_CAPABILITY.to_string(),
        generation: GENERATION + 1,
        expires_at_unix_ms: FAR_FUTURE_MS,
    }));
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-stale",
        action_input("pause", "op-d", None),
        0,
    )
    .expect_err("stale generation refuses control");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert!(fixture.receipts().is_empty());
}

// ---------------------------------------------------------------------------
// Fault injection across restart
// ---------------------------------------------------------------------------

#[test]
fn intent_write_fault_never_authorizes_the_effect() {
    let fixture = Fixture::new("fc-intent-fault");
    let fault = FaultLedger::new(fixture.ledger.clone());
    fault.fail_put_number.store(1, Ordering::SeqCst);
    let (mut router, _read, control_grant) =
        router_with(fixture.service_over(Box::new(fault), Some(Fixture::healthy_runtime())));

    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-intent-fault",
        action_input("pause", "op-i", None),
        0,
    )
    .expect_err("intent write fault is uncertainty");
    assert!(matches!(error, ContractError::KnownFailure(_)));
    assert!(!fixture.latch(), "effect never ran without a durable intent");
    assert!(fixture.receipts().is_empty(), "nothing durable was written");
}

#[test]
fn effect_fault_leaves_an_orphan_intent_that_is_unconfirmed_across_restart() {
    let fixture = Fixture::new("fc-effect-fault");
    let fault = FaultLedger::new(fixture.ledger.clone());
    fault.fail_set_latch.store(true, Ordering::SeqCst);
    let (mut router, _read_grant, control_grant) =
        router_with(fixture.service_over(Box::new(fault), Some(Fixture::healthy_runtime())));

    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-effect-fault",
        action_input("pause", "op-e", None),
        0,
    )
    .expect("effect uncertainty is reported as unconfirmed");
    assert!(matches!(
        response_from(error),
        FactoryControlResponse::Unconfirmed(_)
    ));
    assert!(!fixture.latch());
    assert_eq!(fixture.receipts().len(), 1, "only the durable intent remains");

    // Restart over the same real ledger: the orphan intent is never reexecuted.
    let (mut router, read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-effect-replay",
        action_input("pause", "op-e", None),
        0,
    )
    .expect("orphan intent is answered");
    let FactoryControlResponse::Unconfirmed(unconfirmed) = response_from(value) else {
        panic!("expected an unconfirmed response");
    };
    assert_eq!(unconfirmed.action_id, "op-e");
    assert!(!fixture.latch(), "orphan intent was not reexecuted");
    assert_eq!(fixture.receipts().len(), 1, "no result fabricated");

    // The same holds for read-only lookup across the restart.
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-orphan",
        lookup_input("op-e"),
        0,
    )
    .expect("lookup of an orphan intent");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));
}

#[test]
fn orphan_intent_survives_a_genuine_ledger_reopen() {
    let fixture = Fixture::new("fc-reopen-orphan");
    // Phase 1: an effect fault after the durable intent leaves an orphan intent.
    let fault = FaultLedger::new(fixture.ledger.clone());
    fault.fail_set_latch.store(true, Ordering::SeqCst);
    let service = fixture.service_over(Box::new(fault), Some(Fixture::healthy_runtime()));
    let (mut router, _read, control_grant) = router_with(service);
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-reopen-orphan",
        action_input("pause", "op-reopen", None),
        0,
    )
    .expect("effect uncertainty is unconfirmed");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));
    drop(router);

    // Phase 2: a genuinely fresh binding to the existing ledger (a new
    // connection, not a clone of the still-open handle).
    let reopened = LedgerHandle::in_process_existing(fixture.dir.path())
        .expect("reopen the existing ledger");
    assert_eq!(
        LedgerApi::control_receipts(&reopened).expect("receipts").len(),
        1,
        "the intent is durable on disk"
    );
    let stop: Arc<dyn StopControl + Send + Sync> =
        Arc::new(BoundStopControl::new(fixture.dir.path(), reopened.clone()));
    let service = fixture.service_full(
        Box::new(reopened),
        stop,
        Some(Fixture::healthy_runtime()),
    );
    let (mut router, read_grant, control_grant) = router_with(service);
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-reopen-orphan-2",
        action_input("pause", "op-reopen", None),
        0,
    )
    .expect("the orphan intent is answered without reexecution");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));
    assert!(!fixture.latch(), "the orphan intent was never reexecuted");
    assert_eq!(fixture.receipts().len(), 1, "no result fabricated");
    // Read-only lookup across the same reopen agrees.
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-reopen-orphan",
        lookup_input("op-reopen"),
        0,
    )
    .expect("lookup of the orphan intent");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));
}

#[test]
fn completed_result_replays_across_a_genuine_ledger_reopen() {
    let fixture = Fixture::new("fc-reopen-result");
    let (mut router, _read, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-reopen-result",
        action_input("pause", "op-reopen-result", None),
        0,
    )
    .expect("pause applies");
    let recorded_at = match response_from(value) {
        FactoryControlResponse::Receipt(receipt) => receipt.receipt.recorded_at_unix_ms,
        other => panic!("expected a receipt, got {other:?}"),
    };
    drop(router);

    // Fresh binding to the existing ledger.
    let reopened = LedgerHandle::in_process_existing(fixture.dir.path())
        .expect("reopen the existing ledger");
    let stop: Arc<dyn StopControl + Send + Sync> =
        Arc::new(BoundStopControl::new(fixture.dir.path(), reopened.clone()));
    let service = fixture.service_full(
        Box::new(reopened),
        stop,
        Some(Fixture::healthy_runtime()),
    );
    let (mut router, read_grant, control_grant) = router_with(service);

    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-reopen-result",
        lookup_input("op-reopen-result"),
        0,
    )
    .expect("lookup replays the persisted result");
    match response_from(value) {
        FactoryControlResponse::Receipt(receipt) => {
            assert_eq!(receipt.receipt.recorded_at_unix_ms, recorded_at);
        }
        other => panic!("expected a receipt, got {other:?}"),
    }

    // Re-submitting the identical input replays the immutable timestamp and
    // performs no second effect.
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-reopen-result-2",
        action_input("pause", "op-reopen-result", None),
        0,
    )
    .expect("identical replay");
    match response_from(value) {
        FactoryControlResponse::Receipt(receipt) => {
            assert_eq!(receipt.receipt.recorded_at_unix_ms, recorded_at);
        }
        other => panic!("expected a receipt, got {other:?}"),
    }
    assert_eq!(fixture.receipts().len(), 2, "one intent + one result, no more");
}

#[test]
fn result_write_fault_reports_uncertainty_not_a_receipt() {
    let fixture = Fixture::new("fc-result-fault");
    let fault = FaultLedger::new(fixture.ledger.clone());
    fault.fail_put_number.store(2, Ordering::SeqCst);
    let (mut router, _read, control_grant) =
        router_with(fixture.service_over(Box::new(fault), Some(Fixture::healthy_runtime())));

    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-result-fault",
        action_input("pause", "op-w", None),
        0,
    )
    .expect("result fault reports uncertainty");
    let FactoryControlResponse::Unconfirmed(unconfirmed) = response_from(value) else {
        panic!("expected an unconfirmed response, never a terminal receipt");
    };
    assert!(!unconfirmed.reason.is_empty());
    assert!(fixture.latch(), "the effect did happen");
    assert_eq!(fixture.receipts().len(), 1, "only the intent is durable");
}

#[test]
fn intent_commit_with_lost_ack_is_orphan_uncertainty_across_close_and_reopen() {
    let mut fixture = Fixture::new("fc-lost-ack-intent");
    let path = fixture.dir.path().to_path_buf();
    let fault = FaultLedger::new(fixture.ledger.clone());
    // The intent write (put 1) commits durably, then its acknowledgment is lost.
    fault.lose_ack_number.store(1, Ordering::SeqCst);
    let service = fixture.service_over(Box::new(fault), Some(Fixture::healthy_runtime()));
    let (mut router, _read, control_grant) = router_with(service);

    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-la-intent",
        action_input("pause", "op-la-intent", None),
        0,
    )
    .expect_err("a lost intent acknowledgment is uncertainty, never success");
    assert_eq!(
        failure_code(&error),
        None,
        "the injected lost-ack fault is an unclassified transport error, not a caller error"
    );
    assert!(
        !fixture.latch(),
        "no effect ran without a confirmed intent"
    );
    // Commit-then-error: the intent *did* land durably.
    let receipts = fixture.receipts();
    assert_eq!(receipts.len(), 1, "the intent is durably committed");
    assert_eq!(receipts[0].action_id, "factory-control.intent/op-la-intent");

    // Genuinely close every ledger handle, retain the state directory on disk,
    // then reopen it through a fresh binding.
    drop(router);
    fixture.dir.keep();
    drop(fixture);
    let reopened = LedgerHandle::in_process_existing(&path)
        .expect("reopen the existing ledger");
    assert_eq!(
        LedgerApi::control_receipts(&reopened)
            .expect("receipts")
            .len(),
        1,
        "the committed intent survived the close/reopen",
    );
    let stop: Arc<dyn StopControl + Send + Sync> =
        Arc::new(BoundStopControl::new(&path, reopened.clone()));
    let service = service_over_parts(
        &path,
        Box::new(reopened.clone()),
        stop,
        Arc::new(TestOwner::new(true)),
        Arc::new(TestAuthority::approved(FAR_FUTURE_MS)),
        Arc::new(AtomicU64::new(START_MS)),
    );
    let (mut router, read_grant, control_grant) = router_with(service);

    // Read-only lookup reports the orphan intent as unconfirmed.
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-la-intent",
        lookup_input("op-la-intent"),
        0,
    )
    .expect("the orphan intent is answered");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));

    // Re-submitting the same action never reexecutes the orphan intent.
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-la-intent-2",
        action_input("pause", "op-la-intent", None),
        0,
    )
    .expect("the orphan intent is answered without reexecution");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));
    assert!(
        !LedgerApi::kill_latch(&reopened).expect("latch"),
        "a committed-but-unacknowledged intent applies zero effects"
    );
    assert_eq!(
        LedgerApi::control_receipts(&reopened).expect("receipts").len(),
        1,
        "no result was fabricated for the orphan intent"
    );
}

#[test]
fn result_commit_with_lost_ack_replays_exactly_without_a_second_effect() {
    let mut fixture = Fixture::new("fc-lost-ack-result");
    let path = fixture.dir.path().to_path_buf();
    let fault = FaultLedger::new(fixture.ledger.clone());
    // Put 1 (intent) is acknowledged; put 2 (result) commits, then loses its ack.
    fault.lose_ack_number.store(2, Ordering::SeqCst);
    let service = fixture.service_over(Box::new(fault), Some(Fixture::healthy_runtime()));
    let (mut router, _read, control_grant) = router_with(service);

    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-la-result",
        action_input("pause", "op-la-result", None),
        0,
    )
    .expect("a lost result acknowledgment reports uncertainty");
    let FactoryControlResponse::Unconfirmed(unconfirmed) = response_from(value) else {
        panic!("a lost result acknowledgement must never be a terminal receipt");
    };
    assert!(!unconfirmed.reason.is_empty());
    assert!(fixture.latch(), "the effect did happen before the result write");

    let receipts = fixture.receipts();
    assert_eq!(receipts.len(), 2, "the result is durably committed");
    let durable = receipts
        .iter()
        .find(|receipt| receipt.action_id == "factory-control.result/op-la-result")
        .expect("committed result row");
    let recorded_at = durable.recorded_at_unix_ms.to_string();

    // Genuinely close every ledger handle, retain the state, then reopen.
    drop(router);
    fixture.dir.keep();
    drop(fixture);
    let reopened = LedgerHandle::in_process_existing(&path)
        .expect("reopen the existing ledger");
    let effects = Arc::new(AtomicU64::new(0));
    let counting = CountingLedger {
        inner: reopened.clone(),
        effects: Arc::clone(&effects),
        fail_set: Arc::new(AtomicBool::new(false)),
    };
    let stop: Arc<dyn StopControl + Send + Sync> =
        Arc::new(BoundStopControl::new(&path, reopened.clone()));
    let service = service_over_parts(
        &path,
        Box::new(counting),
        stop,
        Arc::new(TestOwner::new(true)),
        Arc::new(TestAuthority::approved(FAR_FUTURE_MS)),
        Arc::new(AtomicU64::new(START_MS)),
    );
    let (mut router, read_grant, control_grant) = router_with(service);

    // The lookup replays the exact completed result, including its timestamp.
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-la-result",
        lookup_input("op-la-result"),
        0,
    )
    .expect("lookup replays the committed result");
    let FactoryControlResponse::Receipt(receipt) = response_from(value) else {
        panic!("a committed result replays as a receipt");
    };
    assert_eq!(receipt.receipt.outcome, FactoryControlOutcome::Applied);
    assert_eq!(receipt.receipt.recorded_at_unix_ms.as_str(), recorded_at);

    // Re-submitting the identical input replays the result and applies no
    // second effect (independently counted).
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-la-result-2",
        action_input("pause", "op-la-result", None),
        0,
    )
    .expect("identical replay");
    let FactoryControlResponse::Receipt(receipt) = response_from(value) else {
        panic!("identical replay returns the receipt");
    };
    assert_eq!(receipt.receipt.recorded_at_unix_ms.as_str(), recorded_at);
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "a committed result replays without a second effect"
    );
    assert_eq!(
        LedgerApi::control_receipts(&reopened).expect("receipts").len(),
        2,
        "one intent + one committed result, no more"
    );
}

#[test]
fn deadline_after_a_durable_intent_reports_unknown_not_cancellation() {
    let fixture = Fixture::new("fc-deadline-intent");
    let fault = FaultLedger::new(fixture.ledger.clone());
    fault.fail_set_latch.store(true, Ordering::SeqCst);
    let (mut router, _read, control_grant) =
        router_with(fixture.service_over(Box::new(fault), Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-deadline-intent",
        action_input("pause", "op-dl", None),
        0,
    )
    .expect("effect uncertainty reported as unconfirmed");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));

    // Push the service clock past the deadline but keep the router deadline
    // admissible: the orphan intent is still answered as unconfirmed.
    fixture.clock.store(5_000_000_000_000, Ordering::SeqCst);
    let (mut router, _read, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-deadline-replay",
        action_input("pause", "op-dl", None),
        FAR_FUTURE_MS,
    )
    .expect("orphan intent answered");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::Unconfirmed(_)
    ));
}

#[test]
fn fresh_over_deadline_refuses_before_any_effect() {
    let fixture = Fixture::new("fc-deadline-fresh");
    let (mut router, _read, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-deadline-fresh",
        action_input("pause", "op-fd", None),
        START_MS + 1,
    );
    // The router admits the deadline; the adapter refuses it before the effect.
    let error = value.expect_err("expired deadline refuses a new effect");
    assert_eq!(failure_code(&error), Some(FailureCode::DeadlineExceeded));
    assert!(!fixture.latch());
    assert!(fixture.receipts().is_empty());
}

// ---------------------------------------------------------------------------
// Serialized concurrency and slow-source responsiveness
// ---------------------------------------------------------------------------

#[test]
fn concurrent_duplicate_actions_serialize_to_one_effect() {
    let fixture = Fixture::new("fc-concurrent");
    let service = fixture.service(Some(Fixture::healthy_runtime()));
    let (router, _read, control_grant) = router_with(service);
    let service = Arc::new(RouterService::new(router));
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|index| {
            let service = Arc::clone(&service);
            let grant = control_grant.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let request = call(
                    &grant,
                    FACTORY_CONTROL_CAPABILITY,
                    &format!("ctrl-concurrent-{index}"),
                    action_input("pause", "op-shared", None),
                    FAR_FUTURE_MS,
                );
                barrier.wait();
                service
                    .invoke_capability(serde_json::to_value(&request).unwrap())
                    .expect("concurrent duplicate is served")
            })
        })
        .collect();

    let values: Vec<Value> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let timestamps: Vec<String> = values
        .iter()
        .map(|value| match response_from(value.clone()) {
            FactoryControlResponse::Receipt(receipt) => {
                receipt.receipt.recorded_at_unix_ms.as_str().to_string()
            }
            other => panic!("expected a receipt, got {other:?}"),
        })
        .collect();
    assert_eq!(timestamps[0], timestamps[1], "one immutable result replayed");
    assert!(fixture.latch(), "the single effect applied");
    assert_eq!(fixture.receipts().len(), 2, "one intent + one result");
}

#[test]
fn a_slow_read_does_not_block_control() {
    struct GatedRuntime {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl RuntimeObservationSource for GatedRuntime {
        fn snapshot(
            &self,
            _request: &FactoryRuntimeRequest,
        ) -> Result<FactoryRuntime, ContractError> {
            let _ = self.entered.send(());
            let _ = self.release.lock().unwrap().recv();
            Ok(SourceRuntime::healthy().build())
        }
    }

    let fixture = Fixture::new("fc-slow-read");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let service = fixture.service(Some(Arc::new(GatedRuntime {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    })));
    let (router, read_grant, control_grant) = router_with(service);
    let service = Arc::new(RouterService::new(router));
    let read_done = Arc::new(AtomicBool::new(false));

    let read = {
        let service = Arc::clone(&service);
        let grant = read_grant.clone();
        let read_done = Arc::clone(&read_done);
        thread::spawn(move || {
            let request = call(
                &grant,
                FACTORY_READ_CAPABILITY,
                "slow-read",
                snapshot_input(),
                FAR_FUTURE_MS,
            );
            let value = service
                .invoke_capability(serde_json::to_value(&request).unwrap())
                .expect("slow read completes");
            read_done.store(true, Ordering::SeqCst);
            value
        })
    };

    // Wait until the read source is actively held, then prove control answers
    // while the read is still blocked (ordering asserted, not just eventual).
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the read entered the source");
    let control_request = call(
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "control-during-read",
        action_input("pause", "op-slow", None),
        FAR_FUTURE_MS,
    );
    let control_value = service
        .invoke_capability(serde_json::to_value(&control_request).unwrap())
        .expect("control completes during a slow read");
    assert_eq!(
        outcome_of(&response_from(control_value)),
        Some(FactoryControlOutcome::Applied)
    );
    assert!(
        !read_done.load(Ordering::SeqCst),
        "control answered before the read was released"
    );
    assert!(fixture.latch());

    release_tx.send(()).unwrap();
    let read_value = read.join().unwrap();
    let runtime = runtime_from(read_value);
    assert_eq!(runtime.repository_id, REPOSITORY_ID);
}

#[test]
fn control_timeout_before_admission_is_refused() {
    let fixture = Fixture::new("fc-ctrl-timeout-queued");
    let service = fixture.service(Some(Fixture::healthy_runtime()));
    let (router, _read, control_grant) = router_with(service);
    // Hold the control adapter lock so the request stays queued and never
    // reaches the effect.
    let probe = call(
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "probe-queued",
        action_input("pause", "op-q", None),
        FAR_FUTURE_MS,
    );
    let (_scope, adapter) = router.prepare_direct(&probe).expect("control adapter");
    let held = adapter.lock().unwrap();
    let service = Arc::new(RouterService::new(router));

    let deadline = wall_clock_now_ms() + 60;
    let request = call(
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-queued-timeout",
        action_input("pause", "op-q", None),
        deadline,
    );
    let error = service
        .invoke_capability(serde_json::to_value(&request).unwrap())
        .expect_err("a queued control times out as a refusal");
    assert_eq!(
        failure_code(&error),
        Some(FailureCode::DeadlineExceeded),
        "the pre-admission timeout is a plain refusal: {error}"
    );
    drop(held);
    assert!(!fixture.latch(), "no effect was admitted");
    assert!(fixture.receipts().is_empty(), "no durable intent was written");
}

#[test]
fn control_timeout_after_admission_is_unconfirmed_and_lookup_resolves() {
    let fixture = Fixture::new("fc-ctrl-timeout-admitted");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let service = fixture.service_over(
        Box::new(BlockingLedger {
            inner: fixture.ledger.clone(),
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
        }),
        Some(Fixture::healthy_runtime()),
    );
    let (router, read_grant, control_grant) = router_with(service);
    let service = Arc::new(RouterService::new(router));

    let deadline = wall_clock_now_ms() + 80;
    let request = call(
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-admitted-timeout",
        action_input("pause", "op-at", None),
        deadline,
    );
    let invoke_service = Arc::clone(&service);
    let handle = thread::spawn(move || {
        invoke_service.invoke_capability(serde_json::to_value(&request).unwrap())
    });

    // The durable intent write is in progress: admission has happened.
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the intent write entered");
    let value = handle
        .join()
        .unwrap()
        .expect("a post-admission timeout is a typed unconfirmed, not an error");
    let FactoryControlResponse::Unconfirmed(unconfirmed) = response_from(value) else {
        panic!("expected a typed unconfirmed response after admission");
    };
    assert_eq!(unconfirmed.action_id, "op-at");
    assert_eq!(unconfirmed.code, FactoryUnconfirmedCode::EffectUnconfirmed);
    assert_eq!(
        unconfirmed.request_digest,
        action_request("pause", "op-at", None)
            .request_digest()
            .unwrap(),
        "the uncertainty is bound to the exact original request"
    );

    // Release the blocked write; the effect then completes durably.
    release_tx.send(()).unwrap();
    let stop_wait = std::time::Instant::now() + Duration::from_secs(5);
    while fixture.receipts().len() < 2 {
        assert!(
            std::time::Instant::now() < stop_wait,
            "the worker did not finish persisting the effect"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fixture.latch(), "the effect applied after release");

    // Lookup resolves the unknown outcome to the persisted terminal receipt.
    let lookup = call(
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-admitted-timeout",
        lookup_input("op-at"),
        FAR_FUTURE_MS,
    );
    let value = service
        .invoke_capability(serde_json::to_value(&lookup).unwrap())
        .expect("lookup resolves the unknown effect");
    let FactoryControlResponse::Receipt(receipt) = response_from(value) else {
        panic!("expected the terminal receipt after the effect completed");
    };
    assert_eq!(receipt.receipt.action_id, "op-at");
    assert_eq!(receipt.receipt.outcome, FactoryControlOutcome::Applied);
}

// ---------------------------------------------------------------------------
// Route exemption scoping and grant bounds
// ---------------------------------------------------------------------------

#[test]
fn route_exemption_is_scoped_to_the_declared_operations() {
    let fixture = Fixture::new("fc-routes");
    let (mut router, read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    // factory.control refuses a lookup (it is a factory.read operation).
    let error = invoke(
        &mut router,
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-lookup",
        lookup_input("op-x"),
        0,
    )
    .expect_err("lookup is not a control action");
    assert!(matches!(
        error,
        ContractError::UnsupportedCapability(_)
    ));

    // factory.read refuses a pause represented with the wrong shape.
    let error = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "read-pause",
        action_input("pause", "op-x", None),
        0,
    )
    .expect_err("read does not accept control actions");
    assert_eq!(failure_code(&error), Some(FailureCode::InvalidRecord));

    // Malformed and unknown operations are refused.
    let error = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "read-bad",
        json!({ "format": "FactoryRuntime/v2", "operation": "snapshot" }),
        0,
    )
    .expect_err("unknown runtime version refused");
    assert_eq!(failure_code(&error), Some(FailureCode::UnsupportedVersion));
}

#[test]
fn namespace_method_and_expiry_gates_refuse_before_effects() {
    let fixture = Fixture::new("fc-grants");
    let (mut router, read_grant, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));

    // A control grant presented for the read namespace is refused.
    let request = call(
        &control_grant,
        FACTORY_READ_CAPABILITY,
        "ctrl-ns",
        snapshot_input(),
        0,
    );
    let error = router
        .invoke_direct(&request)
        .expect_err("namespace mismatch refused");
    assert_eq!(failure_code(&error), Some(FailureCode::MissingCapability));

    // Expired grant: a grant with a past expiry is refused by authentication.
    let expired = grant(FACTORY_CONTROL_CAPABILITY, START_MS);
    router.install_grant(&expired);
    let error = invoke(
        &mut router,
        &expired,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-expired",
        action_input("pause", "op-exp", None),
        0,
    )
    .expect_err("expired grant refused");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert!(fixture.receipts().is_empty());
    let _ = read_grant;
}

#[test]
fn grants_are_bound_to_the_exact_generation() {
    let fixture = Fixture::new("fc-generation");
    let (mut router, _read, control_grant) =
        router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let mut request = call(
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "ctrl-gen",
        action_input("pause", "op-gen", None),
        0,
    );
    request.generation = GENERATION + 1;
    let error = router
        .invoke_direct(&request)
        .expect_err("stale generation refused");
    assert_eq!(failure_code(&error), Some(FailureCode::StaleGeneration));
}

// ---------------------------------------------------------------------------
// Legacy CLI/TUI regression
// ---------------------------------------------------------------------------

#[test]
fn legacy_control_apply_is_unchanged_and_does_not_share_api_ids() {
    let fixture = Fixture::new("fc-legacy");
    let mut cancel = NoCancel;
    let mut legacy_ledger = fixture.ledger.clone();
    let request = ControlRequest::new("legacy-pause-1", ControlAction::Pause, Actor::Human);
    let outcome = {
        let mut plane = ControlPlane::new(&mut legacy_ledger, &mut cancel, fixture.dir.path());
        plane.apply(&request).expect("legacy apply")
    };
    assert_eq!(outcome, ControlOutcome::Applied);
    assert!(fixture.latch(), "legacy apply set the latch");
    let receipts = fixture.receipts();
    assert_eq!(receipts.len(), 1, "legacy apply records its raw action id");
    assert_eq!(receipts[0].action_id, "legacy-pause-1");

    // The API lookup for that raw id is authoritative not_found: the reserved
    // namespaced ids never collide with a legacy CLI/TUI action id.
    let (mut router, read_grant, _) = router_with(fixture.service(Some(Fixture::healthy_runtime())));
    let value = invoke(
        &mut router,
        &read_grant,
        FACTORY_READ_CAPABILITY,
        "lookup-legacy",
        lookup_input("legacy-pause-1"),
        0,
    )
    .expect("lookup");
    assert!(matches!(
        response_from(value),
        FactoryControlResponse::NotFound(_)
    ));
}

// ---------------------------------------------------------------------------
// Deterministic property tests
// ---------------------------------------------------------------------------

/// Recorded property seed (xorshift64).
const PROPERTY_SEED: u64 = 0x5745_4211;
/// Control-sequence property budget.
const CONTROL_SEQUENCE_CASES: u64 = 128;
/// Runtime/source/page property budget.
const RUNTIME_CASE_BUDGET: u64 = 64;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

#[test]
fn property_control_sequences_have_exactly_one_effect_and_immutable_replays() {
    let mut rng = Rng::new(PROPERTY_SEED);
    for case in 0..CONTROL_SEQUENCE_CASES {
        let fixture = Fixture::new(&format!("fc-prop-control-{case}"));
        let (mut router, _read, control_grant) =
            router_with(fixture.service(Some(Fixture::healthy_runtime())));

        // Independent reference model: action id -> (digest, outcome, timestamp).
        let mut model: BTreeMap<String, (String, bool, String)> = BTreeMap::new();
        let mut latch = false;
        let operations = rng.below(24) + 1;

        for step in 0..operations {
            let action_id = format!("op-{}", rng.below(6));
            let operation = if rng.below(2) == 0 { "pause" } else { "resume" };
            let confirmation = if operation == "resume" && rng.below(2) == 0 {
                Some("resume")
            } else {
                None
            };
            let request: FactoryActionRequest = serde_json::from_value(action_input(
                operation,
                &action_id,
                confirmation,
            ))
            .expect("action request decodes");
            let digest = request.request_digest().expect("digest");
            let label = format!("prop-{case}-{step}");

            let result = invoke(
                &mut router,
                &control_grant,
                FACTORY_CONTROL_CAPABILITY,
                &label,
                action_input(operation, &action_id, confirmation),
                0,
            );

            if let Some((stored_digest, stored_outcome, stored_at)) = model.get(&action_id) {
                if stored_digest != &digest {
                    let error = result.expect_err("conflicting input refused");
                    assert_eq!(failure_code(&error), Some(FailureCode::IdentityConflict));
                    assert_eq!(fixture.latch(), latch, "conflict had zero effect");
                    continue;
                }
                let response = response_from(result.expect("replay succeeds"));
                let FactoryControlResponse::Receipt(receipt) = response else {
                    panic!("replay must be a terminal receipt");
                };
                let expected = if *stored_outcome {
                    FactoryControlOutcome::Applied
                } else {
                    FactoryControlOutcome::Denied
                };
                assert_eq!(receipt.receipt.outcome, expected);
                assert_eq!(receipt.receipt.recorded_at_unix_ms.as_str(), stored_at);
                assert_eq!(fixture.latch(), latch, "replay had zero effect");
                continue;
            }

            // Fresh: compute the independent expected outcome.
            let (expected, new_latch) = match operation {
                "pause" => {
                    if latch {
                        (FactoryControlOutcome::Denied, latch)
                    } else {
                        (FactoryControlOutcome::Applied, true)
                    }
                }
                _ => {
                    if !latch {
                        (FactoryControlOutcome::Denied, latch)
                    } else if confirmation != Some("resume") {
                        (FactoryControlOutcome::Denied, latch)
                    } else {
                        (FactoryControlOutcome::Applied, false)
                    }
                }
            };
            let response = response_from(result.expect("fresh action answered"));
            let FactoryControlResponse::Receipt(receipt) = response else {
                panic!("fresh action must be a terminal receipt");
            };
            assert_eq!(receipt.receipt.outcome, expected, "case {case} step {step}");
            latch = new_latch;
            assert_eq!(fixture.latch(), latch, "observed latch matches the model");
            model.insert(
                action_id.clone(),
                (
                    digest,
                    expected == FactoryControlOutcome::Applied,
                    receipt.receipt.recorded_at_unix_ms.as_str().to_string(),
                ),
            );
        }

        // Each distinct action id left exactly one intent + one result record.
        assert_eq!(fixture.receipts().len(), model.len() * 2);
    }
}

/// Malformed-durable-state property budget.
const MALFORMED_CASE_BUDGET: u64 = 64;

#[test]
fn property_malformed_durable_state_is_state_unavailable_and_effect_free() {
    let mut rng = Rng::new(PROPERTY_SEED ^ 0x0bad_5eed);
    let placeholder = format!("sha256:{}", "0".repeat(64));
    for case in 0..MALFORMED_CASE_BUDGET {
        let fixture = Fixture::new(&format!("fc-prop-malformed-{case}"));
        let (mut router, read_grant, control_grant) =
            router_with(fixture.service(Some(Fixture::healthy_runtime())));

        let action_id = "op-prop-bad";
        let good = action_request("pause", action_id, None);
        let good_digest = good.request_digest().expect("digest");
        let long_reason = "r".repeat(FACTORY_REASON_MAX_BYTES + 1);
        let long_confirmation = "x".repeat(FACTORY_CONFIRMATION_MAX_BYTES + 1);
        let (row_id, action, request, digest, stage, outcome, reason) = match rng.below(7) {
            // Unknown embedded operation on a result row.
            0 => {
                let bad = invalid_action("FactoryControl/v1", "cancel", action_id, None);
                (
                    "factory-control.result/op-prop-bad",
                    "cancel",
                    bad,
                    placeholder.clone(),
                    "result",
                    Some("applied"),
                    None,
                )
            }
            // Unsupported embedded format on a result row.
            1 => {
                let bad = invalid_action("FactoryControl/v2", "pause", action_id, None);
                (
                    "factory-control.result/op-prop-bad",
                    "pause",
                    bad,
                    placeholder.clone(),
                    "result",
                    Some("applied"),
                    None,
                )
            }
            // Over-bound embedded confirmation on a result row.
            2 => {
                let bad = invalid_action(
                    "FactoryControl/v1",
                    "pause",
                    action_id,
                    Some(long_confirmation),
                );
                (
                    "factory-control.result/op-prop-bad",
                    "pause",
                    bad,
                    placeholder.clone(),
                    "result",
                    Some("applied"),
                    None,
                )
            }
            // Non-canonical request digest on a result row.
            3 => (
                "factory-control.result/op-prop-bad",
                "pause",
                good.clone(),
                placeholder.clone(),
                "result",
                Some("applied"),
                None,
            ),
            // Valid request/digest but an over-bound terminal reason.
            4 => (
                "factory-control.result/op-prop-bad",
                "pause",
                good.clone(),
                good_digest.clone(),
                "result",
                Some("applied"),
                Some(long_reason),
            ),
            // A result id carrying the wrong (intent) stage.
            5 => (
                "factory-control.result/op-prop-bad",
                "pause",
                good.clone(),
                good_digest.clone(),
                "intent",
                None,
                None,
            ),
            // Unknown embedded operation in an intent row.
            _ => {
                let bad = invalid_action("FactoryControl/v1", "cancel", action_id, None);
                (
                    "factory-control.intent/op-prop-bad",
                    "cancel",
                    bad,
                    placeholder.clone(),
                    "intent",
                    None,
                    None,
                )
            }
        };
        let row = raw_control_row(
            row_id,
            action,
            &request,
            &digest,
            stage,
            outcome,
            reason.as_deref(),
        );
        let mut ledger = fixture.ledger.clone();
        LedgerApi::record_control_receipt(&mut ledger, &row).expect("write malformed row");
        drop(ledger);

        let error = invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            &format!("lookup-malformed-{case}"),
            lookup_input(action_id),
            0,
        )
        .expect_err("malformed durable state is refused");
        assert_eq!(
            failure_code(&error),
            Some(FailureCode::StateUnavailable),
            "case {case}: lookup over malformed durable state"
        );

        let error = invoke(
            &mut router,
            &control_grant,
            FACTORY_CONTROL_CAPABILITY,
            &format!("ctrl-malformed-{case}"),
            action_input("pause", action_id, None),
            0,
        )
        .expect_err("malformed durable state refuses control");
        assert_eq!(
            failure_code(&error),
            Some(FailureCode::StateUnavailable),
            "case {case}: control over malformed durable state"
        );

        assert!(!fixture.latch(), "case {case}: malformed state is effect-free");
        assert_eq!(
            fixture.receipts().len(),
            1,
            "case {case}: no records are written over malformed state"
        );
    }
}

#[test]
fn property_restart_deadline_and_effect_faults_never_duplicate_effects() {
    let mut rng = Rng::new(PROPERTY_SEED ^ 0x0f0f);
    for case in 0..CONTROL_SEQUENCE_CASES {
        let fixture = Fixture::new(&format!("fc-prop-fault-{case}"));
        let effects = Arc::new(AtomicU64::new(0));
        let fail_set = Arc::new(AtomicBool::new(false));

        // The currently bound router and its control grant; both are rebuilt on
        // a simulated restart (a genuinely fresh ledger binding).
        let mut router;
        let mut control_grant;
        {
            let ledger = CountingLedger {
                inner: fixture.ledger.clone(),
                effects: Arc::clone(&effects),
                fail_set: Arc::clone(&fail_set),
            };
            let stop: Arc<dyn StopControl + Send + Sync> =
                Arc::new(BoundStopControl::new(fixture.dir.path(), fixture.ledger.clone()));
            let service =
                fixture.service_full(Box::new(ledger), stop, Some(Fixture::healthy_runtime()));
            let (r, _read, g) = router_with(service);
            router = r;
            control_grant = g;
        }

        // Independent reference model across restarts.
        let mut terminal: BTreeMap<String, (String, bool, String)> = BTreeMap::new();
        let mut orphan: BTreeSet<String> = BTreeSet::new();
        let mut applied_ids: BTreeSet<String> = BTreeSet::new();
        let operations = rng.below(16) + 1;

        for step in 0..operations {
            // Occasionally restart: reopen the ledger and rebuild the router.
            if rng.below(4) == 0 {
                let reopened = LedgerHandle::in_process_existing(fixture.dir.path())
                    .expect("reopen the existing ledger");
                let ledger = CountingLedger {
                    inner: reopened.clone(),
                    effects: Arc::clone(&effects),
                    fail_set: Arc::clone(&fail_set),
                };
                let stop: Arc<dyn StopControl + Send + Sync> =
                    Arc::new(BoundStopControl::new(fixture.dir.path(), reopened));
                let service =
                    fixture.service_full(Box::new(ledger), stop, Some(Fixture::healthy_runtime()));
                let (r, _read, g) = router_with(service);
                router = r;
                control_grant = g;
            }

            let action_id = format!("op-{}", rng.below(5));
            let operation = if rng.below(2) == 0 { "pause" } else { "resume" };
            let confirmation = if operation == "resume" && rng.below(2) == 0 {
                Some("resume")
            } else {
                None
            };
            let request: FactoryActionRequest =
                serde_json::from_value(action_input(operation, &action_id, confirmation))
                    .expect("action request decodes");
            let digest = request.request_digest().expect("digest");
            let label = format!("prop-fault-{case}-{step}");

            let known = terminal.contains_key(&action_id) || orphan.contains(&action_id);
            // An expired deadline is only chosen for an action with no durable
            // record: a fresh effect is refused before the adapter runs.
            let expired_deadline = rng.below(4) == 0 && !known;
            let inject_fault = rng.below(5) == 0 && !known;
            if inject_fault {
                fail_set.store(true, Ordering::SeqCst);
            }
            let deadline = if expired_deadline { START_MS } else { 0 };

            let result = invoke(
                &mut router,
                &control_grant,
                FACTORY_CONTROL_CAPABILITY,
                &label,
                action_input(operation, &action_id, confirmation),
                deadline,
            );
            if inject_fault {
                fail_set.store(false, Ordering::SeqCst);
            }

            match result {
                Ok(value) => match response_from(value) {
                    FactoryControlResponse::Receipt(receipt) => {
                        let applied =
                            receipt.receipt.outcome == FactoryControlOutcome::Applied;
                        if let Some((stored_digest, stored_applied, stored_at)) =
                            terminal.get(&action_id)
                        {
                            assert_eq!(stored_digest, &digest, "replay keeps the input identity");
                            assert_eq!(applied, *stored_applied, "replay keeps the outcome");
                            assert_eq!(
                                receipt.receipt.recorded_at_unix_ms.as_str(),
                                stored_at,
                                "replay keeps the immutable timestamp"
                            );
                            assert!(
                                !orphan.contains(&action_id),
                                "a terminal replay is never also an orphan"
                            );
                        } else {
                            assert!(
                                !orphan.contains(&action_id),
                                "an orphan intent never becomes a receipt by replay"
                            );
                            if applied {
                                applied_ids.insert(action_id.clone());
                            }
                            terminal.insert(
                                action_id.clone(),
                                (
                                    digest.clone(),
                                    applied,
                                    receipt.receipt.recorded_at_unix_ms.as_str().to_string(),
                                ),
                            );
                        }
                    }
                    FactoryControlResponse::Unconfirmed(_) => {
                        assert!(
                            !terminal.contains_key(&action_id),
                            "a completed result never returns unconfirmed"
                        );
                        orphan.insert(action_id.clone());
                    }
                    FactoryControlResponse::NotFound(_) => {
                        panic!("a control action never returns not_found")
                    }
                },
                Err(error) => match failure_code(&error) {
                    Some(FailureCode::IdentityConflict) => assert!(
                        known,
                        "a conflict requires a prior durable record for the action"
                    ),
                    Some(FailureCode::DeadlineExceeded) => assert!(
                        !known,
                        "an expired deadline is only chosen for a fresh action"
                    ),
                    other => panic!("unexpected failure {other:?}: {error}"),
                },
            }
        }

        // Independent effect count: exactly one effect per distinct applied
        // action id, across every restart, deadline and injected fault.
        let effects_observed = effects.load(Ordering::SeqCst);
        assert_eq!(
            effects_observed,
            applied_ids.len() as u64,
            "case {case}: one effect per distinct applied action id"
        );
        assert!(
            effects_observed <= terminal.len() as u64,
            "case {case}: no action id is applied twice"
        );
    }
}

/// Lost-receipt-acknowledgment property budget.
const LOST_ACK_CASE_BUDGET: u64 = 16;

#[test]
fn property_lost_receipt_ack_never_yields_a_second_effect_or_false_terminal() {
    let mut rng = Rng::new(PROPERTY_SEED ^ 0x10ac_10ac);
    for case in 0..LOST_ACK_CASE_BUDGET {
        let dir = ScratchDir::new(&format!("fc-prop-lost-ack-{case}"));
        let ledger = LedgerHandle::in_process(&dir).expect("bind ledger");
        let operation = if rng.below(2) == 0 { "pause" } else { "resume" };
        // 1 = the intent write loses its ack; 2 = the result write does.
        let ack_put = rng.below(2) + 1;
        if operation == "resume" {
            // Seed a stopped state without an API record so resume applies.
            let mut seed = ledger.clone();
            LedgerApi::set_kill_latch(&mut seed, true).expect("seed latch");
        }
        let owner = Arc::new(TestOwner::new(true));
        let authority = Arc::new(TestAuthority::approved(FAR_FUTURE_MS));
        let clock = Arc::new(AtomicU64::new(START_MS));
        let stop: Arc<dyn StopControl + Send + Sync> =
            Arc::new(BoundStopControl::new(dir.path(), ledger.clone()));
        let fault = FaultLedger::new(ledger.clone());
        fault.lose_ack_number.store(ack_put, Ordering::SeqCst);
        let service = service_over_parts(
            dir.path(),
            Box::new(fault),
            stop,
            Arc::clone(&owner),
            Arc::clone(&authority),
            Arc::clone(&clock),
        );
        let (mut router, _read, control_grant) = router_with(service);
        let action_id = format!("op-la-{case}");
        let confirmation = if operation == "resume" {
            Some("resume")
        } else {
            None
        };

        let result = invoke(
            &mut router,
            &control_grant,
            FACTORY_CONTROL_CAPABILITY,
            &format!("ctrl-la-{case}"),
            action_input(operation, &action_id, confirmation),
            0,
        );
        if ack_put == 1 {
            result.expect_err("a lost intent ack is never terminal success");
        } else {
            let value = result.expect("a lost result ack is reported as uncertainty");
            assert!(
                matches!(
                    response_from(value),
                    FactoryControlResponse::Unconfirmed(_)
                ),
                "case {case}: a lost result ack is never a terminal receipt"
            );
        }
        // Applied effects: 0 for a lost intent ack, 1 for a lost result ack.
        let expected_latched = if ack_put == 2 {
            operation == "pause"
        } else {
            operation == "resume"
        };
        assert_eq!(
            LedgerApi::kill_latch(&ledger).expect("latch"),
            expected_latched,
            "case {case}: effect state after a lost ack"
        );
        let durable = LedgerApi::control_receipts(&ledger).expect("receipts");
        assert_eq!(
            durable.len(),
            ack_put as usize,
            "case {case}: exactly the acknowledged-before-loss rows are durable"
        );

        // Close every handle and reopen through a fresh binding.
        drop(router);
        drop(ledger);
        let reopened =
            LedgerHandle::in_process_existing(dir.path()).expect("reopen the ledger");
        let reopened_receipts = LedgerApi::control_receipts(&reopened).expect("receipts");
        assert_eq!(
            reopened_receipts.len(),
            ack_put as usize,
            "case {case}: committed rows survive the reopen"
        );
        let effects = Arc::new(AtomicU64::new(0));
        let counting = CountingLedger {
            inner: reopened.clone(),
            effects: Arc::clone(&effects),
            fail_set: Arc::new(AtomicBool::new(false)),
        };
        let stop: Arc<dyn StopControl + Send + Sync> =
            Arc::new(BoundStopControl::new(dir.path(), reopened.clone()));
        let service = service_over_parts(
            dir.path(),
            Box::new(counting),
            stop,
            Arc::clone(&owner),
            Arc::clone(&authority),
            Arc::clone(&clock),
        );
        let (mut router, read_grant, control_grant) = router_with(service);

        // Lookup classification depends only on whether the result committed.
        let value = invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            &format!("lookup-la-{case}"),
            lookup_input(&action_id),
            0,
        )
        .expect("lookup after reopen");
        let expected_at = if ack_put == 1 {
            assert!(
                matches!(
                    response_from(value),
                    FactoryControlResponse::Unconfirmed(_)
                ),
                "case {case}: a committed intent without a result is unconfirmed"
            );
            None
        } else {
            let FactoryControlResponse::Receipt(receipt) = response_from(value) else {
                panic!("case {case}: a committed result replays as a receipt");
            };
            assert_eq!(receipt.receipt.outcome, FactoryControlOutcome::Applied);
            Some(receipt.receipt.recorded_at_unix_ms.as_str().to_string())
        };

        // Re-submitting the identical action never applies a second effect and
        // never promotes an orphan intent to a terminal receipt.
        let value = invoke(
            &mut router,
            &control_grant,
            FACTORY_CONTROL_CAPABILITY,
            &format!("ctrl-la-{case}-2"),
            action_input(operation, &action_id, confirmation),
            0,
        )
        .expect("replay after reopen");
        match (&expected_at, response_from(value)) {
            (Some(expected), FactoryControlResponse::Receipt(receipt)) => {
                assert_eq!(receipt.receipt.recorded_at_unix_ms.as_str(), expected);
            }
            (None, FactoryControlResponse::Unconfirmed(_)) => {}
            (_, other) => panic!("case {case}: unexpected replay {other:?}"),
        }
        assert_eq!(
            effects.load(Ordering::SeqCst),
            0,
            "case {case}: a replay after reopen applies no second effect"
        );
    }
}

#[test]
fn property_runtime_permissions_never_grant_authority_from_a_source_flag() {
    let mut rng = Rng::new(PROPERTY_SEED ^ 0x11);
    let fixture = Fixture::new("fc-prop-runtime");

    for case in 0..RUNTIME_CASE_BUDGET {
        let has_source = rng.below(4) != 0;
        let owner = rng.below(2) == 0;
        let authority = rng.below(2) == 0;
        let status = match rng.below(7) {
            0 => FactoryRuntimeStatus::Unavailable,
            1 => FactoryRuntimeStatus::Failed,
            2 => FactoryRuntimeStatus::Running,
            3 => FactoryRuntimeStatus::Paused,
            4 => FactoryRuntimeStatus::Starting,
            5 => FactoryRuntimeStatus::Recovering,
            _ => FactoryRuntimeStatus::Idle,
        };
        let source_state = match rng.below(5) {
            0 => SourceState::Available,
            1 => SourceState::Empty,
            2 => SourceState::Missing,
            3 => SourceState::Unreadable,
            _ => SourceState::Unknown,
        };
        let stopped = match rng.below(3) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };

        fixture.owner.set(owner);
        if authority {
            fixture.authority.set(Some(ApprovedControlBinding {
                plugin_id: PLUGIN.to_string(),
                capability: FACTORY_CONTROL_CAPABILITY.to_string(),
                generation: GENERATION,
                expires_at_unix_ms: FAR_FUTURE_MS,
            }));
        } else {
            fixture.authority.set(None);
        }

        let runtime_source = if has_source {
            let mut source = SourceRuntime::healthy();
            source.status = status;
            source.source_state = source_state;
            source.stopped = stopped;
            // A hostile source claims both permissions.
            source.raw_permissions = FactoryRuntimePermissions {
                pause: true,
                resume: true,
            };
            Some(Arc::new(source) as Arc<dyn RuntimeObservationSource>)
        } else {
            None
        };

        let (mut router, read_grant, _) = router_with(fixture.service(runtime_source));
        let value = invoke(
            &mut router,
            &read_grant,
            FACTORY_READ_CAPABILITY,
            &format!("read-prop-{case}"),
            snapshot_input(),
            0,
        )
        .expect("snapshot is served");
        let runtime = runtime_from(value);
        runtime.validate().expect("snapshot validates");

        // Structural invariants independent of the composition algorithm.
        assert!(
            !(runtime.permissions.pause && runtime.permissions.resume),
            "pause and resume are never both enabled"
        );
        if !owner || !authority {
            assert_eq!(
                runtime.permissions,
                FactoryRuntimePermissions::default(),
                "no authority means no permission (case {case})"
            );
        }
        if !has_source {
            assert_eq!(runtime.status, FactoryRuntimeStatus::Unavailable);
            assert!(runtime.runtime_instance_id.is_none());
            assert!(runtime.eligible.entries.is_empty());
            assert!(runtime.eligible.revision.is_none());
        }
    }
}
