//! HP.7 authorized plugin actions with durable intents, receipts and recovery
//! (`agalma-jmr.7`).
//!
//! Every case runs nonzero assertions against an independent fixture under the
//! persistent scratch root (never `/tmp`, no network, no paid provider call).
//! The durable intent journal is a **real** `SqliteLedger`; the executor's effect
//! journal is a **separate** durable file, so an in-memory transport replay cache
//! cannot masquerade as durable exactly-once reconciliation. Restart is simulated
//! by building a fresh `ActionService` over the same ledger file and a fresh fake
//! executor over the same effect-journal file.
//!
//! Coverage:
//!
//! * an allowed action executes exactly once across a lost acknowledgement and a
//!   restart; reconciliation completes it on its **pinned** generation and never
//!   re-runs the effect;
//! * denied grants (unknown/expired/wrong-generation/wrong-capability/unknown
//!   action) journal nothing and call no executor (zero effects);
//! * a missing human confirmation, and a policy actor on a
//!   confirmation-required action, trigger zero effects while a correct human
//!   confirmation succeeds;
//! * an explicit stop and an unknown (unavailable) stop state trigger zero
//!   effects;
//! * a forged report publication cannot execute or journal an action;
//! * a duplicate delivery replays the recorded outcome without re-invoking;
//! * a changed input under the same operation id is `IdentityConflict`;
//! * a stale (replacement) binding cannot redirect recovery to another
//!   generation;
//! * cancellation acknowledgement is not terminal; only settlement evidence is;
//! * the real UI action hookup routes a selected advertised action through the
//!   common validated service, including typed human confirmation and a
//!   denied-grant zero-effect path, and a restarted UI session dispatches a
//!   distinct durable operation;
//! * a deterministic seeded property test asserts the single-effect-per-input
//!   and monotonic-state invariants.
//!
//! Not covered here (owned by HP.8): production grant issuance and the
//! pinned-plugin RPC `ActionExecutor`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use agalma_conductor::composition::{InMemoryInstallations, PluginHost};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::control::PluginActionControl;
use agalma_conductor::components::{ActionView, PluginComponent, Transports};
use agalma_conductor::control::{ControlOutcome, ControlRequest};
use agalma_conductor::economics::EconomicsSnapshot;
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_actions::{
    action_namespace, verify_plugin_status, ActionCancelRequest, ActionCatalog, ActionExecutor,
    ActionRequest, ActionService, AdvertisedAction, ExecutionOutcome, PluginRpcActionExecutor,
    ReconcileOutcome,
};
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveredPackage, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_registry::InMemoryDesiredStateStore;
use agalma_conductor::plugin_reports::{seal_publication, ReportIngestor};
use agalma_conductor::plugin_router::{LaunchGrant, StopAdmission};
use agalma_conductor::plugin_supervisor::{Clock, SystemClock};
use agalma_conductor::status::Snapshot;
use agalma_conductor::stop::{StopControl, StopDecision};
use agalma_conductor::watch::{
    bind_action_driver, service_action_driver, service_action_driver_with_clock, ActionDriver,
    ActionInvocation, App, ControlDriver, ServiceActionDriver,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use agalma_contracts::digest::{canonical_digest, failure_code, sha256_digest, FailureCode};
use agalma_contracts::task_package::BindingPin;
use agalma_contracts::{
    ActionDescriptor, ActionIntent, ActionScope, Actor, ComponentEventRow, ComponentOperationState,
    ComponentReceipt, ContractError, Durability, LedgerApi, PluginInstallation, PluginManifest,
    PluginOperationStatus, ACTION_INTENT_FORMAT, PLUGIN_CANCEL_REQUEST_FORMAT,
    PLUGIN_OPERATION_STATUS_FORMAT,
};
use agalma_ipc::plugin_server::{
    plugin_server_info, register_plugin, PluginHandler, METHOD_INVOKE_CAPABILITY,
    METHOD_PUBLISH_REPORT,
};
use agalma_ipc::server::Server;
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

/// Recorded deterministic property-test seed (xorshift64).
const PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_00a7;
/// Recorded property-test case budget.
const PROPERTY_CASES: u64 = 48;
/// Start of the injected clock (milliseconds).
const START_MS: u64 = 1_791_820_800_000;
/// A far-future deadline/expiry for cases that do not exercise those bounds.
const FAR_FUTURE_MS: u64 = 10_000_000_000_000;

const PLUGIN: &str = "example.plugin";
const ACTION: &str = "example.refresh";
const CAPABILITY: &str = "example.control";
const REPORT_SCHEMA: &str = "example.plugin.status";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

/// A `Send + Sync` injected clock.
struct ManualClockFn {
    now: Arc<AtomicU64>,
}

impl ManualClockFn {
    fn new(start: u64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(start)),
        }
    }

    fn clock(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let now = Arc::clone(&self.now);
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    /// Advance the injected clock (simulates a later session/restart).
    fn advance(&self, by: u64) {
        self.now.fetch_add(by, Ordering::SeqCst);
    }
}

/// Deterministic xorshift64 so a property failure replays exactly.
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// An injected stop control with a fixed three-way decision.
struct FakeStop(StopDecision);

impl StopControl for FakeStop {
    fn stopped(&self) -> bool {
        !matches!(self.0, StopDecision::Running)
    }

    fn decision(&self) -> StopDecision {
        self.0.clone()
    }
}

fn running_stop() -> Arc<dyn StopAdmission> {
    Arc::new(FakeStop(StopDecision::Running)) as Arc<dyn StopAdmission>
}

fn stopping(decision: StopDecision) -> Arc<dyn StopAdmission> {
    Arc::new(FakeStop(decision)) as Arc<dyn StopAdmission>
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

fn grant(plugin_id: &str, generation: u64, capability: &str, expiry: u64) -> LaunchGrant {
    LaunchGrant::for_capability(
        plugin_id,
        binding(plugin_id, generation),
        METHOD_INVOKE_CAPABILITY,
        capability,
        expiry,
        None,
    )
    .expect("OS randomness available")
}

fn advertised(
    plugin_id: &str,
    generation: u64,
    capability: &str,
    human_confirmation: Option<&str>,
) -> AdvertisedAction {
    AdvertisedAction {
        descriptor: ActionDescriptor {
            action_id: ACTION.to_string(),
            input_schema: "example.plugin.refresh.input/v1".to_string(),
            required_capability: capability.to_string(),
            effectful: true,
            human_confirmation: human_confirmation.map(str::to_string),
            default_deadline_ms: 60_000,
        },
        binding: binding(plugin_id, generation),
    }
}

fn scope() -> ActionScope {
    ActionScope {
        directive_id: "directive/v0".to_string(),
        directive_version: 1,
        execution_id: None,
    }
}

fn action_request(
    token_hex: String,
    plugin_id: &str,
    generation: u64,
    operation_id: &str,
    input: Value,
    actor: Actor,
    confirmation: Option<String>,
) -> ActionRequest {
    ActionRequest {
        grant: token_hex,
        plugin_id: plugin_id.to_string(),
        generation,
        action_id: ACTION.to_string(),
        operation_id: operation_id.to_string(),
        input,
        deadline_unix_ms: FAR_FUTURE_MS,
        actor,
        confirmation,
        scope: scope(),
    }
}

fn cancel_request(
    token_hex: String,
    plugin_id: &str,
    generation: u64,
    operation_id: &str,
    cancel_request_id: &str,
) -> ActionCancelRequest {
    ActionCancelRequest {
        grant: token_hex,
        plugin_id: plugin_id.to_string(),
        generation,
        operation_id: operation_id.to_string(),
        cancel_request_id: cancel_request_id.to_string(),
        deadline_unix_ms: FAR_FUTURE_MS,
        actor: Actor::Human,
        confirmation: None,
        scope: scope(),
    }
}

// --- fake executor with a separate durable effect journal -------------------

#[derive(Default)]
struct SharedCalls {
    invocations: Vec<(u64, String)>,
    reconciles: Vec<(u64, String)>,
    cancels: Vec<(u64, String)>,
}

#[derive(Clone)]
enum InvokePlan {
    Complete(Value),
    Refuse(String),
    Lost,
}

struct FakeExecutor {
    effect_journal: PathBuf,
    plans: BTreeMap<String, InvokePlan>,
    reconcile_plan: BTreeMap<String, ReconcileOutcome>,
    cancel_plan: BTreeMap<String, ReconcileOutcome>,
    unavailable_generations: BTreeSet<u64>,
    calls: Arc<Mutex<SharedCalls>>,
}

impl FakeExecutor {
    fn new(effect_journal: PathBuf, calls: Arc<Mutex<SharedCalls>>) -> Self {
        FakeExecutor {
            effect_journal,
            plans: BTreeMap::new(),
            reconcile_plan: BTreeMap::new(),
            cancel_plan: BTreeMap::new(),
            unavailable_generations: BTreeSet::new(),
            calls,
        }
    }

    fn plan(mut self, operation_id: &str, plan: InvokePlan) -> Self {
        self.plans.insert(operation_id.to_string(), plan);
        self
    }

    fn reconcile_as(mut self, operation_id: &str, outcome: ReconcileOutcome) -> Self {
        self.reconcile_plan.insert(operation_id.to_string(), outcome);
        self
    }

    fn unavailable_generation(mut self, generation: u64) -> Self {
        self.unavailable_generations.insert(generation);
        self
    }
}

impl ActionExecutor for FakeExecutor {
    fn invoke(&mut self, intent: &ActionIntent) -> ExecutionOutcome {
        self.calls
            .lock()
            .unwrap()
            .invocations
            .push((intent.binding.generation, intent.operation_id.clone()));
        match self.plans.get(&intent.operation_id) {
            // A refusal happens before any external effect: do not journal one.
            Some(InvokePlan::Refuse(message)) => ExecutionOutcome::Refused(
                FailureCode::InvalidRecord.failure(&intent.operation_id, message.clone()),
            ),
            Some(InvokePlan::Complete(value)) => {
                append_effect(
                    &self.effect_journal,
                    &intent.operation_id,
                    &intent.input_digest,
                    intent.binding.generation,
                );
                ExecutionOutcome::Completed(value.clone())
            }
            // The effect happens and is recorded durably; the acknowledgement is
            // still lost, so a durable record is not an acknowledgement.
            Some(InvokePlan::Lost) | None => {
                append_effect(
                    &self.effect_journal,
                    &intent.operation_id,
                    &intent.input_digest,
                    intent.binding.generation,
                );
                if matches!(self.plans.get(&intent.operation_id), Some(InvokePlan::Lost)) {
                    ExecutionOutcome::Lost
                } else {
                    ExecutionOutcome::Completed(json!({"ok": true}))
                }
            }
        }
    }

    fn reconcile(&mut self, intent: &ActionIntent) -> ReconcileOutcome {
        self.calls
            .lock()
            .unwrap()
            .reconciles
            .push((intent.binding.generation, intent.operation_id.clone()));
        if self.unavailable_generations.contains(&intent.binding.generation) {
            return ReconcileOutcome::Unknown;
        }
        if let Some(outcome) = self.reconcile_plan.get(&intent.operation_id) {
            return outcome.clone();
        }
        if effect_count_for_input(&self.effect_journal, &intent.operation_id, &intent.input_digest)
            > 0
        {
            ReconcileOutcome::Completed(json!({"reconciled": true}))
        } else {
            ReconcileOutcome::Unknown
        }
    }

    fn cancel(&mut self, intent: &ActionIntent, _cancel: &agalma_contracts::PluginCancelRequest) -> ReconcileOutcome {
        self.calls
            .lock()
            .unwrap()
            .cancels
            .push((intent.binding.generation, intent.operation_id.clone()));
        self.cancel_plan
            .get(&intent.operation_id)
            .cloned()
            .unwrap_or(ReconcileOutcome::CancelRequested)
    }
}

fn append_effect(path: &Path, operation_id: &str, input_digest: &str, generation: u64) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open effect journal");
    writeln!(file, "{operation_id}\t{input_digest}\t{generation}").expect("append effect");
    file.flush().expect("flush effect journal");
}

fn read_effects(path: &Path) -> Vec<(String, String, u64)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let op = parts.next()?.to_string();
            let digest = parts.next()?.to_string();
            let generation = parts.next()?.parse().ok()?;
            Some((op, digest, generation))
        })
        .collect()
}

fn effect_count_for(path: &Path, operation_id: &str) -> usize {
    read_effects(path)
        .into_iter()
        .filter(|(op, _, _)| op == operation_id)
        .count()
}

fn effect_count_for_input(path: &Path, operation_id: &str, input_digest: &str) -> usize {
    read_effects(path)
        .into_iter()
        .filter(|(op, digest, _)| op == operation_id && digest == input_digest)
        .count()
}

// --- ledger/service helpers -------------------------------------------------

fn ledger_path(dir: &Path) -> PathBuf {
    dir.join("ledger.sqlite")
}

fn action_events(dir: &Path, plugin_id: &str) -> Vec<ComponentEventRow> {
    let ledger = SqliteLedger::open(ledger_path(dir)).expect("open ledger to inspect events");
    ledger
        .component_events(&action_namespace(plugin_id), 0, 500)
        .expect("read action events")
}

fn new_service(
    dir: &Path,
    catalog: Vec<AdvertisedAction>,
    executor: FakeExecutor,
    stop: Arc<dyn StopAdmission>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> ActionService {
    let sqlite = SqliteLedger::open(ledger_path(dir)).expect("open ledger");
    ActionService::new(
        Box::new(catalog) as Box<dyn ActionCatalog>,
        Box::new(Mutex::new(sqlite)) as Box<dyn agalma_conductor::plugin_actions::ActionStore>,
        Box::new(executor) as Box<dyn ActionExecutor>,
        stop,
    )
    .with_clock(clock)
}

fn failure(err: &ContractError) -> FailureCode {
    failure_code(err).expect("structured failure detail")
}

fn is_terminal(state: ComponentOperationState) -> bool {
    matches!(
        state,
        ComponentOperationState::Completed
            | ComponentOperationState::Failed
            | ComponentOperationState::Cancelled
    )
}

// ---------------------------------------------------------------------------
// 1 + 9. Allowed action executes once across loss/restart on the pinned gen
// ---------------------------------------------------------------------------

#[test]
fn allowed_action_executes_once_across_loss_and_restart() {
    let dir = fresh("hp7-loss-restart");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");

    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared)).plan("op-1", InvokePlan::Lost);
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);

    let request = action_request(
        g.token_hex(),
        PLUGIN,
        1,
        "op-1",
        json!({"n": 1}),
        Actor::Human,
        None,
    );
    let outcome = service.submit(&request).expect("allowed action executes");
    assert!(outcome.is_unknown(), "a lost acknowledgement stays unknown");
    assert!(!outcome.is_terminal());
    assert_eq!(
        shared.lock().unwrap().invocations,
        vec![(1u64, "op-1".to_string())],
        "the effect ran exactly once"
    );
    assert_eq!(effect_count_for(&journal, "op-1"), 1);
    drop(service);

    // Restart: a new service over the same ledger and a new executor over the
    // same durable effect journal.
    let shared2 = Arc::new(Mutex::new(SharedCalls::default()));
    let exec2 = FakeExecutor::new(journal.clone(), Arc::clone(&shared2));
    let g2 = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service2 = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec2,
        running_stop(),
        clock.clock(),
    );
    service2.install_grant(&g2);

    let reconcile = action_request(
        g2.token_hex(),
        PLUGIN,
        1,
        "op-1",
        json!({"n": 1}),
        Actor::Human,
        None,
    );
    let reconciled = service2.reconcile(&reconcile).expect("reconcile completes");
    assert_eq!(reconciled.state, ComponentOperationState::Completed);
    assert!(reconciled.receipt.is_some());
    assert_eq!(
        shared2.lock().unwrap().reconciles,
        vec![(1u64, "op-1".to_string())],
        "reconciliation targets the pinned generation 1"
    );
    assert!(
        shared2.lock().unwrap().invocations.is_empty(),
        "reconciliation never re-runs the effect"
    );
    assert_eq!(effect_count_for(&journal, "op-1"), 1, "still exactly one effect");
}

// ---------------------------------------------------------------------------
// 2. Denied actions have zero journal entries and zero effects
// ---------------------------------------------------------------------------

#[test]
fn denied_actions_have_zero_journal_and_zero_effects() {
    let dir = fresh("hp7-denied");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let good = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    let installed = service.install_grant(&good);
    assert!(!installed.is_empty());

    // (a) Unknown grant token (never installed).
    let unknown = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let err = service
        .submit(&action_request(
            unknown.token_hex(),
            PLUGIN,
            1,
            "op-a",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);

    // (b) Expired grant.
    let expired = grant(PLUGIN, 1, CAPABILITY, START_MS); // expiry == now
    service.install_grant(&expired);
    let err = service
        .submit(&action_request(
            expired.token_hex(),
            PLUGIN,
            1,
            "op-b",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);

    // (c) Wrong (replacement) generation.
    let err = service
        .submit(&action_request(
            good.token_hex(),
            PLUGIN,
            2,
            "op-c",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::StaleGeneration);

    // (d) Grant does not carry the required capability.
    let wrong_cap = grant(PLUGIN, 1, "other.capability", FAR_FUTURE_MS);
    service.install_grant(&wrong_cap);
    let err = service
        .submit(&action_request(
            wrong_cap.token_hex(),
            PLUGIN,
            1,
            "op-d",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::MissingCapability);

    // (e) Unknown action id advertised by no generation.
    let err = service
        .submit(&ActionRequest {
            action_id: "example.not-advertised".to_string(),
            ..action_request(
                good.token_hex(),
                PLUGIN,
                1,
                "op-e",
                json!({"n": 1}),
                Actor::Human,
                None,
            )
        })
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::InvalidRecord);

    assert!(shared.lock().unwrap().invocations.is_empty(), "no effect ran");
    assert!(
        action_events(&dir, PLUGIN).is_empty(),
        "a refused action journals nothing"
    );
}

// ---------------------------------------------------------------------------
// 2b. A plugin refusal records a failed state with no effect
// ---------------------------------------------------------------------------

#[test]
fn refused_action_records_failed_state_and_performs_no_effect() {
    let dir = fresh("hp7-refused");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared))
        .plan("op-refuse", InvokePlan::Refuse("input rejected".to_string()));
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);

    let err = service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-refuse",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::InvalidRecord);
    assert_eq!(shared.lock().unwrap().invocations.len(), 1);
    assert_eq!(
        effect_count_for(&journal, "op-refuse"),
        0,
        "a refusal performs no effect"
    );
    let record = service
        .record(PLUGIN, 1, "op-refuse")
        .unwrap()
        .expect("operation is journaled");
    assert_eq!(record.state, ComponentOperationState::Failed);
    assert!(record.receipt.is_none());
}

// ---------------------------------------------------------------------------
// 3. Human confirmation
// ---------------------------------------------------------------------------

#[test]
fn missing_or_policy_human_confirmation_has_zero_effects() {
    let dir = fresh("hp7-confirm");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, Some("confirm-refresh"))],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);

    // Missing confirmation.
    let err = service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-missing",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);

    // A policy actor cannot satisfy a human confirmation even with the phrase.
    let err = service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-policy",
            json!({"n": 1}),
            Actor::Policy,
            Some("confirm-refresh".to_string()),
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);

    assert!(shared.lock().unwrap().invocations.is_empty());
    assert!(action_events(&dir, PLUGIN).is_empty());

    // Positive control: the correct human confirmation succeeds.
    let ok = service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-human",
            json!({"n": 1}),
            Actor::Human,
            Some("confirm-refresh".to_string()),
        ))
        .expect("a confirmed human action executes");
    assert_eq!(ok.state, ComponentOperationState::Completed);
    assert!(!shared.lock().unwrap().invocations.is_empty());
}

// ---------------------------------------------------------------------------
// 4. Stop admission
// ---------------------------------------------------------------------------

#[test]
fn stopped_and_unknown_stop_state_have_zero_effects() {
    for (decision, expected) in [
        (
            StopDecision::Stopped("maintenance".to_string()),
            FailureCode::Stopped,
        ),
        (
            StopDecision::Unavailable("ledger unreadable".to_string()),
            FailureCode::StateUnavailable,
        ),
    ] {
        let dir = fresh("hp7-stop");
        let clock = ManualClockFn::new(START_MS);
        let journal = dir.join("effects.log");
        let shared = Arc::new(Mutex::new(SharedCalls::default()));
        let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
        let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
        let mut service = new_service(
            &dir,
            vec![advertised(PLUGIN, 1, CAPABILITY, None)],
            exec,
            stopping(decision),
            clock.clock(),
        );
        service.install_grant(&g);
        let err = service
            .submit(&action_request(
                g.token_hex(),
                PLUGIN,
                1,
                "op-stop",
                json!({"n": 1}),
                Actor::Human,
                None,
            ))
            .unwrap_err();
        assert_eq!(failure(&err), expected);
        assert!(shared.lock().unwrap().invocations.is_empty());
        assert!(action_events(&dir, PLUGIN).is_empty());
    }
}

// ---------------------------------------------------------------------------
// 5. A forged report cannot execute or journal an action
// ---------------------------------------------------------------------------

#[test]
fn forged_report_triggers_zero_effects() {
    let dir = fresh("hp7-forged-report");
    let clock = ManualClockFn::new(START_MS);

    // Establish the action service over the same ledger file.
    let action_journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(action_journal.clone(), Arc::clone(&shared));
    let good = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&good);

    // Publish a report that mimics an action request.
    {
        let sqlite = SqliteLedger::open(ledger_path(&dir)).unwrap();
        let generations = [(PLUGIN.to_string(), 1u64)].into_iter().collect();
        let schemas = [(REPORT_SCHEMA.to_string(), 1u64)].into_iter().collect();
        let mut ingestor: ReportIngestor = ReportIngestor::open(
            Box::new(Mutex::new(sqlite)),
            clock.clock(),
            generations,
            schemas,
        )
        .unwrap();
        let publication = seal_publication(
            PLUGIN,
            binding(PLUGIN, 1),
            REPORT_SCHEMA,
            1,
            1,
            START_MS,
            Durability::Durable,
            json!({"action_id": ACTION, "operation_id": "forged", "input": {"n": 1}}),
        )
        .unwrap();
        ingestor
            .publish(PLUGIN, &binding(PLUGIN, 1), &publication)
            .expect("report publishes");
        // The report is stored in the report namespace, never the action one.
        let report_rows = ingestor
            .store()
            .component_events(
                &agalma_conductor::plugin_reports::report_namespace(PLUGIN),
                0,
                100,
            )
            .unwrap();
        assert_eq!(report_rows.len(), 1);
    }

    assert!(action_events(&dir, PLUGIN).is_empty(), "no action journaled");
    assert!(shared.lock().unwrap().invocations.is_empty(), "no action ran");

    // A report-scoped grant cannot invoke an action.
    let report_grant = LaunchGrant::for_capability(
        PLUGIN,
        binding(PLUGIN, 1),
        METHOD_PUBLISH_REPORT,
        "report.publish",
        FAR_FUTURE_MS,
        None,
    )
    .unwrap();
    service.install_grant(&report_grant);
    let err = service
        .submit(&action_request(
            report_grant.token_hex(),
            PLUGIN,
            1,
            "forged",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);
    assert!(action_events(&dir, PLUGIN).is_empty());
    assert!(shared.lock().unwrap().invocations.is_empty());
}

// ---------------------------------------------------------------------------
// 6. Duplicate replay returns the recorded outcome
// ---------------------------------------------------------------------------

#[test]
fn duplicate_replay_returns_recorded_outcome_without_reinvoking() {
    let dir = fresh("hp7-replay");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared))
        .plan("op-dup", InvokePlan::Complete(json!({"v": 7})));
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);

    let request = action_request(
        g.token_hex(),
        PLUGIN,
        1,
        "op-dup",
        json!({"n": 1}),
        Actor::Human,
        None,
    );
    let first = service.submit(&request).expect("fresh action completes");
    assert_eq!(first.state, ComponentOperationState::Completed);
    assert_eq!(effect_count_for(&journal, "op-dup"), 1);

    let replay = service.submit(&request).expect("replay returns recorded outcome");
    assert_eq!(replay.state, ComponentOperationState::Completed);
    assert_eq!(replay.receipt, first.receipt);
    assert_eq!(replay.intent, first.intent);
    assert_eq!(
        shared.lock().unwrap().invocations.len(),
        1,
        "the executor is invoked exactly once"
    );
    assert_eq!(effect_count_for(&journal, "op-dup"), 1);
}

// ---------------------------------------------------------------------------
// 7. Changed input under the same operation id conflicts
// ---------------------------------------------------------------------------

#[test]
fn changed_input_under_same_operation_id_conflicts() {
    let dir = fresh("hp7-changed-input");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);

    service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-change",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .expect("first submit completes");

    let err = service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-change",
            json!({"n": 2}),
            Actor::Human,
            None,
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::IdentityConflict);
    assert_eq!(
        shared.lock().unwrap().invocations.len(),
        1,
        "the executor is not re-invoked"
    );
    assert_eq!(effect_count_for(&journal, "op-change"), 1);
}

// ---------------------------------------------------------------------------
// 8. A stale binding cannot redirect recovery
// ---------------------------------------------------------------------------

#[test]
fn stale_binding_cannot_redirect_recovery() {
    let dir = fresh("hp7-stale-binding");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");

    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared)).plan("op-pin", InvokePlan::Lost);
    let g1 = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g1);
    let outcome = service
        .submit(&action_request(
            g1.token_hex(),
            PLUGIN,
            1,
            "op-pin",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .expect("journal operation pinned to generation 1");
    assert!(outcome.is_unknown());
    drop(service);

    // Generation 2 is now advertised and active; generation 1 is unavailable
    // but a draining generation-1 grant still authorizes reconciling its pinned
    // operation.
    let shared2 = Arc::new(Mutex::new(SharedCalls::default()));
    let exec2 = FakeExecutor::new(journal.clone(), Arc::clone(&shared2)).unavailable_generation(1);
    let g1b = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let g2 = grant(PLUGIN, 2, CAPABILITY, FAR_FUTURE_MS);
    let mut service2 = new_service(
        &dir,
        vec![
            advertised(PLUGIN, 1, CAPABILITY, None),
            advertised(PLUGIN, 2, CAPABILITY, None),
        ],
        exec2,
        running_stop(),
        clock.clock(),
    );
    service2.install_grant(&g1b);
    service2.install_grant(&g2);

    // Reconcile targeting generation 1: generation 1 is unavailable, so the
    // operation stays unknown and is never redispatched to generation 2.
    let reconcile1 = action_request(
        g1b.token_hex(),
        PLUGIN,
        1,
        "op-pin",
        json!({"n": 1}),
        Actor::Human,
        None,
    );
    let reconciled = service2.reconcile(&reconcile1).expect("reconcile parks");
    assert!(reconciled.is_unknown());
    assert_eq!(
        shared2.lock().unwrap().reconciles,
        vec![(1u64, "op-pin".to_string())],
        "recovery targets only the pinned generation 1"
    );

    // A generation-2 request for the same operation id cannot redirect it: the
    // journaled operation is pinned to generation 1.
    let reconcile2 = action_request(
        g2.token_hex(),
        PLUGIN,
        2,
        "op-pin",
        json!({"n": 1}),
        Actor::Human,
        None,
    );
    let err = service2.reconcile(&reconcile2).unwrap_err();
    assert_eq!(failure(&err), FailureCode::StaleGeneration);
    assert_eq!(
        shared2.lock().unwrap().reconciles,
        vec![(1u64, "op-pin".to_string())],
        "generation 2 is never asked to reconcile a generation-1 operation"
    );
    assert_eq!(effect_count_for(&journal, "op-pin"), 1);
}

// ---------------------------------------------------------------------------
// recover() reconciles parked operations on their pinned generation
// ---------------------------------------------------------------------------

#[test]
fn recover_reconciles_parked_operations_on_pinned_generation() {
    let dir = fresh("hp7-recover");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");

    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared)).plan("op-r", InvokePlan::Lost);
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);
    service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-r",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .expect("parked operation");
    drop(service);

    let shared2 = Arc::new(Mutex::new(SharedCalls::default()));
    let exec2 = FakeExecutor::new(journal.clone(), Arc::clone(&shared2));
    let g2 = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service2 = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec2,
        running_stop(),
        clock.clock(),
    );
    service2.install_grant(&g2);
    let outcomes = service2.recover().expect("recover runs");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].state, ComponentOperationState::Completed);
    assert_eq!(
        shared2.lock().unwrap().reconciles,
        vec![(1u64, "op-r".to_string())]
    );
    assert_eq!(effect_count_for(&journal, "op-r"), 1);
}

// ---------------------------------------------------------------------------
// 10. Cancellation: acknowledgement is not terminal; settlement is
// ---------------------------------------------------------------------------

#[test]
fn cancel_ack_is_not_terminal_until_settlement() {
    let dir = fresh("hp7-cancel");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared))
        .plan("op-c", InvokePlan::Lost)
        .reconcile_as("op-c", ReconcileOutcome::Cancelled);
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);
    service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-c",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .expect("parked operation");

    let cancel = cancel_request(g.token_hex(), PLUGIN, 1, "op-c", "cancel-1");
    let ack = service.cancel(&cancel).expect("cancel acknowledged");
    assert_eq!(ack.state, ComponentOperationState::CancelRequested);
    assert!(
        !ack.is_terminal(),
        "a cancel acknowledgement alone is not terminal cleanup evidence"
    );
    assert!(ack.receipt.is_none());
    assert_eq!(shared.lock().unwrap().cancels.len(), 1);

    // Identical duplicate cancel is a no-op that returns the current state.
    let replay = service.cancel(&cancel).expect("duplicate cancel is idempotent");
    assert_eq!(replay.state, ComponentOperationState::CancelRequested);
    assert_eq!(
        shared.lock().unwrap().cancels.len(),
        1,
        "a duplicate cancel does not re-invoke the executor"
    );

    // Changed content under the same cancel id conflicts.
    let mut changed = cancel.clone();
    changed.deadline_unix_ms += 1;
    let err = service.cancel(&changed).unwrap_err();
    assert_eq!(failure(&err), FailureCode::IdentityConflict);

    // Settlement evidence (Cancelled) is required for a terminal settlement.
    let reconcile = action_request(
        g.token_hex(),
        PLUGIN,
        1,
        "op-c",
        json!({"n": 1}),
        Actor::Human,
        None,
    );
    let settled = service.reconcile(&reconcile).expect("settlement reconciles");
    assert_eq!(settled.state, ComponentOperationState::Cancelled);
    assert!(settled.is_terminal());
    assert!(settled.receipt.is_some());
    assert_eq!(effect_count_for(&journal, "op-c"), 1);
}

// ---------------------------------------------------------------------------
// 11. Control-plane seam: dispatch is human-only and forwards to the service
// ---------------------------------------------------------------------------

#[test]
fn control_plane_dispatch_is_human_only_and_forwards() {
    let dir = fresh("hp7-control-seam");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);

    // A policy actor is refused by the control-plane guard BEFORE the service is
    // consulted: zero effects and zero journal entries.
    {
        let mut control = PluginActionControl::new(&mut service);
        let err = control
            .dispatch(&action_request(
                g.token_hex(),
                PLUGIN,
                1,
                "op-policy",
                json!({"n": 1}),
                Actor::Policy,
                None,
            ))
            .unwrap_err();
        assert_eq!(failure(&err), FailureCode::AuthorityDenied);
    }
    assert!(
        shared.lock().unwrap().invocations.is_empty(),
        "a policy dispatch performs no effect"
    );
    assert!(
        action_events(&dir, PLUGIN).is_empty(),
        "a policy dispatch journals nothing"
    );

    // A human dispatch forwards to the validated service and completes exactly
    // once.
    let outcome = {
        let mut control = PluginActionControl::new(&mut service);
        control
            .dispatch(&action_request(
                g.token_hex(),
                PLUGIN,
                1,
                "op-human",
                json!({"n": 1}),
                Actor::Human,
                None,
            ))
            .expect("human dispatch completes")
    };
    assert_eq!(outcome.state, ComponentOperationState::Completed);
    assert_eq!(
        shared.lock().unwrap().invocations,
        vec![(1u64, "op-human".to_string())],
        "a human dispatch runs the effect exactly once"
    );
    assert_eq!(effect_count_for(&journal, "op-human"), 1);
    assert_eq!(
        action_events(&dir, PLUGIN).len(),
        2,
        "accepted + completed journaled by the service"
    );
}

// ---------------------------------------------------------------------------
// 12. Deterministic property test
// ---------------------------------------------------------------------------

#[test]
fn property_single_effect_and_monotonic_state_invariants() {
    let mut rng = XorShift64::new(PROPERTY_SEED);
    let mut total_effects = 0usize;
    for case in 0..PROPERTY_CASES {
        let dir = fresh(&format!("hp7-prop-{case}"));
        let clock = ManualClockFn::new(START_MS);
        let journal = dir.join("effects.log");
        let shared = Arc::new(Mutex::new(SharedCalls::default()));
        let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
        let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
        let mut service = new_service(
            &dir,
            vec![advertised(PLUGIN, 1, CAPABILITY, None)],
            exec,
            running_stop(),
            clock.clock(),
        );
        service.install_grant(&g);

        // Observed state and the chosen input per operation id.
        let mut state: BTreeMap<String, ComponentOperationState> = BTreeMap::new();
        let mut chosen_input: BTreeMap<String, Value> = BTreeMap::new();

        let steps = 1 + rng.below(24);
        for _ in 0..steps {
            let operation_id = format!("op-{}", rng.below(4));
            let input = match chosen_input.get(&operation_id) {
                Some(existing) if rng.below(2) == 0 => existing.clone(),
                _ => json!({"i": rng.below(3)}),
            };
            chosen_input.insert(operation_id.clone(), input.clone());
            let request = action_request(
                g.token_hex(),
                PLUGIN,
                1,
                &operation_id,
                input,
                Actor::Human,
                None,
            );
            let observed = match rng.below(5) {
                0 | 1 | 2 => match service.submit(&request) {
                    Ok(outcome) => Some(outcome.state),
                    Err(err) => {
                        // The only expected refusal with a valid grant/stop is a
                        // changed input under an existing operation id.
                        assert_eq!(
                            failure(&err),
                            FailureCode::IdentityConflict,
                            "case {case} op {operation_id}: only changed-input conflicts are expected"
                        );
                        state.get(&operation_id).copied()
                    }
                },
                3 => match service.reconcile(&request) {
                    Ok(outcome) => Some(outcome.state),
                    Err(err) => {
                        // An operation that was never submitted is not journaled.
                        assert_eq!(
                            failure(&err),
                            FailureCode::StateUnavailable,
                            "case {case} op {operation_id}: reconcile refusal"
                        );
                        None
                    }
                },
                _ => {
                    let cancel = cancel_request(
                        g.token_hex(),
                        PLUGIN,
                        1,
                        &operation_id,
                        &format!("c-{operation_id}"),
                    );
                    match service.cancel(&cancel) {
                        Ok(outcome) => Some(outcome.state),
                        Err(err) => {
                            assert_eq!(
                                failure(&err),
                                FailureCode::StateUnavailable,
                                "case {case} op {operation_id}: cancel refusal"
                            );
                            None
                        }
                    }
                }
            };

            let Some(observed) = observed else {
                continue;
            };
            if let Some(previous) = state.get(&operation_id) {
                if is_terminal(*previous) {
                    assert_eq!(
                        observed, *previous,
                        "case {case} op {operation_id}: a terminal state is never rewound"
                    );
                }
            }
            state.insert(operation_id, observed);
        }

        // Invariant: at most one durable effect per (operation_id, input_digest).
        let mut per_input: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (op, digest, _gen) in read_effects(&journal) {
            *per_input.entry((op, digest)).or_default() += 1;
        }
        for (key, count) in &per_input {
            assert!(
                *count <= 1,
                "case {case}: effect for {key:?} happened {count} times"
            );
        }
        total_effects += per_input.values().sum::<usize>();
    }
    assert!(total_effects > 0, "the property run exercised real effects");
}

// ---------------------------------------------------------------------------
// 14. prior_cancel matches the EXACT request id, never an id suffix
// ---------------------------------------------------------------------------

#[test]
fn prior_cancel_matches_exact_request_id_not_suffix() {
    let dir = fresh("hp7-cancel-id-suffix");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared)).plan("op-alias", InvokePlan::Lost);
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    service.install_grant(&g);

    // 1. An allowed action that is accepted but non-terminal.
    let accepted = service
        .submit(&action_request(
            g.token_hex(),
            PLUGIN,
            1,
            "op-alias",
            json!({"n": 1}),
            Actor::Human,
            None,
        ))
        .expect("allowed action parks");
    assert!(accepted.is_unknown(), "the action is accepted, non-terminal");

    fn cancel_events(dir: &Path) -> Vec<ComponentEventRow> {
        action_events(dir, PLUGIN)
            .into_iter()
            .filter(|row| row.event.kind == "action.cancel_requested")
            .collect()
    }

    // 2. Cancel with request id "cancel:a" journals and calls the executor once.
    let first = cancel_request(g.token_hex(), PLUGIN, 1, "op-alias", "cancel:a");
    let ack1 = service.cancel(&first).expect("cancel request acknowledged");
    assert_eq!(ack1.state, ComponentOperationState::CancelRequested);
    assert_eq!(
        shared.lock().unwrap().cancels.len(),
        1,
        "the first cancel reaches the executor"
    );
    let after_first = cancel_events(&dir);
    assert_eq!(after_first.len(), 1);
    assert_eq!(
        after_first[0].event.event_id,
        "1:op-alias:cancel_requested:cancel:a"
    );

    // 3. "a" is a suffix of "cancel:a" but a DISTINCT request id: it must journal
    // its own cancel_requested event and call the executor a second time. Under
    // the suffix bug this request was silently swallowed.
    let second = cancel_request(g.token_hex(), PLUGIN, 1, "op-alias", "a");
    let ack2 = service.cancel(&second).expect("distinct cancel acknowledged");
    assert_eq!(ack2.state, ComponentOperationState::CancelRequested);
    let after_second = cancel_events(&dir);
    assert_eq!(
        after_second.len(),
        2,
        "a distinct request id journals a second cancel_requested event"
    );
    assert!(
        after_second
            .iter()
            .any(|row| row.event.event_id == "1:op-alias:cancel_requested:a"),
        "request id \"a\" is journaled under its own exact event id"
    );
    assert_eq!(
        shared.lock().unwrap().cancels.len(),
        2,
        "a distinct request id reaches the executor a second time"
    );

    // 4. An exact replay of "cancel:a" returns the current state without a third
    // executor call and without journaling anything.
    let replay = service.cancel(&first).expect("exact replay is idempotent");
    assert_eq!(replay.state, ComponentOperationState::CancelRequested);
    assert_eq!(
        shared.lock().unwrap().cancels.len(),
        2,
        "an exact replay never re-invokes the executor"
    );
    assert_eq!(
        cancel_events(&dir).len(),
        2,
        "an exact replay journals no new cancel_requested event"
    );
}

// ---------------------------------------------------------------------------
// 15. UI action path routes through the common validated service
// ---------------------------------------------------------------------------

/// A no-op control driver: the `a` binding never touches it.
struct NoopControlDriver;

impl ControlDriver for NoopControlDriver {
    fn apply(&mut self, _request: &ControlRequest) -> Result<ControlOutcome, ContractError> {
        Ok(ControlOutcome::Applied)
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A UI snapshot with one plugin component advertising one action.
fn ui_snapshot(plugin_id: &str, generation: u64, confirmation: Option<&str>) -> Snapshot {
    Snapshot {
        state_dir: "/evidence/hp7-ui".to_string(),
        exists: true,
        kill_latch: false,
        schema_version: Some(4),
        binding: None,
        executions: Vec::new(),
        components: Vec::new(),
        plugins: vec![PluginComponent {
            plugin_id: plugin_id.to_string(),
            origin: "discovered".to_string(),
            desired: "running".to_string(),
            lifecycle: "ready".to_string(),
            generation: Some(generation),
            freshness: "fresh".to_string(),
            stale: false,
            disconnected: false,
            incomplete: false,
            last_error: None,
            health: None,
            actions: vec![ActionView {
                action_id: ACTION.to_string(),
                required_capability: CAPABILITY.to_string(),
                effectful: true,
                human_confirmation: confirmation.map(str::to_string),
            }],
            views: Vec::new(),
        }],
        economics: EconomicsSnapshot::default(),
    }
}

#[test]
fn ui_action_path_dispatches_through_common_validated_service() {
    let dir = fresh("hp7-ui-action");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    // Host wiring: installing the grant registers it hub-side (so the common
    // service authenticates the presented token) and supplies the presented
    // token. The UI never mints action authority of its own.
    let mut driver = ServiceActionDriver::new(service)
        .with_clock(clock.clock())
        .with_deadline_ms(60_000);
    driver.install_grant(&g);

    let mut app = App::new(
        ui_snapshot(PLUGIN, 1, None),
        Transports::all_in_process(),
        1000,
    );
    let mut control = NoopControlDriver;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut driver);

    let operation_id = format!("ui:{ACTION}:{START_MS}:1");
    assert_eq!(
        shared.lock().unwrap().invocations,
        vec![(1u64, operation_id.clone())],
        "the UI dispatch runs the pinned generation's effect exactly once"
    );
    assert_eq!(effect_count_for(&journal, &operation_id), 1);
    assert_eq!(
        action_events(&dir, PLUGIN).len(),
        2,
        "accepted + completed journaled through the common service"
    );
    assert!(app.refresh_requested, "a dispatched action refreshes the view");
    let status = app.status.clone().unwrap_or_default();
    assert!(
        status.contains(ACTION) && status.contains("Completed"),
        "status reflects the terminal outcome: {status}"
    );
}

#[test]
fn ui_confirmation_action_requires_the_typed_phrase() {
    let dir = fresh("hp7-ui-confirm");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, Some("confirm-refresh"))],
        exec,
        running_stop(),
        clock.clock(),
    );
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut driver = ServiceActionDriver::new(service)
        .with_clock(clock.clock())
        .with_deadline_ms(60_000);
    driver.install_grant(&g);

    let mut app = App::new(
        ui_snapshot(PLUGIN, 1, Some("confirm-refresh")),
        Transports::all_in_process(),
        1000,
    );
    let mut control = NoopControlDriver;

    // 'a' arms the typed-confirmation prompt; nothing dispatches yet.
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut driver);
    assert!(app.pending_action.is_some(), "confirmation prompt armed");
    assert!(
        shared.lock().unwrap().invocations.is_empty(),
        "no effect before the phrase is typed"
    );

    // A wrong phrase plus Enter dispatches nothing.
    for c in "nope".chars() {
        app.handle_key_with(key(KeyCode::Char(c)), &mut control, &mut driver);
    }
    app.handle_key_with(key(KeyCode::Enter), &mut control, &mut driver);
    assert!(shared.lock().unwrap().invocations.is_empty());
    assert!(app.pending_action.is_some());

    // The exact phrase dispatches once with the typed confirmation.
    for _ in 0.."nope".len() {
        app.handle_key_with(key(KeyCode::Backspace), &mut control, &mut driver);
    }
    for c in "confirm-refresh".chars() {
        app.handle_key_with(key(KeyCode::Char(c)), &mut control, &mut driver);
    }
    app.handle_key_with(key(KeyCode::Enter), &mut control, &mut driver);

    let operation_id = format!("ui:{ACTION}:{START_MS}:1");
    assert_eq!(
        shared.lock().unwrap().invocations,
        vec![(1u64, operation_id.clone())],
        "the confirmed action runs once"
    );
    assert_eq!(effect_count_for(&journal, &operation_id), 1);
    assert!(app.pending_action.is_none());
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("Completed"), "{status}");
}

#[test]
fn ui_action_without_installed_grant_has_zero_effects() {
    let dir = fresh("hp7-ui-denied");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    // No grant is installed for the plugin: the UI adds no authority.
    let mut driver = ServiceActionDriver::new(service)
        .with_clock(clock.clock())
        .with_deadline_ms(60_000);

    let mut app = App::new(
        ui_snapshot(PLUGIN, 1, None),
        Transports::all_in_process(),
        1000,
    );
    let mut control = NoopControlDriver;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut driver);

    assert!(shared.lock().unwrap().invocations.is_empty(), "no effect ran");
    assert!(
        action_events(&dir, PLUGIN).is_empty(),
        "a denied UI action journals nothing"
    );
    assert!(!app.refresh_requested, "a refused action does not refresh");
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("error"), "the refusal is surfaced: {status}");
}

// ---------------------------------------------------------------------------
// 19. Production PluginHost action-service assembly (registry-derived catalog,
//     selected-ledger durable store, per-launch grant, pinned-generation executor)
// ---------------------------------------------------------------------------

const HOST_PLUGIN: &str = "example.hostplugin";
const HOST_ACTION: &str = "example.host.refresh";
const HOST_CAPABILITY: &str = "example.host.control";
const HOST_SCHEMA: &str = "example.hostplugin.status";

const HOST_CONFIG_SCHEMA: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
const HOST_DEFAULT_CONFIG: &[u8] = b"{}\n";
const HOST_REPORT_SCHEMA: &[u8] = b"{\"type\":\"object\",\"required\":[\"ready\"]}\n";

fn host_byte_ref(path: &str, bytes: &[u8], media_type: &str) -> Value {
    json!({
        "path": path,
        "digest": sha256_digest(bytes),
        "bytes": bytes.len() as u64,
        "media_type": media_type,
    })
}

/// A manifest advertising exactly one action requiring [`HOST_CAPABILITY`].
fn host_manifest(plugin_id: &str) -> Value {
    json!({
        "format": "PluginManifest/v1",
        "plugin_id": plugin_id,
        "package_version": "1.2.3",
        "package_digest": format!("sha256:{}", "0".repeat(64)),
        "protocol_major": 1,
        "entrypoint": {"path": "bin/plugin", "argv": ["--serve"], "launch_profile": "host/v1"},
        "domain_apis": [],
        "requested_capabilities": [HOST_CAPABILITY],
        "required_services": [],
        "optional_services": [],
        "config_schema": host_byte_ref("objects/config-schema.json", HOST_CONFIG_SCHEMA, "application/json"),
        "default_configuration": host_byte_ref("objects/default-config.json", HOST_DEFAULT_CONFIG, "application/json"),
        "report_schemas": [{
            "namespace": HOST_SCHEMA,
            "version": 1,
            "schema": host_byte_ref("objects/report-schema.json", HOST_REPORT_SCHEMA, "application/json")
        }],
        "views": [{
            "view_id": "status",
            "schema_namespace": HOST_SCHEMA,
            "kind": "Status",
            "title": "Host status",
            "schema_version": 1,
            "fields": ["ready"]
        }],
        "actions": [{
            "action_id": HOST_ACTION,
            "input_schema": "example.host.refresh.input/v1",
            "required_capability": HOST_CAPABILITY,
            "effectful": true,
            "human_confirmation": null,
            "default_deadline_ms": 60_000
        }],
        "state_compatibility": {"reads": ["host-state/v1"], "writes": "host-state/v1"}
    })
}

fn host_seal_package(package_dir: &Path, manifest: &Value) -> PluginManifest {
    let files: Vec<(String, Vec<u8>)> = vec![
        (
            "objects/config-schema.json".to_string(),
            HOST_CONFIG_SCHEMA.to_vec(),
        ),
        (
            "objects/default-config.json".to_string(),
            HOST_DEFAULT_CONFIG.to_vec(),
        ),
        (
            "objects/report-schema.json".to_string(),
            HOST_REPORT_SCHEMA.to_vec(),
        ),
        ("bin/plugin".to_string(), b"#!/bin/sh\nexit 0\n".to_vec()),
    ];
    std::fs::create_dir_all(package_dir).unwrap();
    for (relative, bytes) in &files {
        let path = package_dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            package_dir.join("bin/plugin"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let parsed: PluginManifest = serde_json::from_value(manifest.clone()).unwrap();
    let digest = package_inventory_digest(package_dir, &parsed).unwrap();
    let mut sealed = manifest.clone();
    sealed["package_digest"] = Value::String(digest);
    std::fs::write(
        package_dir.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&sealed).unwrap(),
    )
    .unwrap();
    serde_json::from_value(sealed).unwrap()
}

fn host_installation(package: &DiscoveredPackage) -> PluginInstallation {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: package.plugin_id.clone(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: package.manifest.requested_capabilities.clone(),
        installation_digest: String::new(),
    };
    installation.installation_digest = installation.recomputed_installation_digest().unwrap();
    installation
}

fn host_fixture(root: &Path) -> DiscoveredPackage {
    let package_dir = root.join("pkg");
    host_seal_package(&package_dir, &host_manifest(HOST_PLUGIN));
    let report = discover_plugins(&[PluginRoot::required(root)]).unwrap();
    report.package(HOST_PLUGIN).unwrap().clone()
}

fn host_config(state_dir: &Path, root: &Path) -> PluginHostConfig {
    PluginHostConfig {
        state_dir: state_dir.to_path_buf(),
        plugin_roots: vec![PluginRoot::required(root)],
        once: true,
    }
}

/// Assemble a host over the selected ledger, activate the installed generation
/// and issue its per-launch grant (no child is spawned).
fn assemble_granted_host(state_dir: &Path, root: &Path, ledger: &LedgerHandle) -> PluginHost {
    let package = host_fixture(root);
    let mut installations = InMemoryInstallations::new();
    installations.insert(host_installation(&package));
    let config = host_config(state_dir, root);
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| START_MS);
    let mut host =
        PluginHost::assemble_with_ledger(&config, &installations, ledger.clone(), clock).unwrap();
    host.registry.activate(HOST_PLUGIN, 1).unwrap();
    host.launch_request(HOST_PLUGIN).unwrap();
    host
}

fn host_action_request(
    token_hex: String,
    generation: u64,
    operation_id: &str,
    input: Value,
) -> ActionRequest {
    ActionRequest {
        grant: token_hex,
        plugin_id: HOST_PLUGIN.to_string(),
        generation,
        action_id: HOST_ACTION.to_string(),
        operation_id: operation_id.to_string(),
        input,
        deadline_unix_ms: FAR_FUTURE_MS,
        actor: Actor::Human,
        confirmation: None,
        scope: scope(),
    }
}

/// A UI snapshot advertising the host fixture's action.
fn host_ui_snapshot() -> Snapshot {
    Snapshot {
        state_dir: "/evidence/hp7-host".to_string(),
        exists: true,
        kill_latch: false,
        schema_version: Some(4),
        binding: None,
        executions: Vec::new(),
        components: Vec::new(),
        plugins: vec![PluginComponent {
            plugin_id: HOST_PLUGIN.to_string(),
            origin: "discovered".to_string(),
            desired: "running".to_string(),
            lifecycle: "ready".to_string(),
            generation: Some(1),
            freshness: "fresh".to_string(),
            stale: false,
            disconnected: false,
            incomplete: false,
            last_error: None,
            health: None,
            actions: vec![ActionView {
                action_id: HOST_ACTION.to_string(),
                required_capability: HOST_CAPABILITY.to_string(),
                effectful: true,
                human_confirmation: None,
            }],
            views: Vec::new(),
        }],
        economics: EconomicsSnapshot::default(),
    }
}

fn host_action_events(state_dir: &Path) -> Vec<ComponentEventRow> {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).unwrap();
    ledger
        .component_events(&action_namespace(HOST_PLUGIN), 0, 500)
        .unwrap()
}

/// A host with no selected ledger exposes no action service: the driver is
/// absent and the interactive path stays fail-closed.
#[test]
fn host_action_driver_fails_closed_without_selected_ledger() {
    let scratch = fresh("hp7-host-no-ledger");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let package = host_fixture(&root);
    let mut installations = InMemoryInstallations::new();
    installations.insert(host_installation(&package));
    let config = host_config(&state_dir, &root);
    let clock: Rc<dyn Clock> = Rc::new(SystemClock);
    let host = PluginHost::assemble(
        &config,
        &installations,
        Box::new(InMemoryDesiredStateStore::new()),
        clock,
    )
    .unwrap();

    assert!(
        host.action_driver().unwrap().is_none(),
        "no selected ledger => no durable action service"
    );

    // The interactive path binds the fail-closed default and refuses.
    let mut driver = bind_action_driver(None);
    let mut app = App::new(host_ui_snapshot(), Transports::all_in_process(), 1000);
    let mut control = NoopControlDriver;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut *driver);
    assert!(!app.refresh_requested);
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("error"), "{status}");
}

/// The host-derived service resolves the advertised action from the active
/// pinned registry generation, accepts the per-launch grant, journals the intent
/// in the selected ledger and invokes the pinned-generation child RPC (which
/// refuses because no child is running).
#[test]
fn host_action_service_uses_registry_catalog_and_selected_ledger() {
    let scratch = fresh("hp7-host-journal");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    let host = assemble_granted_host(&state_dir, &root, &ledger);
    let token = host.launch_grant(HOST_PLUGIN).unwrap().token_hex();

    let mut service = host.action_service_with_ledger(&ledger).unwrap();
    let err = service
        .submit(&host_action_request(
            token,
            1,
            "op-host-1",
            json!({"n": 1}),
        ))
        .unwrap_err();
    // The pinned child is not running, so the executor refuses; the intent is
    // still durably journaled (accepted + failed) in the selected ledger.
    assert_eq!(failure(&err), FailureCode::StateUnavailable);

    let events = host_action_events(&state_dir);
    assert_eq!(events.len(), 2, "accepted + failed: {events:?}");
    assert!(
        events
            .iter()
            .any(|row| row.event.event_id == "1:op-host-1:accepted"),
        "intent journaled under the pinned generation: {events:?}"
    );

    // A request for a non-active generation cannot be redirected onto it.
    let err = service
        .submit(&host_action_request(
            host.launch_grant(HOST_PLUGIN).unwrap().token_hex(),
            2,
            "op-host-2",
            json!({"n": 1}),
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::StaleGeneration);
    assert_eq!(host_action_events(&state_dir).len(), 2, "no new journal");
}

/// An action that no active generation advertises is refused, and a missing
/// grant is refused, both with zero journal entries.
#[test]
fn host_action_service_denies_unknown_action_and_missing_grant() {
    let scratch = fresh("hp7-host-denied");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    let host = assemble_granted_host(&state_dir, &root, &ledger);
    let token = host.launch_grant(HOST_PLUGIN).unwrap().token_hex();
    let mut service = host.action_service_with_ledger(&ledger).unwrap();

    // Unknown action id: not advertised by the active generation.
    let err = service
        .submit(&ActionRequest {
            action_id: "example.host.not-advertised".to_string(),
            ..host_action_request(token.clone(), 1, "op-unknown", json!({}))
        })
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::InvalidRecord);

    // A grant that was never installed for this host.
    let stranger = grant(HOST_PLUGIN, 1, HOST_CAPABILITY, FAR_FUTURE_MS);
    let err = service
        .submit(&host_action_request(
            stranger.token_hex(),
            1,
            "op-no-grant",
            json!({}),
        ))
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);

    assert!(
        host_action_events(&state_dir).is_empty(),
        "refusals journal nothing"
    );
}

/// A journaled operation is durable across a host/service restart: a repeated
/// delivery of the same operation id + input replays the recorded outcome
/// without a new effect or journal entry.
#[test]
fn host_action_service_replays_durable_operation_across_restart() {
    let scratch = fresh("hp7-host-durable");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    let input = json!({"n": 7});
    {
        let host = assemble_granted_host(&state_dir, &root, &ledger);
        let token = host.launch_grant(HOST_PLUGIN).unwrap().token_hex();
        let mut service = host.action_service_with_ledger(&ledger).unwrap();
        let err = service
            .submit(&host_action_request(token, 1, "op-durable", input.clone()))
            .unwrap_err();
        assert_eq!(failure(&err), FailureCode::StateUnavailable);
    }
    let after_first = host_action_events(&state_dir).len();
    assert_eq!(after_first, 2, "accepted + failed");

    // Restart: a fresh host + service over the same selected ledger.
    let host = assemble_granted_host(&state_dir, &root, &ledger);
    let token = host.launch_grant(HOST_PLUGIN).unwrap().token_hex();
    let mut service = host.action_service_with_ledger(&ledger).unwrap();
    let replay = service
        .submit(&host_action_request(token, 1, "op-durable", input))
        .expect("replay returns the recorded outcome");
    assert_eq!(replay.state, ComponentOperationState::Failed);
    assert_eq!(
        host_action_events(&state_dir).len(),
        after_first,
        "a durable replay journals no new events"
    );
}

/// The host-derived driver is the production `ServiceActionDriver` and reaches
/// the common service: a selected action is journaled durably (no service path
/// invents authority).
#[test]
fn host_action_driver_reaches_the_common_service() {
    let scratch = fresh("hp7-host-driver");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    let host = assemble_granted_host(&state_dir, &root, &ledger);

    let driver = host.action_driver_with_ledger(&ledger).unwrap();
    let mut driver = bind_action_driver(Some(driver));
    let mut app = App::new(host_ui_snapshot(), Transports::all_in_process(), 1000);
    let mut control = NoopControlDriver;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut *driver);

    // The intent reached the common service's durable journal under the pinned
    // generation (the pinned child is not running, so the outcome is a refusal).
    let events = host_action_events(&state_dir);
    assert_eq!(events.len(), 2, "accepted + failed journaled: {events:?}");
    assert!(events
        .iter()
        .any(|row| row.event.event_id == "1:ui:example.host.refresh:1:accepted"
            || row.event.event_id.starts_with("1:ui:")));
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("error"), "{status}");
}

// ---------------------------------------------------------------------------
// 16. A UI invocation after a restart is a distinct durable operation
// ---------------------------------------------------------------------------

#[test]
fn ui_restart_invokes_a_distinct_operation() {
    let dir = fresh("hp7-ui-restart");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");

    // Session 1: dispatch one action.
    let shared1 = Arc::new(Mutex::new(SharedCalls::default()));
    let exec1 = FakeExecutor::new(journal.clone(), Arc::clone(&shared1));
    let service1 = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec1,
        running_stop(),
        clock.clock(),
    );
    let g1 = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut driver1 = ServiceActionDriver::new(service1)
        .with_clock(clock.clock())
        .with_deadline_ms(60_000);
    driver1.install_grant(&g1);
    let mut app1 = App::new(
        ui_snapshot(PLUGIN, 1, None),
        Transports::all_in_process(),
        1000,
    );
    let mut control1 = NoopControlDriver;
    app1.handle_key_with(key(KeyCode::Char('a')), &mut control1, &mut driver1);

    let first_op = format!("ui:{ACTION}:{START_MS}:1");
    assert_eq!(
        shared1.lock().unwrap().invocations,
        vec![(1u64, first_op.clone())]
    );
    assert_eq!(effect_count_for(&journal, &first_op), 1);
    drop(driver1);

    // Restart at a later clock: a fresh service, executor and grant over the
    // same ledger and effect journal.
    clock.advance(5_000);
    let later = START_MS + 5_000;
    let shared2 = Arc::new(Mutex::new(SharedCalls::default()));
    let exec2 = FakeExecutor::new(journal.clone(), Arc::clone(&shared2));
    let service2 = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec2,
        running_stop(),
        clock.clock(),
    );
    let g2 = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut driver2 = ServiceActionDriver::new(service2)
        .with_clock(clock.clock())
        .with_deadline_ms(60_000);
    driver2.install_grant(&g2);
    let mut app2 = App::new(
        ui_snapshot(PLUGIN, 1, None),
        Transports::all_in_process(),
        1000,
    );
    let mut control2 = NoopControlDriver;
    app2.handle_key_with(key(KeyCode::Char('a')), &mut control2, &mut driver2);

    let second_op = format!("ui:{ACTION}:{later}:1");
    assert_ne!(
        first_op, second_op,
        "a restarted UI session yields a distinct operation id"
    );
    assert_eq!(
        shared2.lock().unwrap().invocations,
        vec![(1u64, second_op.clone())],
        "the restarted session runs a new effect rather than replaying the old one"
    );
    assert_eq!(effect_count_for(&journal, &second_op), 1);
    assert_eq!(
        read_effects(&journal).len(),
        2,
        "both sessions' effects are durable and distinct"
    );
    let status = app2.status.clone().unwrap_or_default();
    assert!(status.contains("Completed"), "{status}");
}

// ---------------------------------------------------------------------------
// 17. Deterministic property: each UI invocation is a distinct single effect
// ---------------------------------------------------------------------------

/// Deterministic property over UI invocations: every accepted dispatch produces
/// exactly one durable effect, and no two dispatches share an operation identity
/// (so a same-millisecond burst and a later session never collapse).
#[test]
fn property_ui_invocations_are_distinct_and_single_effect() {
    let dir = fresh("hp7-ui-prop");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);
    let mut driver = ServiceActionDriver::new(service)
        .with_clock(clock.clock())
        .with_deadline_ms(60_000);
    driver.install_grant(&g);

    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x5eed);
    let cases = 1 + rng.below(24);
    let mut dispatched = 0usize;
    for case in 0..cases {
        // Sometimes the clock ticks; sometimes two invocations land in the same
        // millisecond (the counter must still keep them distinct).
        if rng.below(2) == 0 {
            clock.advance(1 + rng.below(50));
        }
        let invocation = ActionInvocation {
            plugin_id: PLUGIN.to_string(),
            generation: 1,
            action_id: ACTION.to_string(),
            input: json!({"i": rng.below(4)}),
            confirmation: None,
        };
        let outcome = driver
            .dispatch(&invocation)
            .unwrap_or_else(|e| panic!("case {case}: UI dispatch completes: {e}"));
        assert_eq!(outcome.state, ComponentOperationState::Completed, "case {case}");
        dispatched += 1;
    }

    let effects = read_effects(&journal);
    assert_eq!(
        effects.len(),
        dispatched,
        "one durable effect per accepted UI dispatch"
    );
    let mut identities: BTreeSet<(String, String)> = BTreeSet::new();
    for (operation_id, digest, _generation) in &effects {
        assert!(
            identities.insert((operation_id.clone(), digest.clone())),
            "distinct operation identity per dispatch: {operation_id}"
        );
    }
    assert!(dispatched > 0, "the property run exercised real dispatches");
}

// ---------------------------------------------------------------------------
// 18. Production UI entry: the injected driver is bound and reaches the service
// ---------------------------------------------------------------------------

/// The production host builds the driver with [`service_action_driver`] (over the
/// common service + parent-issued grants); the interactive loop binds exactly
/// that driver via [`bind_action_driver`]. A selected action then reaches the
/// common validated service (one effect), and the view refreshes.
#[test]
fn production_entry_binds_injected_service_driver() {
    let dir = fresh("hp7-prod-entry");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    let g = grant(PLUGIN, 1, CAPABILITY, FAR_FUTURE_MS);

    // Host wiring: build the production driver and inject it into the view entry.
    let injected = service_action_driver_with_clock(service, &[g], clock.clock());
    let mut driver = bind_action_driver(Some(injected));

    let mut app = App::new(
        ui_snapshot(PLUGIN, 1, None),
        Transports::all_in_process(),
        1000,
    );
    let mut control = NoopControlDriver;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut *driver);

    let operation_id = format!("ui:{ACTION}:{START_MS}:1");
    assert_eq!(
        shared.lock().unwrap().invocations,
        vec![(1u64, operation_id.clone())],
        "the production-bound UI dispatch runs the pinned effect exactly once"
    );
    assert_eq!(effect_count_for(&journal, &operation_id), 1);
    assert_eq!(
        action_events(&dir, PLUGIN).len(),
        2,
        "accepted + completed journaled through the common service"
    );
    assert!(app.refresh_requested, "a dispatched action refreshes the view");
    let status = app.status.clone().unwrap_or_default();
    assert!(
        status.contains(ACTION) && status.contains("Completed"),
        "status reflects the terminal outcome: {status}"
    );
}

/// With no service injected the loop binds the fail-closed default: the selected
/// action is refused (`UnsupportedCapability`) with zero effects.
#[test]
fn production_entry_without_service_is_fail_closed() {
    let mut driver = bind_action_driver(None);
    let mut app = App::new(
        ui_snapshot(PLUGIN, 1, None),
        Transports::all_in_process(),
        1000,
    );
    let mut control = NoopControlDriver;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut *driver);

    assert!(!app.refresh_requested, "a refused action does not refresh");
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("error"), "the refusal is surfaced: {status}");
    assert!(
        status.contains("no plugin action service"),
        "the fail-closed default is bound: {status}"
    );
}

/// The production driver with no installed grant denies the action
/// (`AuthorityDenied`) with zero journal entries and zero effects.
#[test]
fn production_entry_without_grant_denies_with_zero_effects() {
    let dir = fresh("hp7-prod-no-grant");
    let clock = ManualClockFn::new(START_MS);
    let journal = dir.join("effects.log");
    let shared = Arc::new(Mutex::new(SharedCalls::default()));
    let exec = FakeExecutor::new(journal.clone(), Arc::clone(&shared));
    let service = new_service(
        &dir,
        vec![advertised(PLUGIN, 1, CAPABILITY, None)],
        exec,
        running_stop(),
        clock.clock(),
    );
    // No parent-issued grant is installed: the UI adds no authority.
    let injected = service_action_driver(service, &[]);
    let mut driver = bind_action_driver(Some(injected));

    let mut app = App::new(
        ui_snapshot(PLUGIN, 1, None),
        Transports::all_in_process(),
        1000,
    );
    let mut control = NoopControlDriver;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, &mut *driver);

    assert!(shared.lock().unwrap().invocations.is_empty(), "no effect ran");
    assert!(
        action_events(&dir, PLUGIN).is_empty(),
        "a denied action journals nothing"
    );
    assert!(!app.refresh_requested);
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("error"), "the denial is surfaced: {status}");
}

// ---------------------------------------------------------------------------
// 20. Pinned-generation routing, plugin-reply identity, grant rotation
// ---------------------------------------------------------------------------

fn intent_pinned(
    plugin_id: &str,
    generation: u64,
    operation_id: &str,
    input: Value,
) -> ActionIntent {
    ActionIntent {
        format: ACTION_INTENT_FORMAT.to_string(),
        plugin_id: plugin_id.to_string(),
        binding: binding(plugin_id, generation),
        action_id: ACTION.to_string(),
        operation_id: operation_id.to_string(),
        input: input.clone(),
        input_digest: canonical_digest(&input).unwrap(),
        deadline_unix_ms: FAR_FUTURE_MS,
        scope: scope(),
        grant_fingerprint: format!("sha256:{}", "b".repeat(64)),
        actor: Actor::Human,
    }
}

/// A plugin reply matching the intent's identity (terminal, no receipt).
fn status_for(intent: &ActionIntent) -> PluginOperationStatus {
    PluginOperationStatus {
        format: PLUGIN_OPERATION_STATUS_FORMAT.to_string(),
        plugin_id: intent.plugin_id.clone(),
        binding: intent.binding.clone(),
        operation_id: intent.operation_id.clone(),
        input_digest: intent.input_digest.clone(),
        state: ComponentOperationState::Completed,
        receipt: None,
    }
}

/// An intent pinned to an unavailable (replaced) generation never contacts the
/// replacement generation's live socket.
#[test]
fn executor_never_contacts_a_replacement_generation() {
    let scratch = fresh("hp7-gen-pin");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    // A live listener stands in for the REPLACEMENT generation's child socket.
    // Its path must fit SUN_LEN, so it is short and outside the long scratch root.
    let replacement = std::env::temp_dir().join(format!("hp7-pin-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&replacement);
    let listener = std::os::unix::net::UnixListener::bind(&replacement).unwrap();
    listener.set_nonblocking(true).unwrap();

    let sockets: Arc<Mutex<BTreeMap<(String, u64), PathBuf>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    sockets
        .lock()
        .unwrap()
        .insert((PLUGIN.to_string(), 2), replacement.clone());
    let mut executor = PluginRpcActionExecutor::new(sockets).unwrap();

    // The pinned generation 1 has no live socket; generation 2 is live.
    assert!(executor.socket(PLUGIN, 1).is_err(), "gen 1 is not reachable");
    assert_eq!(executor.socket(PLUGIN, 2).unwrap(), replacement);

    let intent = intent_pinned(PLUGIN, 1, "op-pin-gen", json!({"n": 1}));
    assert_eq!(executor.reconcile(&intent), ReconcileOutcome::Unknown);
    assert!(matches!(
        executor.invoke(&intent),
        ExecutionOutcome::Refused(_)
    ));

    // The replacement generation was never contacted.
    let contacted = match listener.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => false,
        _ => true,
    };
    let _ = std::fs::remove_file(&replacement);
    assert!(!contacted, "the replacement generation was contacted");
}

/// A plugin reply is verified against the requested identity before its state or
/// outcome is used; a forged/mismatched reply is refused.
#[test]
fn plugin_status_identity_is_verified_before_use() {
    let intent = intent_pinned(PLUGIN, 1, "op-verify", json!({"n": 1}));
    let good = status_for(&intent);
    assert!(verify_plugin_status(&intent, &good).is_ok());

    let mut wrong_plugin = good.clone();
    wrong_plugin.plugin_id = "example.other".to_string();
    assert!(verify_plugin_status(&intent, &wrong_plugin).is_err());

    let mut wrong_binding = good.clone();
    wrong_binding.binding = binding(PLUGIN, 2);
    assert!(verify_plugin_status(&intent, &wrong_binding).is_err());

    let mut wrong_operation = good.clone();
    wrong_operation.operation_id = "op-other".to_string();
    assert!(verify_plugin_status(&intent, &wrong_operation).is_err());

    let mut wrong_digest = good.clone();
    wrong_digest.input_digest = format!("sha256:{}", "f".repeat(64));
    assert!(verify_plugin_status(&intent, &wrong_digest).is_err());

    // A forged receipt for another generation/operation is refused.
    let mut forged = good.clone();
    forged.receipt = Some(ComponentReceipt {
        operation_id: "9:op-verify".to_string(),
        input_digest: intent.input_digest.clone(),
        binding: binding(PLUGIN, 9),
        outcome: json!({"ok": true}),
        completed_at_unix_ms: START_MS,
    });
    assert!(verify_plugin_status(&intent, &forged).is_err());

    // The matching receipt is accepted.
    let mut matching = good.clone();
    matching.receipt = Some(ComponentReceipt {
        operation_id: "1:op-verify".to_string(),
        input_digest: intent.input_digest.clone(),
        binding: binding(PLUGIN, 1),
        outcome: json!({"ok": true}),
        completed_at_unix_ms: START_MS,
    });
    assert!(verify_plugin_status(&intent, &matching).is_ok());
}

/// A host restart rotates the launch grant; the active driver revokes the stale
/// grant from the service before further actions, so the stale token is denied
/// and the new launch grant works.
#[test]
fn host_restart_rotates_grants_in_the_active_driver() {
    let scratch = fresh("hp7-host-grant-rotate");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    let mut host = assemble_granted_host(&state_dir, &root, &ledger);
    let old = host.launch_grant(HOST_PLUGIN).unwrap().clone();

    let service = host.action_service_with_ledger(&ledger).unwrap();
    let mut driver = ServiceActionDriver::with_grants(service, host.action_grants());
    assert!(driver.sync_grants().is_empty(), "the first grant installs");

    // Host restart: a fresh launch epoch rotates the grant (same immutable scope).
    host.launch_request(HOST_PLUGIN).unwrap();
    let new = host.launch_grant(HOST_PLUGIN).unwrap().clone();
    assert_ne!(old.fingerprint(), new.fingerprint(), "the token rotated");
    host.refresh_action_sockets();

    assert_eq!(
        driver.sync_grants(),
        vec![old.fingerprint()],
        "the stale grant is revoked from the active service"
    );

    // The stale grant is denied; the new launch grant authenticates.
    let err = driver
        .service_mut()
        .submit(&host_action_request(
            old.token_hex(),
            1,
            "op-stale",
            json!({}),
        ))
        .unwrap_err();
    assert_eq!(
        failure(&err),
        FailureCode::AuthorityDenied,
        "stale grant denied"
    );

    let err = driver
        .service_mut()
        .submit(&host_action_request(
            new.token_hex(),
            1,
            "op-new",
            json!({}),
        ))
        .unwrap_err();
    assert_eq!(
        failure(&err),
        FailureCode::StateUnavailable,
        "the new launch grant authenticates (the pinned child is unreachable)"
    );
}

/// A plugin handler that returns one canned reply for every spoke action method.
struct ForgedHandler {
    reply: Value,
}

impl PluginHandler for ForgedHandler {
    fn describe(&self) -> Result<Value, ContractError> {
        Ok(json!({}))
    }
    fn health(&self) -> Result<Value, ContractError> {
        Ok(json!({"state": "ready"}))
    }
    fn snapshot(&self) -> Result<Value, ContractError> {
        Ok(json!({}))
    }
    fn invoke(&self, _request: Value) -> Result<Value, ContractError> {
        Ok(self.reply.clone())
    }
    fn reconcile(&self, _request: Value) -> Result<Value, ContractError> {
        Ok(self.reply.clone())
    }
    fn cancel(&self, _request: Value) -> Result<Value, ContractError> {
        Ok(self.reply.clone())
    }
}

/// End to end: a plugin whose `plugin.Invoke`/`Reconcile`/`Cancel` reply names a
/// different operation is refused by the executor (fail closed), never
/// completing the wrong durable intent.
#[test]
fn executor_refuses_a_forged_plugin_reply_end_to_end() {
    let intent = intent_pinned(PLUGIN, 1, "op-forged", json!({"n": 1}));
    // A well-formed reply that names a DIFFERENT operation.
    let mut forged = status_for(&intent);
    forged.operation_id = "op-other".to_string();
    let forged = serde_json::to_value(&forged).unwrap();

    let mut server = Server::new(plugin_server_info(
        PLUGIN,
        "1.2.3",
        std::iter::empty::<String>(),
    ));
    register_plugin(&mut server, Arc::new(ForgedHandler { reply: forged }));

    let socket = std::env::temp_dir().join(format!("hp7-forged-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let listener = server.bind(&socket).unwrap();
    let _server = std::thread::spawn(move || {
        let mut server = server;
        let _ = server.accept_loop(listener);
    });

    let sockets: Arc<Mutex<BTreeMap<(String, u64), PathBuf>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    sockets
        .lock()
        .unwrap()
        .insert((PLUGIN.to_string(), 1), socket.clone());
    let mut executor = PluginRpcActionExecutor::new(sockets).unwrap();

    assert!(
        matches!(executor.invoke(&intent), ExecutionOutcome::Refused(_)),
        "a mismatched invoke reply is refused"
    );
    assert_eq!(
        executor.reconcile(&intent),
        ReconcileOutcome::Unknown,
        "a mismatched reconcile reply stays unknown"
    );
    let cancel = agalma_contracts::PluginCancelRequest {
        format: PLUGIN_CANCEL_REQUEST_FORMAT.to_string(),
        plugin_id: PLUGIN.to_string(),
        binding: binding(PLUGIN, 1),
        cancel_request_id: "cancel-forged".to_string(),
        operation_id: intent.operation_id.clone(),
        deadline_unix_ms: FAR_FUTURE_MS,
        grant_fingerprint: format!("sha256:{}", "c".repeat(64)),
    };
    assert_eq!(
        executor.cancel(&intent, &cancel),
        ReconcileOutcome::CancelRequested,
        "a mismatched cancel reply is not settlement evidence"
    );
    let _ = std::fs::remove_file(&socket);
}
