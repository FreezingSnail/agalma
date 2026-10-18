//! HP.4 common plugin RPC and capability-scoped hub routing acceptance tests
//! (`agalma-jmr.4`).
//!
//! Behavioral coverage (each case runs nonzero meaningful assertions against an
//! independent recording fixture):
//!
//! * an authorized capability routes to its hub adapter exactly once (a repeat
//!   replays the durable receipt, it does not re-run the effect);
//! * missing, expired, wrong-method, wrong-generation grants; unknown hub
//!   methods; unmet capabilities; expired deadlines; explicit stops; and an
//!   unknown (unavailable) stop state each yield **zero effects** (no journal
//!   write, no adapter call);
//! * the same durable operation id with changed input is rejected
//!   (`IdentityConflict`);
//! * two independent clients whose numeric transport ids overlap do not collide
//!   as durable operation identity;
//! * Connect protocol agreement (identity + protocol major) is enforced;
//! * a real Connect capability call over a Unix socket routes to the adapter and
//!   an unknown Connect method has zero effects;
//! * a real Connect callback (the enclosing action calls a spoke that calls back
//!   into the hub) starts before the action completes and both finish within a
//!   bounded deadline — the concurrent dispatcher releases the server lock
//!   before the handler runs, so no external call executes under it;
//! * a deterministic, seeded property test exercises generated request
//!   sequences, including invalid/boundary inputs.
//!
//! Not covered here (owned elsewhere): durable selected-ledger persistence
//! (HP.5) and the UI/autonomous action service (HP.7). The journal here is the
//! injected in-memory test double, so this is not claimed as completed durable
//! production action recovery.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agalma_conductor::components::{derive_plugin_components, HealthView};
use agalma_conductor::composition::{
    HubServices, InMemoryInstallations, PluginHost, HUB_SOCKET_ENV, PLUGIN_GRANT_ENV,
};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveredPackage, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_registry::{
    plugin_socket_path, InMemoryDesiredStateStore, LifecycleState,
};
use agalma_conductor::plugin_reports::ReportIngestor;
use agalma_conductor::plugin_router::{
    CapabilityAdapter, CapabilityCallRequest, CapabilityRouter, GrantScope, InMemoryOperationJournal,
    LaunchGrant, RouterService, RpcHealthProbe, RunningStop, StopAdmission, DEFAULT_MAX_DEPTH,
};
use agalma_conductor::plugin_supervisor::{
    Clock, HealthProbe, HealthProbeResult, ManualClock,
};
use agalma_conductor::stop::{StopControl, StopDecision};
use agalma_contracts::digest::failure_code;
use agalma_contracts::task_package::BindingPin;
use agalma_contracts::{
    ComponentOperationState, ContractError, FailureCode, HealthState, PackageArtifactRef,
    PluginHealth, PluginInstallation, PluginManifest, FACTORY_CONTROL_CAPABILITY,
    FACTORY_READ_CAPABILITY, PLUGIN_HEALTH_FORMAT,
};
use agalma_ipc::plugin_server::{
    plugin_server_info, register_hub, register_plugin, HubHandler, PluginHandler,
    METHOD_INVOKE_CAPABILITY,
};
use agalma_ipc::{ConnectClient, HubCallbackClient, PluginClient, Server, Wire};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

/// Recorded deterministic property-test seed (xorshift64).
const PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_0042;
/// Recorded property-test case budget.
const PROPERTY_CASES: u64 = 40;
/// A far-future expiry/deadline for cases that do not exercise those bounds.
const FAR_FUTURE_MS: u64 = 10_000_000_000_000;

const CAPABILITY: &str = "ledger.read";
const PLUGIN_A: &str = "example.plugin-a";
const PLUGIN_B: &str = "example.plugin-b";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A recording hub adapter that counts and captures every invocation.
struct RecordingAdapter {
    capability: String,
    calls: Arc<Mutex<Vec<Value>>>,
    response: Value,
}

impl CapabilityAdapter for RecordingAdapter {
    fn capability(&self) -> &str {
        &self.capability
    }

    fn invoke(&mut self, _scope: &GrantScope, input: &Value) -> Result<Value, ContractError> {
        self.calls.lock().unwrap().push(input.clone());
        Ok(self.response.clone())
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

/// A valid invoke-capability grant for `plugin_id`.
fn invoke_grant(plugin_id: &str, generation: u64, capability: &str, expiry: u64) -> LaunchGrant {
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

/// A valid reconcile-capability grant for `plugin_id`.
fn reconcile_grant(plugin_id: &str, generation: u64, capability: &str, expiry: u64) -> LaunchGrant {
    LaunchGrant::for_capability(
        plugin_id,
        binding(plugin_id, generation),
        agalma_ipc::METHOD_RECONCILE_CAPABILITY,
        capability,
        expiry,
        None,
    )
    .expect("OS randomness available")
}

fn call(
    grant: &LaunchGrant,
    plugin_id: &str,
    generation: u64,
    capability: &str,
    operation_id: &str,
    input: Value,
    deadline: u64,
) -> CapabilityCallRequest {
    CapabilityCallRequest {
        grant: grant.token_hex(),
        plugin_id: plugin_id.to_string(),
        generation,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: capability.to_string(),
        operation_id: operation_id.to_string(),
        input,
        deadline_unix_ms: deadline,
        depth: 0,
    }
}

/// A router with the recording adapter registered and one live grant installed.
fn router_with_adapter(
    plugin_id: &str,
    generation: u64,
    capability: &str,
) -> (CapabilityRouter, Arc<Mutex<Vec<Value>>>, LaunchGrant) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut router = CapabilityRouter::new(
        Box::new(InMemoryOperationJournal::new()),
        running_stop(),
    );
    router.register_adapter(RecordingAdapter {
        capability: capability.to_string(),
        calls: calls.clone(),
        response: json!({ "ok": true }),
    });
    let grant = invoke_grant(plugin_id, generation, capability, FAR_FUTURE_MS);
    router.install_grant(&grant);
    (router, calls, grant)
}

fn call_count(calls: &Arc<Mutex<Vec<Value>>>) -> usize {
    calls.lock().unwrap().len()
}

// ---------------------------------------------------------------------------
// Direct router cases
// ---------------------------------------------------------------------------

#[test]
fn allowed_route_routes_exactly_once() {
    let (mut router, calls, grant) = router_with_adapter(PLUGIN_A, 1, CAPABILITY);
    let request = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({ "k": 1 }), FAR_FUTURE_MS);

    let first = router.invoke_and_execute(&request).expect("authorized call routes");
    assert_eq!(first.state, ComponentOperationState::Completed);
    assert!(first.receipt.is_some(), "terminal call carries a durable receipt");
    assert_eq!(call_count(&calls), 1);

    // Repeat with the same operation id and input: replay, not a second effect.
    let replay = router.invoke_and_execute(&request).expect("replay is acknowledged");
    assert_eq!(replay.state, ComponentOperationState::Completed);
    assert_eq!(call_count(&calls), 1, "allowed route ran exactly once");
    assert_eq!(router.journaled_operations(), 1);
}

#[test]
fn missing_grant_yields_zero_effects() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop());
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: calls.clone(),
        response: json!({ "ok": true }),
    });
    // No grant is installed; a token that was never issued is presented.
    let orphan = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    let request = call(&orphan, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);

    let error = router.invoke_and_execute(&request).expect_err("unknown grant is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert_eq!(call_count(&calls), 0);
    assert_eq!(router.journaled_operations(), 0);
}

#[test]
fn expired_grant_yields_zero_effects() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst)) as Arc<dyn Fn() -> u64 + Send + Sync>
    };
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop())
        .with_clock(clock);
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: calls.clone(),
        response: json!({ "ok": true }),
    });
    // Expiry exactly at the current instant is already refused.
    let grant = invoke_grant(PLUGIN_A, 1, CAPABILITY, 1_000);
    router.install_grant(&grant);
    let request = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);

    let error = router.invoke_and_execute(&request).expect_err("expired grant is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert_eq!(call_count(&calls), 0);
    assert_eq!(router.journaled_operations(), 0);
}

#[test]
fn wrong_generation_grant_yields_zero_effects() {
    let (mut router, calls, grant) = router_with_adapter(PLUGIN_A, 1, CAPABILITY);
    // The grant pins generation 1; the request claims generation 2.
    let request = call(&grant, PLUGIN_A, 2, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);

    let error = router.invoke_and_execute(&request).expect_err("stale generation is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::StaleGeneration));
    assert_eq!(call_count(&calls), 0);
    assert_eq!(router.journaled_operations(), 0);
}

#[test]
fn unknown_method_yields_zero_effects() {
    let (mut router, calls, grant) = router_with_adapter(PLUGIN_A, 1, CAPABILITY);
    let mut request = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);
    request.method = "hub.Nonexistent".to_string();

    let error = router.invoke_and_execute(&request).expect_err("unknown method is refused");
    assert!(matches!(error, ContractError::UnsupportedCapability(_)));
    assert_eq!(call_count(&calls), 0);
    assert_eq!(router.journaled_operations(), 0);
}

#[test]
fn unmet_capability_yields_zero_effects() {
    let (mut router, calls, grant) = router_with_adapter(PLUGIN_A, 1, CAPABILITY);
    // The grant authorizes `ledger.read`; the request asks for another capability.
    let request = call(&grant, PLUGIN_A, 1, "ledger.write", "op-1", json!({}), FAR_FUTURE_MS);

    let error = router.invoke_and_execute(&request).expect_err("unmet capability is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::MissingCapability));
    assert_eq!(call_count(&calls), 0);
    assert_eq!(router.journaled_operations(), 0);
}

#[test]
fn expired_deadline_yields_zero_effects() {
    let now = Arc::new(AtomicU64::new(2_000));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst)) as Arc<dyn Fn() -> u64 + Send + Sync>
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop())
        .with_clock(clock);
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: calls.clone(),
        response: json!({ "ok": true }),
    });
    let grant = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    router.install_grant(&grant);
    let request = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), 2_000);

    let error = router.invoke_and_execute(&request).expect_err("expired deadline is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::DeadlineExceeded));
    assert_eq!(call_count(&calls), 0);
    assert_eq!(router.journaled_operations(), 0);
}

#[test]
fn unknown_stop_state_and_explicit_stop_yield_zero_effects() {
    for (label, decision, expected) in [
        (
            "unavailable",
            StopDecision::Unavailable("stop state unreadable".to_string()),
            FailureCode::StateUnavailable,
        ),
        (
            "stopped",
            StopDecision::Stopped("stop policy active".to_string()),
            FailureCode::Stopped,
        ),
    ] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(FakeStop(decision)) as Arc<dyn StopAdmission>;
        let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), stop);
        router.register_adapter(RecordingAdapter {
            capability: CAPABILITY.to_string(),
            calls: calls.clone(),
            response: json!({ "ok": true }),
        });
        let grant = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
        router.install_grant(&grant);
        let request = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);

        let error = router
            .invoke_and_execute(&request)
            .expect_err("stop admission refuses the effect");
        assert_eq!(failure_code(&error), Some(expected), "case {label}");
        assert_eq!(call_count(&calls), 0, "case {label}");
        assert_eq!(router.journaled_operations(), 0, "case {label}");
    }
}

#[test]
fn changed_input_same_operation_id_is_rejected() {
    let (mut router, calls, grant) = router_with_adapter(PLUGIN_A, 1, CAPABILITY);
    let first = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({ "k": 1 }), FAR_FUTURE_MS);
    router.invoke_and_execute(&first).expect("first effect accepted");

    let changed = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({ "k": 2 }), FAR_FUTURE_MS);
    let error = router
        .invoke_and_execute(&changed)
        .expect_err("changed input under the same operation id conflicts");
    assert_eq!(failure_code(&error), Some(FailureCode::IdentityConflict));
    assert_eq!(call_count(&calls), 1, "the conflicting call had zero effects");
    assert_eq!(router.journaled_operations(), 1);
}

#[test]
fn overlapping_transport_ids_do_not_collide() {
    // Two independent clients (distinct plugins/generations) both use the durable
    // operation id `shared-op`. Durable identity is (plugin, generation,
    // operation_id); the Connect envelope's numeric transport id plays no part,
    // so the two operations cannot collide even when both clients happen to
    // allocate the same numeric request id.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop());
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: calls.clone(),
        response: json!({ "ok": true }),
    });
    let grant_a = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    let grant_b = invoke_grant(PLUGIN_B, 1, CAPABILITY, FAR_FUTURE_MS);
    router.install_grant(&grant_a);
    router.install_grant(&grant_b);

    let req_a = call(&grant_a, PLUGIN_A, 1, CAPABILITY, "shared-op", json!({ "who": "a" }), FAR_FUTURE_MS);
    let req_b = call(&grant_b, PLUGIN_B, 1, CAPABILITY, "shared-op", json!({ "who": "b" }), FAR_FUTURE_MS);

    let status_a = router.invoke_and_execute(&req_a).expect("client A routes");
    let status_b = router.invoke_and_execute(&req_b).expect("client B routes");
    assert_ne!(status_a.plugin_id, status_b.plugin_id, "distinct durable operations");
    assert_eq!(call_count(&calls), 2, "both clients' effects ran");
    assert_eq!(router.journaled_operations(), 2);

    // Replaying client A's op replays only A's receipt.
    let replay = router.invoke_and_execute(&req_a).expect("A replays");
    assert_eq!(replay.plugin_id, PLUGIN_A);
    assert_eq!(replay.operation_id, "shared-op");
    assert_eq!(call_count(&calls), 2, "a replay caused no new effect");
}

#[test]
fn invalid_grant_and_depth_boundaries_are_refused() {
    let (mut router, calls, grant) = router_with_adapter(PLUGIN_A, 1, CAPABILITY);

    // A token of the wrong length is refused before any effect.
    let mut short = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);
    short.grant = "abcd".to_string();
    let error = router.invoke_and_execute(&short).expect_err("short token refused");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));

    // A non-hex token of the right length is refused.
    let mut non_hex = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-2", json!({}), FAR_FUTURE_MS);
    non_hex.grant = "z".repeat(64);
    let error = router.invoke_and_execute(&non_hex).expect_err("non-hex token refused");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));

    // A nested-capability call beyond the depth bound is refused.
    let mut deep = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-3", json!({}), FAR_FUTURE_MS);
    deep.depth = DEFAULT_MAX_DEPTH + 1;
    let error = router.invoke_and_execute(&deep).expect_err("depth bound refused");
    assert_eq!(failure_code(&error), Some(FailureCode::MissingCapability));

    assert_eq!(call_count(&calls), 0);
    assert_eq!(router.journaled_operations(), 0);
}

// ---------------------------------------------------------------------------
// Deterministic property test
// ---------------------------------------------------------------------------

/// xorshift64 so a property failure replays exactly from [`PROPERTY_SEED`].
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

#[test]
fn property_refusals_have_zero_effects_and_replays_do_not_repeat() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let now = Arc::new(AtomicU64::new(5_000));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst)) as Arc<dyn Fn() -> u64 + Send + Sync>
    };
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop())
        .with_clock(clock);
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: calls.clone(),
        response: json!({ "ok": true }),
    });
    let grant = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    router.install_grant(&grant);

    let mut rng = XorShift64::new(PROPERTY_SEED);
    // op id -> the input it was first accepted with
    let mut accepted: BTreeMap<String, Value> = BTreeMap::new();
    let mut expected_calls: u64 = 0;

    for case in 0..PROPERTY_CASES {
        let op_id = format!("op-{}", rng.below(4));
        let input = json!({ "n": rng.below(2) });
        let adversarial = rng.below(10) < 4;
        let calls_before = call_count(&calls);
        let journal_before = router.journaled_operations();
        let mut request =
            call(&grant, PLUGIN_A, 1, CAPABILITY, &op_id, input.clone(), FAR_FUTURE_MS);

        if adversarial {
            match rng.below(5) {
                0 => request.grant = "00".repeat(32), // unknown token
                1 => request.generation = 2,           // wrong generation
                2 => request.method = "hub.Unknown".to_string(),
                3 => request.capability = "ledger.write".to_string(),
                _ => request.deadline_unix_ms = 5_000, // already passed
            }
            let result = router.invoke_and_execute(&request);
            assert!(result.is_err(), "case {case}: adversarial request must be refused");
            assert_eq!(
                call_count(&calls),
                calls_before,
                "case {case}: refusal had zero adapter effects"
            );
            assert_eq!(
                router.journaled_operations(),
                journal_before,
                "case {case}: refusal journaled nothing"
            );
            continue;
        }

        match accepted.get(&op_id).cloned() {
            Some(first_input) if first_input == input => {
                // Replay: acknowledged, no new effect.
                let status = router
                    .invoke_and_execute(&request)
                    .unwrap_or_else(|e| panic!("case {case}: replay should acknowledge: {e:?}"));
                assert_eq!(status.state, ComponentOperationState::Completed);
                assert_eq!(call_count(&calls), calls_before, "case {case}: replay repeated no effect");
            }
            Some(_) => {
                // Same op id, changed input: conflict, zero effects.
                let error = router
                    .invoke_and_execute(&request)
                    .expect_err("case: changed input conflicts");
                assert_eq!(failure_code(&error), Some(FailureCode::IdentityConflict), "case {case}");
                assert_eq!(call_count(&calls), calls_before, "case {case}");
                assert_eq!(router.journaled_operations(), journal_before, "case {case}");
            }
            None => {
                let status = router
                    .invoke_and_execute(&request)
                    .unwrap_or_else(|e| panic!("case {case}: fresh authorized call routes: {e:?}"));
                assert_eq!(status.state, ComponentOperationState::Completed);
                expected_calls += 1;
                accepted.insert(op_id.clone(), input.clone());
                assert_eq!(call_count(&calls), expected_calls as usize, "case {case}");
                assert_eq!(
                    router.journaled_operations(),
                    accepted.len(),
                    "case {case}: one journal row per durable operation"
                );
            }
        }
    }

    assert!(expected_calls > 0, "property exercised at least one accepted route");
    assert_eq!(call_count(&calls), expected_calls as usize);
}

// ---------------------------------------------------------------------------
// Connect transport cases
// ---------------------------------------------------------------------------

fn wait_for_socket(path: &Path) {
    for _ in 0..400 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("socket {} did not appear", path.display());
}

/// Spawn a Connect server on a scratch socket and wait until it is bound.
fn spawn_connect_server(
    scratch: &ScratchDir,
    name: &str,
    info: agalma_ipc::ServerInfo,
    configure: impl FnOnce(&mut Server) + Send + 'static,
) -> PathBuf {
    // Use the production SUN_LEN-safe socket path: a deep evidence scratch root
    // exceeds macOS `sun_path`, so this falls back to a short per-user path.
    spawn_connect_server_at(
        plugin_socket_path(scratch.path(), name, 0),
        info,
        configure,
    )
}

/// Spawn a Connect server bound at an exact socket path and wait until bound.
///
/// Used by the production-host health regression: the child is served at the
/// supervisor's real launch socket (`plugin_socket_path(state_dir, plugin_id,
/// epoch)`) rather than the scratch-derived default.
fn spawn_connect_server_at(
    socket: PathBuf,
    info: agalma_ipc::ServerInfo,
    configure: impl FnOnce(&mut Server) + Send + 'static,
) -> PathBuf {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).expect("create socket dir");
    }
    let mut server = Server::new(info);
    configure(&mut server);
    let bind_path = socket.clone();
    std::thread::spawn(move || {
        let _ = agalma_ipc::serve_wire(server, &bind_path, Wire::Connect);
    });
    wait_for_socket(&socket);
    socket
}

/// A plugin handler with a fixed Describe and a caller-supplied `plugin.Health`
/// record + Invoke.
struct FakePlugin {
    plugin_id: String,
    health: Value,
    invoke: Box<dyn Fn(Value) -> Result<Value, ContractError> + Send + Sync>,
}

impl FakePlugin {
    /// A plugin whose `plugin.Health` returns a well-formed `ready`
    /// `PluginHealth/v1` for `plugin_id`/`generation`.
    fn ready(plugin_id: &str, generation: u64) -> Self {
        FakePlugin {
            plugin_id: plugin_id.to_string(),
            health: plugin_health_value(
                plugin_id,
                &binding(plugin_id, generation),
                HealthState::Ready,
                "serving",
            ),
            invoke: Box::new(|_| Ok(Value::Null)),
        }
    }

    /// Override the exact `plugin.Health` JSON the peer returns (including a
    /// deliberately malformed record).
    fn with_health(mut self, health: Value) -> Self {
        self.health = health;
        self
    }

    /// Override the `plugin.Invoke` behavior.
    fn with_invoke(
        mut self,
        invoke: Box<dyn Fn(Value) -> Result<Value, ContractError> + Send + Sync>,
    ) -> Self {
        self.invoke = invoke;
        self
    }
}

/// One serialized `PluginHealth/v1` record.
fn plugin_health_value(
    plugin_id: &str,
    binding: &BindingPin,
    state: HealthState,
    detail: &str,
) -> Value {
    serde_json::to_value(PluginHealth {
        format: PLUGIN_HEALTH_FORMAT.to_string(),
        plugin_id: plugin_id.to_string(),
        binding: binding.clone(),
        state,
        detail: detail.to_string(),
        checked_at_unix_ms: 0,
    })
    .expect("PluginHealth serializes")
}

impl PluginHandler for FakePlugin {
    fn describe(&self) -> Result<Value, ContractError> {
        Ok(json!({
            "api": "plugin",
            "protocol_major": 1,
            "plugin_id": self.plugin_id,
            "implementation_version": "1.2.3",
            "capabilities": ["ledger.read"],
        }))
    }

    fn health(&self) -> Result<Value, ContractError> {
        Ok(self.health.clone())
    }

    fn snapshot(&self) -> Result<Value, ContractError> {
        Ok(Value::Null)
    }

    fn invoke(&self, request: Value) -> Result<Value, ContractError> {
        (self.invoke)(request)
    }
}

fn to_contract(error: agalma_ipc::IpcError) -> ContractError {
    match error {
        agalma_ipc::IpcError::Contract(inner) => inner,
        other => ContractError::KnownFailure(other.to_string()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_protocol_agreement_is_enforced() {
    let scratch = ScratchDir::new("hp-routes-agreement");
    let plugin_id = "example.agree";
    let socket = {
        let plugin_id = plugin_id.to_string();
        spawn_connect_server(
            &scratch,
            "plugin",
            plugin_server_info(&plugin_id, "1.2.3", ["ledger.read".to_string()]),
            move |server| {
                register_plugin(
                    server,
                    Arc::new(FakePlugin::ready(&plugin_id, 1)),
                );
            },
        )
    };

    let mut client = PluginClient::connect(&socket).await.expect("connect");
    let identity = client.handshake(plugin_id).await.expect("identity agreement");
    assert_eq!(identity.api, "plugin");
    assert_eq!(identity.protocol_major, 1);
    assert_eq!(identity.plugin_id, plugin_id);

    // A peer with the wrong identity is refused before any effect.
    let mut client = PluginClient::connect(&socket).await.expect("reconnect");
    let error = client
        .handshake("some.other.plugin")
        .await
        .expect_err("identity mismatch refused");
    assert!(matches!(error, agalma_ipc::IpcError::Contract(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_capability_call_routes_to_adapter_and_unknown_method_is_refused() {
    let scratch = ScratchDir::new("hp-routes-connect-route");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop());
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: calls.clone(),
        response: json!({ "routed": true }),
    });
    let grant = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    router.install_grant(&grant);
    let service_router = Arc::new(RouterService::new(router));
    let service: Arc<dyn HubHandler> = service_router.clone();

    let socket = spawn_connect_server(
        &scratch,
        "hub",
        plugin_server_info("hub", "0.0.1", Vec::<String>::new()),
        move |server| register_hub(server, service),
    );

    let request = call(&grant, PLUGIN_A, 1, CAPABILITY, "op-connect", json!({ "n": 7 }), FAR_FUTURE_MS);
    let mut hub = HubCallbackClient::connect(&socket).await.expect("connect hub");
    let result = hub
        .invoke_capability(serde_json::to_value(&request).unwrap())
        .await
        .expect("authorized capability routes over Connect");
    let status: Value = result;
    assert_eq!(status["state"], json!("completed"));
    assert_eq!(call_count(&calls), 1);

    // Reconcile over Connect returns the durable terminal status, using its own
    // frozen reconcile-scoped grant.
    let reconcile_scope = reconcile_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    service_router.router().install_grant(&reconcile_scope);
    let mut reconcile = request.clone();
    reconcile.method = agalma_ipc::METHOD_RECONCILE_CAPABILITY.to_string();
    reconcile.grant = reconcile_scope.token_hex();
    let result = hub
        .reconcile_capability(serde_json::to_value(&reconcile).unwrap())
        .await
        .expect("reconcile returns durable status");
    assert_eq!(result["state"], json!("completed"));
    assert_eq!(result["operation_id"], json!("op-connect"));

    // An unknown Connect method has zero effects.
    let mut raw = ConnectClient::connect(&socket).await.expect("raw connect");
    let error = raw
        .call("hub.Nonexistent", Value::Null)
        .await
        .expect_err("unknown method refused");
    assert!(matches!(
        error,
        agalma_ipc::IpcError::Contract(ContractError::UnsupportedCapability(_))
    ));
    assert_eq!(call_count(&calls), 1, "unknown method had zero effects");
}

/// Drive a blocking Connect client call from a handler thread.
fn blocking_call<T, F>(f: F) -> Result<T, ContractError>
where
    T: Send + 'static,
    F: FnOnce(&tokio::runtime::Runtime) -> T + Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("callback runtime");
        f(&runtime)
    })
    .join()
    .map_err(|_| ContractError::KnownFailure("callback thread panicked".to_string()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_connect_callback_starts_before_completion_within_deadline() {
    let scratch = ScratchDir::new("hp-routes-callback");
    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    // The hub serves `hub.PublishReport`; the enclosing hub action
    // (`hub.InvokeCapability`) calls the spoke, which calls back into the hub.
    let hub_socket = {
        let events = events.clone();
        spawn_connect_server(
            &scratch,
            "hub",
            plugin_server_info("hub", "0.0.1", Vec::<String>::new()),
            move |server| {
                let publish_events = events.clone();
                server.on("hub.PublishReport", move |_params: Value| {
                    publish_events.lock().unwrap().push("publish");
                    Ok(json!({ "accepted": true }))
                });
                let action_events = events.clone();
                server.on("hub.InvokeCapability", move |_params: Value| {
                    action_events.lock().unwrap().push("outer-start");
                    // The enclosing action performs a real external RPC to the
                    // spoke while it is still in flight. The spoke's socket is
                    // published once both servers are up (process-global slot).
                    let plugin_socket = scratch_path_for_plugin();
                    let _ = blocking_call(move |runtime| {
                        runtime.block_on(async move {
                            let mut client = PluginClient::connect(&plugin_socket)
                                .await
                                .map_err(to_contract)?;
                            client.invoke(json!({ "operation_id": "outer-action" })).await
                                .map_err(to_contract)
                        })
                    })??;
                    action_events.lock().unwrap().push("outer-done");
                    Ok(json!({ "completed": true }))
                });
            },
        )
    };

    // The spoke's `plugin.Invoke` calls back into the hub's `hub.PublishReport`
    // before it replies.
    let plugin_socket = {
        let events = events.clone();
        let hub_socket = hub_socket.clone();
        let plugin_id = "example.callback".to_string();
        spawn_connect_server(
            &scratch,
            "plugin",
            plugin_server_info(&plugin_id, "1.2.3", ["ledger.read".to_string()]),
            move |server| {
                register_plugin(
                    server,
                    Arc::new(FakePlugin::ready(&plugin_id, 1).with_invoke(Box::new(
                        move |_req: Value| {
                            events.lock().unwrap().push("plugin-entered");
                            let hub_socket = hub_socket.clone();
                            let _ = blocking_call(move |runtime| {
                                runtime.block_on(async move {
                                    let mut hub = HubCallbackClient::connect(&hub_socket)
                                        .await
                                        .map_err(to_contract)?;
                                    hub.publish_report(json!({ "seq": 1 })).await.map_err(to_contract)
                                })
                            })??;
                            events.lock().unwrap().push("plugin-exit");
                            Ok(json!({ "done": true }))
                        },
                    ))),
                );
            },
        )
    };
    // The hub action closes over the plugin socket; expose it via the helper.
    set_plugin_socket(plugin_socket.clone());

    // Bound the whole exchange: a regression that holds the dispatcher lock
    // across the outbound call would deadlock here instead of hanging the suite.
    let deadline = Duration::from_secs(10);
    let started = Instant::now();
    let mut hub = HubCallbackClient::connect(&hub_socket).await.expect("connect hub");
    let outcome = tokio::time::timeout(
        deadline,
        hub.invoke_capability(json!({ "operation_id": "outer-action" })),
    )
    .await
    .expect("callback exchange finished within the bounded deadline")
    .expect("hub action completed");
    let elapsed = started.elapsed();

    assert_eq!(outcome["completed"], json!(true));
    assert!(elapsed < deadline, "exchange finished in {elapsed:?}");

    // Ordering: the callback (publish) began and finished *before* the enclosing
    // action completed.
    let observed = events.lock().unwrap().clone();
    let publish = observed.iter().position(|e| *e == "publish").expect("callback ran");
    let outer_done = observed
        .iter()
        .position(|e| *e == "outer-done")
        .expect("action completed");
    assert!(
        publish < outer_done,
        "callback must begin before the enclosing action completes: {observed:?}"
    );
    assert_eq!(observed.first(), Some(&"outer-start"));
    assert!(observed.contains(&"plugin-entered"));
}

// The hub action's closure needs the plugin socket, which is created after the
// hub server in the callback test. A process-global one-slot avoids threading it
// through the closure lifetime.
static PLUGIN_SOCKET: Mutex<Option<PathBuf>> = Mutex::new(None);

fn set_plugin_socket(path: PathBuf) {
    *PLUGIN_SOCKET.lock().unwrap() = Some(path);
}

fn scratch_path_for_plugin() -> PathBuf {
    PLUGIN_SOCKET
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
}

// ---------------------------------------------------------------------------
// Repair regressions (Sol review findings 1-5)
// ---------------------------------------------------------------------------

/// Finding 1: rotating the token for a new launch revokes the old token while
/// the immutable logical binding scope (plugin/generation/method/capability) is
/// unchanged.
#[test]
fn grant_rotation_revokes_old_token_preserving_scope() {
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop());
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: Arc::new(Mutex::new(Vec::new())),
        response: json!({ "ok": true }),
    });
    let first = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    let fingerprint = router.install_grant(&first);
    let scope_before = first.scope().clone();

    // A new launch reissues the SAME logical scope under a fresh token; the old
    // token is revoked (a leaked launch token cannot resume a replacement).
    let rotated = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    assert_ne!(first.token_hex(), rotated.token_hex(), "each launch gets a fresh token");
    assert_eq!(rotated.scope(), &scope_before, "logical binding scope is preserved");
    assert!(router.revoke_grant(&fingerprint));
    router.install_grant(&rotated);

    let stale = call(&first, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);
    let error = router.invoke_and_execute(&stale).expect_err("revoked token is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));

    let current = call(&rotated, PLUGIN_A, 1, CAPABILITY, "op-2", json!({}), FAR_FUTURE_MS);
    router.invoke_and_execute(&current).expect("rotated token authenticates");
}

/// Finding 2: reconcile enforces the frozen single-method scope and the declared
/// generation; an invoke-scoped grant cannot reconcile.
#[test]
fn reconcile_requires_reconcile_scope_and_generation() {
    let (mut router, calls, invoke_scope) = router_with_adapter(PLUGIN_A, 1, CAPABILITY);
    let invoke_call = call(&invoke_scope, PLUGIN_A, 1, CAPABILITY, "op-1", json!({}), FAR_FUTURE_MS);
    router.invoke_and_execute(&invoke_call).expect("invoke establishes the operation");

    // An invoke-scoped grant may not reconcile.
    let mut reconcile = invoke_call.clone();
    reconcile.method = agalma_ipc::METHOD_RECONCILE_CAPABILITY.to_string();
    let error = router
        .reconcile(&reconcile)
        .expect_err("invoke scope cannot reconcile");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));

    // A reconcile-scoped grant reconciles, but only for its bound generation.
    let reconcile_scope = reconcile_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    router.install_grant(&reconcile_scope);
    let mut good = reconcile.clone();
    good.grant = reconcile_scope.token_hex();
    let status = router.reconcile(&good).expect("reconcile scope reconciles");
    assert_eq!(status.state, ComponentOperationState::Completed);

    let mut wrong_generation = good.clone();
    wrong_generation.generation = 2;
    let error = router
        .reconcile(&wrong_generation)
        .expect_err("wrong declared generation refused");
    assert_eq!(failure_code(&error), Some(FailureCode::StaleGeneration));
    assert_eq!(call_count(&calls), 1);
}

/// Finding 3: concurrent retries of the same durable operation run the adapter
/// exactly once (the journal's fresh-claim is atomic).
#[test]
fn concurrent_retry_same_operation_runs_adapter_once() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut router = CapabilityRouter::new(Box::new(InMemoryOperationJournal::new()), running_stop());
    router.register_adapter(RecordingAdapter {
        capability: CAPABILITY.to_string(),
        calls: calls.clone(),
        response: json!({ "ok": true }),
    });
    let grant = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);
    router.install_grant(&grant);
    let service = Arc::new(RouterService::new(router));

    let request = serde_json::to_value(call(
        &grant,
        PLUGIN_A,
        1,
        CAPABILITY,
        "op-race",
        json!({ "k": 1 }),
        FAR_FUTURE_MS,
    ))
    .unwrap();

    let outcomes: Vec<Result<Value, ContractError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let service = service.clone();
                let request = request.clone();
                scope.spawn(move || service.invoke_capability(request))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    assert!(outcomes.iter().all(Result::is_ok), "every concurrent retry is acknowledged");
    assert_eq!(call_count(&calls), 1, "exactly one adapter effect for the durable operation");
    assert_eq!(service.router().journaled_operations(), 1);
}

/// Finding 4: two independent Connect clients using the same numeric request id
/// over a real UDS do not collide (receipts are per-client scoped).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_connect_clients_overlapping_ids_do_not_collide() {
    let scratch = ScratchDir::new("hp-routes-receipt-scope");
    let counter = Arc::new(AtomicU64::new(0));
    let observed = counter.clone();
    let socket = spawn_connect_server(
        &scratch,
        "scope",
        plugin_server_info("scope", "0.0.1", Vec::<String>::new()),
        move |server| {
            server.on("counter", move |_params: Value| {
                let n = observed.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(json!({ "count": n }))
            });
        },
    );

    let a = ConnectClient::connect(&socket).await.expect("client a");
    let b = ConnectClient::connect(&socket).await.expect("client b");
    let from_a = a.call_with_id(1, "counter", json!({})).await.expect("a id 1");
    let from_b = b.call_with_id(1, "counter", json!({})).await.expect("b id 1");
    assert_eq!(from_a, json!({ "count": 1 }));
    assert_eq!(from_b, json!({ "count": 2 }), "independent clients do not share receipts");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

const PROD_PLUGIN_ID: &str = "example.prod";
const PROD_CONFIG_SCHEMA: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
const PROD_DEFAULT_CONFIG: &[u8] = b"{}\n";
const PROD_REPORT_SCHEMA: &[u8] = b"{\"type\":\"object\",\"required\":[\"ready\"]}\n";

fn artifact_ref(path: &str, bytes: &[u8]) -> PackageArtifactRef {
    PackageArtifactRef {
        path: path.to_string(),
        digest: agalma_contracts::sha256_digest(bytes),
        bytes: bytes.len() as u64,
        media_type: "application/json".to_string(),
    }
}

/// Seal one production plugin package under `root` and discover it.
fn seal_production_package(
    root: &Path,
    plugin_id: &str,
) -> agalma_conductor::plugin_manifest::DiscoveredPackage {
    seal_production_package_version(root, plugin_id, "1.2.3")
}

/// Seal a production plugin package with an explicit `package_version` so a
/// replacement seals distinct bytes (and therefore a distinct generation).
fn seal_production_package_version(
    root: &Path,
    plugin_id: &str,
    package_version: &str,
) -> agalma_conductor::plugin_manifest::DiscoveredPackage {
    let package_dir = root.join("pkg");
    let write = |relative: &str, bytes: &[u8]| {
        let path = package_dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };
    write("objects/config-schema.json", PROD_CONFIG_SCHEMA);
    write("objects/default-config.json", PROD_DEFAULT_CONFIG);
    write("objects/report-schema.json", PROD_REPORT_SCHEMA);
    // A real child: announces the identity/protocol/capabilities the supervisor's
    // READY gate requires, then drains stdin for the process lifetime.
    let child = format!(
        "#!/bin/sh\nprintf 'READY {{\"api\":\"plugin\",\"api_version\":1,\"impl\":\"{plugin_id}\",\"impl_version\":\"1.2.3\",\"capabilities\":[\"{CAPABILITY}\"]}}\n'\ncat >/dev/null\n"
    );
    write("bin/plugin", child.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            package_dir.join("bin/plugin"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let mut manifest = json!({
        "format": "PluginManifest/v1",
        "plugin_id": plugin_id,
        "package_version": package_version,
        "package_digest": format!("sha256:{}", "0".repeat(64)),
        "protocol_major": 1,
        "entrypoint": {"path": "bin/plugin", "argv": ["--serve"], "launch_profile": "prod/v1"},
        "domain_apis": [],
        "requested_capabilities": [CAPABILITY],
        "required_services": [],
        "optional_services": [],
        "config_schema": artifact_ref("objects/config-schema.json", PROD_CONFIG_SCHEMA),
        "default_configuration": artifact_ref("objects/default-config.json", PROD_DEFAULT_CONFIG),
        "report_schemas": [{
            "namespace": "example.prod.status",
            "version": 1,
            "schema": artifact_ref("objects/report-schema.json", PROD_REPORT_SCHEMA)
        }],
        "views": [{
            "view_id": "status",
            "schema_namespace": "example.prod.status",
            "kind": "Status",
            "title": "Prod status",
            "schema_version": 1,
            "fields": ["ready"]
        }],
        "actions": [],
        "state_compatibility": {"reads": ["prod-state/v1"], "writes": "prod-state/v1"}
    });
    let parsed: PluginManifest = serde_json::from_value(manifest.clone()).unwrap();
    let digest = package_inventory_digest(&package_dir, &parsed).unwrap();
    manifest["package_digest"] = Value::String(digest);
    fs::write(
        package_dir.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let report = discover_plugins(&[PluginRoot::required(root)]).unwrap();
    report.package(plugin_id).expect("package discovered").clone()
}

/// Assemble a production host with the recording adapter installed.
fn assemble_production_host(
    scratch: &ScratchDir,
    calls: Arc<Mutex<Vec<Value>>>,
) -> PluginHost {
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let package = seal_production_package(&root, PROD_PLUGIN_ID);
    let installation = {
        let mut installation = PluginInstallation {
            format: "PluginInstallation/v1".to_string(),
            plugin_id: PROD_PLUGIN_ID.to_string(),
            package_digest: package.package_digest.clone(),
            configuration: artifact_ref("objects/default-config.json", PROD_DEFAULT_CONFIG),
            launch_profile: "prod/v1".to_string(),
            approved_capabilities: vec![CAPABILITY.to_string()],
            installation_digest: String::new(),
        };
        installation.installation_digest = installation.recomputed_installation_digest().unwrap();
        installation
    };
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation);
    let config = PluginHostConfig {
        state_dir: scratch.child("state"),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    let clock: Rc<dyn Clock> = Rc::new(ManualClock::new(1_000_000));
    PluginHost::assemble_with_hub(
        &config,
        &installations,
        Box::new(InMemoryDesiredStateStore::new()),
        clock,
        HubServices {
            journal: Box::new(InMemoryOperationJournal::new()),
            stop: Arc::new(RunningStop),
            adapters: vec![Box::new(RecordingAdapter {
                capability: CAPABILITY.to_string(),
                calls,
                response: json!({ "routed": true }),
            })],
            reports: None,
        },
    )
    .expect("production host assembles")
}

/// Finding 1 (re-review): every launch epoch issues a fresh scoped grant and
/// revokes the prior token, while the immutable binding scope is unchanged.
#[test]
fn production_restart_rotates_launch_grant() {
    let scratch = ScratchDir::new("hp-routes-restart");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut host = assemble_production_host(&scratch, calls.clone());
    let generation = *host.installed.get(PROD_PLUGIN_ID).expect("installed generation");

    // First launch (epoch 1).
    let launch1 = host.launch_request(PROD_PLUGIN_ID).expect("launch 1");
    let grant1 = host.launch_grant(PROD_PLUGIN_ID).cloned().expect("grant 1");
    let scope1 = grant1.scope().clone();

    // Simulate the real restart path: the supervisor marks a new launch epoch.
    host.registry
        .mark_starting(PROD_PLUGIN_ID, generation, 2_000_000)
        .expect("mark starting");
    let launch2 = host.launch_request(PROD_PLUGIN_ID).expect("launch 2");
    let grant2 = host.launch_grant(PROD_PLUGIN_ID).cloned().expect("grant 2");

    assert_ne!(grant1.token_hex(), grant2.token_hex(), "restart rotates the token");
    assert_ne!(
        launch1.socket, launch2.socket,
        "a new launch epoch uses a fresh socket"
    );
    // The immutable binding scope (plugin, pinned binding, method, resource
    // namespace, directive) is preserved across restart. The expiry is a
    // wall-clock refresh (`wall_clock_now_ms() + LAUNCH_GRANT_TTL_MS`, no clock
    // seam), so it may advance by a millisecond between launches; assert the
    // immutable fields exactly and the expiry as a valid, non-regressing future
    // refresh rather than comparing the whole scope for equality.
    let scope2 = grant2.scope();
    assert_eq!(scope2.plugin_id, scope1.plugin_id, "plugin id preserved");
    assert_eq!(scope2.binding, scope1.binding, "pinned binding preserved");
    assert_eq!(scope2.method, scope1.method, "method preserved");
    assert_eq!(
        scope2.resource_namespace, scope1.resource_namespace,
        "resource namespace preserved"
    );
    assert_eq!(scope2.directive, scope1.directive, "directive preserved");
    assert!(
        scope2.expiry_unix_ms >= scope1.expiry_unix_ms,
        "restart refreshes the expiry without regressing it"
    );
    assert!(
        scope1.expiry_unix_ms > agalma_conductor::plugin_router::wall_clock_now_ms(),
        "the launch grant expiry stays in the future"
    );

    // The prior token is revoked; only the new token authorizes an effect.
    let stale = call(&grant1, PROD_PLUGIN_ID, 1, CAPABILITY, "op-stale", json!({}), FAR_FUTURE_MS);
    let error = host
        .router
        .router()
        .invoke_and_execute(&stale)
        .expect_err("revoked token is refused");
    assert_eq!(failure_code(&error), Some(FailureCode::AuthorityDenied));
    assert_eq!(call_count(&calls), 0, "revoked token had zero effects");

    let current = call(&grant2, PROD_PLUGIN_ID, 1, CAPABILITY, "op-current", json!({}), FAR_FUTURE_MS);
    host.router
        .router()
        .invoke_and_execute(&current)
        .expect("rotated token authorizes the effect");
    assert_eq!(call_count(&calls), 1);
}

/// Finding 5: the production host builds the hub capability service, issues a
/// per-launch scoped grant carried on the launch request, and routes an
/// authorized spoke callback end to end over the production hub socket with
/// injected adapter/journal/stop seams. Persistence stays volatile (HP.5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_host_serves_hub_capability_with_launch_grant() {
    let scratch = ScratchDir::new("hp-routes-production");
    let calls = Arc::new(Mutex::new(Vec::new()));
    // Assemble in a scope so the non-`Send` host is dropped before any await.
    let (hub_socket, router, grant) = {
        let mut host = assemble_production_host(&scratch, calls.clone());
        assert!(
            host.launch_grants().is_empty(),
            "assembly issues no grant; the launch epoch issues it"
        );
        // The launch request issues the per-launch grant and carries it on the
        // private launch channel, never through a report/receipt/log.
        let launch = host.launch_request(PROD_PLUGIN_ID).expect("launch request");
        let grant = host
            .launch_grant(PROD_PLUGIN_ID)
            .cloned()
            .expect("launch request issued a grant");
        assert_eq!(
            launch.env.get(PLUGIN_GRANT_ENV),
            Some(&grant.token_hex()),
            "grant token is passed to the child launch request"
        );
        assert!(launch.env.contains_key(HUB_SOCKET_ENV));
        (host.hub_socket.clone(), host.router.clone(), grant)
    };

    // Serve the production hub capability socket and call it back as a spoke.
    let serve_socket = hub_socket.clone();
    tokio::spawn(async move {
        let _ = agalma_conductor::plugin_router::serve_hub(&serve_socket, router).await;
    });
    wait_for_socket(&hub_socket);

    let request = call(&grant, PROD_PLUGIN_ID, 1, CAPABILITY, "op-prod", json!({ "n": 1 }), FAR_FUTURE_MS);
    let mut spoke = HubCallbackClient::connect(&hub_socket).await.expect("connect hub");
    let status = spoke
        .invoke_capability(serde_json::to_value(&request).unwrap())
        .await
        .expect("authorized production capability routes");
    assert_eq!(status["state"], json!("completed"));
    assert_eq!(call_count(&calls), 1);

    // An unknown token over the production socket has zero effects.
    let mut unauthorized = request.clone();
    unauthorized.grant = "00".repeat(32);
    let error = spoke
        .invoke_capability(serde_json::to_value(&unauthorized).unwrap())
        .await
        .expect_err("unknown grant refused");
    assert!(matches!(error, agalma_ipc::IpcError::Contract(_)));
    assert_eq!(call_count(&calls), 1, "unauthorized call had zero effects");
}

// ---------------------------------------------------------------------------
// HP.4 reopen: production RPC/runtime lifecycle repair (agalma-jmr.4)
// ---------------------------------------------------------------------------

/// Regression (`agalma-jmr.4` reopen): the production `agalma plugins --once`
/// host entry runs `run_plugin_host` (an `async fn` under `#[tokio::main]`) and
/// dropped its `RpcHealthProbe` — and the probe's owned multi-thread runtime —
/// from inside that async context, panicking at exit with code 101
/// ("Cannot drop a runtime in a context where blocking is not allowed").
/// Dropping the probe inside the async host entry must be safe, and its real
/// `plugin.Health` read must still report readiness truthfully.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_health_probe_survives_async_host_entry_drop() {
    let scratch = ScratchDir::new("hp-routes-probe-drop");

    // A real child bound at the probe's deterministic socket reports `ready`.
    let ready_id = "example.probe.ready";
    spawn_connect_server(
        &scratch,
        ready_id,
        plugin_server_info(ready_id, "1.2.3", Vec::<String>::new()),
        {
            let ready_id = ready_id.to_string();
            move |server| {
                register_plugin(server, Arc::new(FakePlugin::ready(&ready_id, 0)));
            }
        },
    );

    // Constructed, used, and dropped inside the async host-entry context — the
    // exact place the production runtime drop panicked.
    let mut probe = RpcHealthProbe::new(scratch.path().to_path_buf());
    assert!(
        probe.probe(ready_id, 0).expect("probe runs"),
        "real child health reads ready"
    );
    drop(probe);

    // A second cycle in the same context: an unserved socket is not Ready (never
    // fabricated) and dropping the fresh probe stays panic-free.
    let mut absent = RpcHealthProbe::new(scratch.path().to_path_buf());
    assert!(
        !absent.socket("example.probe.absent", 0).exists(),
        "the absent child serves no socket"
    );
    assert!(
        !absent.probe("example.probe.absent", 0).expect("probe runs"),
        "unserved child is never fabricated Ready"
    );
    drop(absent);
}

/// Property (`agalma-jmr.4` reopen): over a deterministic, seeded sequence of
/// host shutdown cycles, a `RpcHealthProbe` constructed and dropped inside the
/// async host-entry context never panics, and it reports Ready exactly for a
/// child actually serving its deterministic socket. Generated inputs vary the
/// probed plugin id and launch epoch; the served set is fixed independently of
/// the generator, so readiness is an independently defined oracle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn property_probe_drop_is_async_safe_and_readiness_is_exact() {
    let scratch = ScratchDir::new("hp-routes-probe-prop");

    // Two served children, both at launch epoch 0; any other (id, epoch) pair
    // resolves to a distinct, unserved socket.
    let served = ["example.prop.alpha", "example.prop.beta"];
    for id in served {
        spawn_connect_server(
            &scratch,
            id,
            plugin_server_info(id, "1.2.3", Vec::<String>::new()),
            {
                let id = id.to_string();
                move |server| {
                    register_plugin(server, Arc::new(FakePlugin::ready(&id, 0)));
                }
            },
        );
    }

    let candidates = [
        "example.prop.alpha",
        "example.prop.beta",
        "example.prop.gamma",
    ];
    let mut rng = XorShift64::new(PROPERTY_SEED);
    let mut ready_cycles: u64 = 0;
    let mut not_ready_cycles: u64 = 0;

    for case in 0..PROPERTY_CASES {
        let id = candidates[rng.below(candidates.len() as u64) as usize];
        let epoch = rng.below(4);
        let expected = served.contains(&id) && epoch == 0;

        // Reuse the served state root; the generator varies only the probed
        // target, so a genuine connect can occur for the served pairs.
        let mut probe = RpcHealthProbe::new(scratch.path().to_path_buf());
        let observed = probe
            .probe(id, epoch)
            .unwrap_or_else(|e| panic!("case {case}: probe {id}@{epoch} errored: {e:?}"));
        assert_eq!(
            observed, expected,
            "case {case}: readiness must match the independently served set"
        );
        assert_eq!(
            probe.socket(id, epoch).exists(),
            expected,
            "case {case}: socket presence tracks the served set"
        );
        // Dropping inside the async host-entry context must not panic.
        drop(probe);

        if expected {
            ready_cycles += 1;
        } else {
            not_ready_cycles += 1;
        }
    }

    assert!(
        ready_cycles > 0,
        "property exercised at least one ready read"
    );
    assert!(
        not_ready_cycles > 0,
        "property exercised at least one not-ready read"
    );
}

// ---------------------------------------------------------------------------
// HP.4 reopen: production plugin.Health → host pin → transient read model
// (`agalma-jmr.4` reopen acceptance)
// ---------------------------------------------------------------------------
//
// These cases drive the **real production path**: `PluginHost::assemble_with_ledger`
// (auto-install from the configured root, durable desired state, registry
// checkpoint and the selected-ledger [`ReportIngestor`]), the real supervisor
// spawn, the real [`RpcHealthProbe`] over Connect, the host-pinned `BindingPin`
// identity check, the transient `ReportIngestor::record_health` observation and
// the generic [`derive_plugin_components`] read model the UI renders. The only
// fixture is the child process: it is served at the supervisor's real launch
// socket by an in-process Connect peer, exactly as HP.8 proved is possible.

/// Recorded deterministic seed for the health-gate generated cases (replayable).
const HEALTH_PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_0044;
/// Recorded case budget for the health-gate generated cases.
const HEALTH_PROPERTY_CASES: u64 = 48;

/// Assemble the real production plugin host over a selected in-process ledger.
///
/// `assemble_with_ledger` auto-installs the configured package, attaches the
/// durable [`ReportIngestor`] and records the registry generations. Returns the
/// host and its attached read-model ingestor.
fn assemble_ledger_host(scratch: &ScratchDir) -> (PluginHost, Arc<Mutex<ReportIngestor>>) {
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    fs::create_dir_all(&root).unwrap();
    seal_production_package(&root, PROD_PLUGIN_ID);
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    let ledger = LedgerHandle::in_process(&state_dir).expect("in-process selected ledger");
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_000_000u64);
    let host = PluginHost::assemble_with_ledger(&config, &InMemoryInstallations::new(), ledger, clock)
        .expect("production host assembles over the selected ledger");
    let reports = host.reports.clone().expect("report ingestor attached");
    (host, reports)
}

/// Launch the auto-installed generation through the real supervisor (child
/// process + READY identity gate) and return the socket it must serve.
fn spawn_production_child(host: &mut PluginHost) -> PathBuf {
    let request = host.launch_request(PROD_PLUGIN_ID).expect("launch request");
    let socket = request.socket.clone();
    let spawned = host
        .supervisor
        .spawn(&mut host.registry, &request)
        .expect("real child spawns and passes the READY identity gate");
    assert_eq!(spawned.plugin_id, PROD_PLUGIN_ID);
    socket
}

/// Serve `socket` with a real Connect plugin peer whose `plugin.Health` returns
/// `health` (the child the supervisor's READY gate launched).
fn serve_child_health(socket: PathBuf, health: Value) {
    spawn_connect_server_at(
        socket,
        plugin_server_info(PROD_PLUGIN_ID, "1.2.3", [CAPABILITY.to_string()]),
        move |server| {
            register_plugin(
                server,
                Arc::new(FakePlugin::ready(PROD_PLUGIN_ID, 0).with_health(health)),
            );
        },
    );
}

/// The generic read-model health detail for `PROD_PLUGIN_ID`.
fn read_model_health(host: &PluginHost) -> Option<HealthView> {
    let reports = host.reports.as_ref().expect("report ingestor attached");
    let guard = reports.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    derive_plugin_components(&host.registry, Some(&guard))
        .into_iter()
        .find(|component| component.plugin_id == PROD_PLUGIN_ID)
        .and_then(|component| component.health)
}

/// One fail-closed production case: spawn the real child, optionally serve a
/// Connect peer, run the production health gate, and report the outcome plus the
/// generic read-model health state. The lifecycle must never become `Ready`.
fn run_health_case(
    label: &str,
    served: bool,
    build: impl FnOnce(&BindingPin) -> Value,
) -> (bool, String) {
    let scratch = ScratchDir::new(&format!("hp4-health-{label}"));
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let pinned = host.pinned_binding(PROD_PLUGIN_ID).expect("host pin");
    let socket = spawn_production_child(&mut host);
    if served {
        serve_child_health(socket, build(&pinned));
    }
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    let ready = host
        .confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
        .unwrap_or_else(|e| panic!("{label}: health gate errored: {e:?}"));
    assert_ne!(
        host.registry.entry(PROD_PLUGIN_ID).unwrap().observed,
        LifecycleState::Ready,
        "{label}: a non-pinned/failed probe must never mark the plugin Ready"
    );
    let state = read_model_health(&host)
        .unwrap_or_else(|| panic!("{label}: the generic read model shows a health observation"))
        .state;
    let _ = host.shutdown();
    (ready, state)
}

/// The reopen acceptance: a **running** plugin's real `plugin.Health` result is
/// identity-checked against the host-pinned `BindingPin` and appears as `ready`
/// health detail in the generic read model the UI renders.
#[test]
fn production_host_records_validated_health_in_generic_read_model() {
    let scratch = ScratchDir::new("hp4-health-ready");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let pinned = host.pinned_binding(PROD_PLUGIN_ID).expect("host pin");
    let socket = spawn_production_child(&mut host);
    serve_child_health(
        socket,
        plugin_health_value(PROD_PLUGIN_ID, &pinned, HealthState::Ready, "serving"),
    );

    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    assert!(
        host.confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
            .expect("production health gate runs"),
        "a validated ready observation confirms readiness"
    );
    assert_eq!(
        host.registry.entry(PROD_PLUGIN_ID).unwrap().observed,
        LifecycleState::Ready,
        "readiness is recorded only after the health gate passed"
    );

    let health = read_model_health(&host).expect("generic read model shows health detail");
    assert_eq!(health.state, "ready");
    assert_eq!(health.detail, "serving");
    assert_eq!(health.observed_at_unix_ms, 1_000_000, "hub observation time");

    let _ = host.shutdown();
}

/// Fail-closed: malformed, wrong-plugin, wrong-generation, stale-binding and
/// transport-failed probes never display `ready`; a truthfully degraded peer is
/// shown `degraded` and is not Ready.
#[test]
fn production_host_health_fails_closed_for_mismatched_or_failed_probes() {
    // Wrong generation: the pin's generation does not match the peer's record.
    let (ready, state) = run_health_case("wrong-generation", true, |pin| {
        let mut wrong = pin.clone();
        wrong.generation += 1;
        plugin_health_value(PROD_PLUGIN_ID, &wrong, HealthState::Ready, "forged")
    });
    assert!(!ready, "wrong-generation probe is never ready");
    assert_eq!(state, "unavailable");

    // Wrong plugin: the peer names another plugin id.
    let (ready, state) = run_health_case("wrong-plugin", true, |pin| {
        plugin_health_value("example.other", pin, HealthState::Ready, "forged")
    });
    assert!(!ready, "wrong-plugin probe is never ready");
    assert_eq!(state, "unavailable");

    // Stale binding: same plugin/generation but a changed configuration digest.
    let (ready, state) = run_health_case("stale-binding", true, |pin| {
        let mut stale = pin.clone();
        stale.config_digest = format!("sha256:{}", "b".repeat(64));
        plugin_health_value(PROD_PLUGIN_ID, &stale, HealthState::Ready, "forged")
    });
    assert!(!ready, "stale-binding probe is never ready");
    assert_eq!(state, "unavailable");

    // Malformed: not a well-formed `PluginHealth/v1` at all.
    let (ready, state) = run_health_case("malformed", true, |_pin| json!({ "state": "ready" }));
    assert!(!ready, "malformed probe is never ready");
    assert_eq!(state, "unavailable");

    // Transport failure: no peer serves the socket.
    let (ready, state) = run_health_case("transport", false, |_pin| Value::Null);
    assert!(!ready, "transport-failed probe is never ready");
    assert_eq!(state, "unavailable");

    // Correct identity, genuinely degraded: shown truthfully, never ready.
    let (ready, state) = run_health_case("degraded", true, |pin| {
        plugin_health_value(PROD_PLUGIN_ID, pin, HealthState::Degraded, "impaired")
    });
    assert!(!ready, "a degraded peer is not Ready");
    assert_eq!(state, "degraded");
}

/// A scripted probe returning one preset result per call (the generated cases
/// drive the production host gate without a socket).
struct ScriptedHealthProbe {
    result: HealthProbeResult,
}

impl HealthProbe for ScriptedHealthProbe {
    fn probe(&mut self, _plugin_id: &str, _epoch: u64) -> Result<bool, ContractError> {
        Ok(self.result.ready)
    }

    fn probe_health(
        &mut self,
        _plugin_id: &str,
        _epoch: u64,
    ) -> Result<HealthProbeResult, ContractError> {
        Ok(self.result.clone())
    }
}

/// Property: over a deterministic, seeded sequence of generated
/// `plugin.Health` identity/state cases (valid and invalid), the production host
/// records `ready` in the generic read model **exactly** for a well-formed
/// `ready` observation of the host-pinned plugin/id/generation/config digest,
/// and never otherwise.
#[test]
fn property_health_gate_records_ready_exactly_for_the_pinned_identity() {
    let scratch = ScratchDir::new("hp4-health-prop");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let pinned = host.pinned_binding(PROD_PLUGIN_ID).expect("host pin");
    let _socket = spawn_production_child(&mut host);

    let identities = [PROD_PLUGIN_ID, "example.other"];
    let states = [HealthState::Ready, HealthState::Degraded, HealthState::Unavailable];
    let mut rng = XorShift64::new(HEALTH_PROPERTY_SEED);
    let mut ready_cases: u64 = 0;
    let mut not_ready_cases: u64 = 0;

    for case in 0..HEALTH_PROPERTY_CASES {
        let id = identities[rng.below(identities.len() as u64) as usize];
        let generation = if rng.below(2) == 0 {
            pinned.generation
        } else {
            pinned.generation + 1
        };
        let config_digest = if rng.below(2) == 0 {
            pinned.config_digest.clone()
        } else {
            format!("sha256:{}", "c".repeat(64))
        };
        let state = states[rng.below(states.len() as u64) as usize];
        let malformed = rng.below(5) == 0;

        let mut binding = pinned.clone();
        binding.generation = generation;
        binding.config_digest = config_digest;
        let expected_ready =
            !malformed && id == PROD_PLUGIN_ID && binding == pinned && state == HealthState::Ready;

        let result = if malformed {
            // A malformed wire record yields no observation and no readiness.
            HealthProbeResult {
                ready: false,
                observation: None,
                detail: "malformed plugin.Health".to_string(),
            }
        } else {
            HealthProbeResult {
                ready: state == HealthState::Ready,
                observation: Some(PluginHealth {
                    format: PLUGIN_HEALTH_FORMAT.to_string(),
                    plugin_id: id.to_string(),
                    binding,
                    state,
                    detail: "probe".to_string(),
                    checked_at_unix_ms: 0,
                }),
                detail: "probe".to_string(),
            }
        };

        let mut probe = ScriptedHealthProbe { result };
        let ready = host
            .confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
            .unwrap_or_else(|e| panic!("case {case}: gate errored: {e:?}"));
        assert_eq!(
            ready, expected_ready,
            "case {case}: readiness follows the host-pinned identity exactly"
        );
        let health = read_model_health(&host).expect("case: read model shows health");
        if expected_ready {
            assert_eq!(health.state, "ready", "case {case}");
            ready_cases += 1;
        } else {
            assert_ne!(
                health.state, "ready",
                "case {case}: only the pinned identity/state may ever display ready"
            );
            not_ready_cases += 1;
        }
    }

    assert!(ready_cases > 0, "generated at least one pinned ready case");
    assert!(
        not_ready_cases > 0,
        "generated at least one refused case"
    );
    let _ = host.shutdown();
}

/// Stale launch epoch: a health observation from a superseded epoch is fenced
/// and never establishes readiness, while the live epoch's observation applies.
#[test]
fn stale_launch_epoch_health_observation_never_marks_ready() {
    let scratch = ScratchDir::new("hp4-health-epoch");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let generation = host.pinned_binding(PROD_PLUGIN_ID).expect("pin").generation;

    // Launch (epoch 1) then simulate a restart's new launch epoch (epoch 2).
    let first_epoch = host
        .registry
        .mark_starting(PROD_PLUGIN_ID, generation, 1_000_000)
        .expect("first launch epoch");
    host.registry
        .mark_failed(PROD_PLUGIN_ID, "crashed", 1_000_000)
        .expect("record crash");
    let live_epoch = host
        .registry
        .mark_starting(PROD_PLUGIN_ID, generation, 2_000_000)
        .expect("restart launch epoch");
    assert!(live_epoch > first_epoch, "restart advances the launch epoch");

    // A stale-epoch observation is fenced: it never establishes readiness.
    assert!(
        !host
            .supervisor
            .observe_health(
                &mut host.registry,
                PROD_PLUGIN_ID,
                first_epoch,
                true,
                "stale process reported ready"
            )
            .expect("fenced observation applies no state"),
        "a superseded launch epoch's observation is refused"
    );
    assert_ne!(
        host.registry.entry(PROD_PLUGIN_ID).unwrap().observed,
        LifecycleState::Ready,
        "a stale-epoch observation never marks the plugin Ready"
    );

    // The live epoch's observation applies and marks it Ready.
    assert!(
        host.supervisor
            .observe_health(
                &mut host.registry,
                PROD_PLUGIN_ID,
                live_epoch,
                true,
                "live process reported ready"
            )
            .expect("live observation applies"),
        "the live launch epoch's observation applies"
    );
    assert_eq!(
        host.registry.entry(PROD_PLUGIN_ID).unwrap().observed,
        LifecycleState::Ready
    );
}

// ---------------------------------------------------------------------------
// HP.4 reopen repair (review findings): owned-launch identity pin, staged
// candidate preservation, and ingestion-refusal fail-closed
// ---------------------------------------------------------------------------

/// Recorded deterministic seed for the owned-launch health-gate property.
const HEALTH_LAUNCH_PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_0045;

/// The frozen pin for one installed generation (mirrors the host's construction:
/// `plugin_binding` in `composition.rs`).
fn installed_binding(host: &PluginHost, plugin_id: &str, generation: u64) -> BindingPin {
    let installed = host
        .registry
        .entry(plugin_id)
        .expect("registry entry")
        .generations
        .get(&generation)
        .expect("installed generation");
    BindingPin {
        binding_id: plugin_id.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: installed.package_version.clone(),
        generation,
        config_digest: installed.config_digest.clone(),
    }
}

/// A valid `PluginInstallation/v1` pin over `package`'s sealed default config.
fn default_pin(package: &DiscoveredPackage) -> PluginInstallation {
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

/// Install and stage a newer candidate generation for the running plugin.
fn stage_new_candidate(host: &mut PluginHost, scratch: &ScratchDir, version: &str) -> u64 {
    let root = scratch.child(&format!("candidate-{version}"));
    fs::create_dir_all(&root).unwrap();
    let package = seal_production_package_version(&root, PROD_PLUGIN_ID, version);
    let pin = default_pin(&package);
    let generation = host
        .registry
        .install(&host.install_store, &package, &pin)
        .expect("install candidate generation");
    host.registry
        .stage(PROD_PLUGIN_ID, generation)
        .expect("stage candidate generation");
    generation
}

/// Finding 1: a newer candidate staged while the running generation is healthy
/// must not reject the running generation, must not accept the running
/// generation claiming the candidate, and must preserve the staged candidate.
#[test]
fn health_gate_uses_owned_launch_pin_and_preserves_staged_candidate() {
    let scratch = ScratchDir::new("hp4-health-staged");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let g1 = host.pinned_binding(PROD_PLUGIN_ID).expect("pin").generation;
    let socket = spawn_production_child(&mut host);

    // A newer candidate is staged while g1 runs.
    let g2 = stage_new_candidate(&mut host, &scratch, "1.2.4");
    assert_ne!(g2, g1, "a replacement seals a distinct generation");
    {
        let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
        assert_eq!(entry.active_generation, None, "g1 activates only after health");
        assert_eq!(entry.pending_generation, Some(g2), "g2 is the staged candidate");
    }

    // The running child reports g1's own pin: confirmed against the owned launch,
    // not the staged candidate.
    let g1_pin = installed_binding(&host, PROD_PLUGIN_ID, g1);
    serve_child_health(
        socket,
        plugin_health_value(PROD_PLUGIN_ID, &g1_pin, HealthState::Ready, "serving"),
    );
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    assert!(
        host.confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
            .expect("owned-launch gate runs"),
        "a healthy running generation is confirmed against its own launch pin"
    );

    let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
    assert_eq!(entry.active_generation, Some(g1), "the running generation activates");
    assert_eq!(
        entry.pending_generation,
        Some(g2),
        "the unrelated staged candidate is preserved, not cleared"
    );
    assert_eq!(entry.observed, LifecycleState::Ready);
    let health = read_model_health(&host).expect("health visible");
    assert_eq!(health.state, "ready");
    let _ = host.shutdown();
}

/// Finding 1: the running launch claiming the staged candidate's identity is
/// refused (never accepted, never activated), and the candidate is preserved.
#[test]
fn health_gate_refuses_a_launch_claiming_the_staged_candidate() {
    let scratch = ScratchDir::new("hp4-health-claim-candidate");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let socket = spawn_production_child(&mut host);
    let g2 = stage_new_candidate(&mut host, &scratch, "1.2.4");

    // The owned launch is g1, but the peer claims g2.
    let g2_pin = installed_binding(&host, PROD_PLUGIN_ID, g2);
    serve_child_health(
        socket,
        plugin_health_value(PROD_PLUGIN_ID, &g2_pin, HealthState::Ready, "forged"),
    );
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    assert!(
        !host
            .confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
            .expect("gate runs"),
        "a peer claiming a generation other than the owned launch is refused"
    );

    let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
    assert_eq!(entry.active_generation, None, "the running generation is not activated");
    assert_eq!(entry.pending_generation, Some(g2), "the staged candidate is preserved");
    assert_ne!(entry.observed, LifecycleState::Ready);
    assert_eq!(read_model_health(&host).expect("health visible").state, "unavailable");
    let _ = host.shutdown();
}

/// Finding 1: a superseded launch epoch is refused before activation (the owned
/// child is no longer the registry's current launch).
#[test]
fn health_gate_refuses_a_superseded_launch_epoch() {
    let scratch = ScratchDir::new("hp4-health-superseded-epoch");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let pinned = host.pinned_binding(PROD_PLUGIN_ID).expect("pin");
    let socket = spawn_production_child(&mut host);

    // The registry advances to a new launch epoch (a restart the supervisor does
    // not own): the owned child is superseded.
    host.registry
        .mark_starting(PROD_PLUGIN_ID, pinned.generation, 2_000_000)
        .expect("advance the launch epoch");

    serve_child_health(
        socket,
        plugin_health_value(PROD_PLUGIN_ID, &pinned, HealthState::Ready, "serving"),
    );
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    let err = host
        .confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
        .expect_err("a superseded launch epoch refuses before activation");
    assert_eq!(failure_code(&err), Some(FailureCode::StaleGeneration));
    assert_ne!(
        host.registry.entry(PROD_PLUGIN_ID).unwrap().observed,
        LifecycleState::Ready
    );
    let _ = host.shutdown();
}

/// Finding 2: a refused report ingestion fails the gate before lifecycle
/// activation — a rejected observation can never establish Ready.
#[test]
fn health_gate_refuses_activation_when_ingestion_is_rejected() {
    let scratch = ScratchDir::new("hp4-health-ingest-reject");
    let mut host = assemble_production_host(&scratch, Arc::new(Mutex::new(Vec::new())));

    // Attach an ingestor with **no view** for this plugin: `record_health`
    // refuses the observation (the plugin is not registered in the read model).
    let ledger = SqliteLedger::open(scratch.path().join("reports.sqlite")).expect("open store");
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_000_000u64);
    let ingestor = ReportIngestor::open(
        Box::new(Mutex::new(ledger)),
        clock,
        BTreeMap::new(),
        std::collections::BTreeSet::new(),
    )
    .expect("open ingestor");
    let reports = Arc::new(Mutex::new(ingestor));
    host.router.router().install_reports(Arc::clone(&reports));
    host.reports = Some(reports);

    let pin = host.pinned_binding(PROD_PLUGIN_ID).expect("pin");
    let socket = spawn_production_child(&mut host);
    serve_child_health(
        socket,
        plugin_health_value(PROD_PLUGIN_ID, &pin, HealthState::Ready, "serving"),
    );

    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    let err = host
        .confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
        .expect_err("a refused ingestion refuses activation");
    assert_eq!(
        failure_code(&err),
        Some(FailureCode::InvalidRecord),
        "the refused ingestion is propagated as a typed failure"
    );
    let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
    assert_ne!(
        entry.observed,
        LifecycleState::Ready,
        "a rejected ingestion cannot establish Ready"
    );
    assert_eq!(entry.active_generation, None, "no activation on rejected ingestion");
    let _ = host.shutdown();
}

/// Property: over a deterministic, seeded sequence of generated identity/state
/// cases against a running launch `g1` with an unrelated candidate `g2` staged,
/// the production gate is Ready **exactly** for a well-formed `ready` record of
/// the owned launch pin `g1`, and the staged candidate `g2` is always preserved.
#[test]
fn property_health_gate_is_pinned_to_the_owned_launch_and_preserves_candidates() {
    let scratch = ScratchDir::new("hp4-health-launch-prop");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let g1 = host.pinned_binding(PROD_PLUGIN_ID).expect("pin").generation;
    let _socket = spawn_production_child(&mut host);
    let g2 = stage_new_candidate(&mut host, &scratch, "1.2.4");
    let g1_pin = installed_binding(&host, PROD_PLUGIN_ID, g1);
    let g2_pin = installed_binding(&host, PROD_PLUGIN_ID, g2);

    let states = [HealthState::Ready, HealthState::Degraded, HealthState::Unavailable];
    let mut rng = XorShift64::new(HEALTH_LAUNCH_PROPERTY_SEED);
    let mut ready_cases: u64 = 0;
    let mut refused_cases: u64 = 0;

    for case in 0..HEALTH_PROPERTY_CASES {
        // Keep the unrelated candidate staged for every case.
        host.registry.stage(PROD_PLUGIN_ID, g2).expect("re-stage candidate");

        let claims_launch = rng.below(2) == 0;
        let id = if rng.below(4) == 0 { "example.other" } else { PROD_PLUGIN_ID };
        let state = states[rng.below(states.len() as u64) as usize];
        let malformed = rng.below(5) == 0;
        let binding = if claims_launch { g1_pin.clone() } else { g2_pin.clone() };
        let expected_ready =
            !malformed && id == PROD_PLUGIN_ID && claims_launch && state == HealthState::Ready;

        let result = if malformed {
            HealthProbeResult {
                ready: false,
                observation: None,
                detail: "malformed plugin.Health".to_string(),
            }
        } else {
            HealthProbeResult {
                ready: state == HealthState::Ready,
                observation: Some(PluginHealth {
                    format: PLUGIN_HEALTH_FORMAT.to_string(),
                    plugin_id: id.to_string(),
                    binding,
                    state,
                    detail: "probe".to_string(),
                    checked_at_unix_ms: 0,
                }),
                detail: "probe".to_string(),
            }
        };

        let mut probe = ScriptedHealthProbe { result };
        let ready = host
            .confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
            .unwrap_or_else(|e| panic!("case {case}: gate errored: {e:?}"));
        assert_eq!(
            ready, expected_ready,
            "case {case}: readiness is pinned to the owned launch g1"
        );
        let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
        assert_eq!(
            entry.pending_generation,
            Some(g2),
            "case {case}: the unrelated staged candidate is preserved"
        );
        if expected_ready {
            assert_eq!(entry.active_generation, Some(g1), "case {case}");
            ready_cases += 1;
        } else {
            refused_cases += 1;
        }
    }

    assert!(ready_cases > 0, "generated at least one owned-launch ready case");
    assert!(refused_cases > 0, "generated at least one refused case");
    let _ = host.shutdown();
}

/// Review defect: readying the running generation `g1` must preserve a staged
/// candidate `g2` **without revalidating its package bytes**. `PluginRegistry::mark_ready`
/// is authoritative and touches no package I/O, so a candidate whose sealed
/// bytes were damaged after staging is preserved rather than discarded (or the
/// transition failing) after `g1` became Ready.
#[test]
fn ready_transition_preserves_a_damaged_staged_candidate_without_revalidation() {
    let scratch = ScratchDir::new("hp4-health-damaged-candidate");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let g1 = host.pinned_binding(PROD_PLUGIN_ID).expect("pin").generation;
    let socket = spawn_production_child(&mut host);
    let g2 = stage_new_candidate(&mut host, &scratch, "1.2.4");

    // Damage g2's sealed bytes after staging: `stage`/rehash on g2 would now
    // fail, so no correct repair may depend on re-staging it.
    let candidate_dir = host
        .registry
        .entry(PROD_PLUGIN_ID)
        .expect("entry")
        .generations
        .get(&g2)
        .expect("installed candidate")
        .installed_dir
        .clone();
    let entrypoint = candidate_dir.join("bin/plugin");
    let mut damaged = fs::read(&entrypoint).expect("read candidate entrypoint");
    damaged.extend_from_slice(b"\n# damaged after staging\n");
    fs::write(&entrypoint, damaged).expect("damage candidate bytes");

    // The running g1 is healthy: confirming it must activate g1 without touching
    // the damaged candidate.
    let g1_pin = installed_binding(&host, PROD_PLUGIN_ID, g1);
    serve_child_health(
        socket,
        plugin_health_value(PROD_PLUGIN_ID, &g1_pin, HealthState::Ready, "serving"),
    );
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    assert!(
        host.confirm_ready_health(PROD_PLUGIN_ID, &mut probe)
            .expect("readying g1 cannot fail on the damaged candidate"),
        "a healthy running generation is confirmed"
    );

    let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
    assert_eq!(entry.active_generation, Some(g1), "g1 is active");
    assert_eq!(entry.observed, LifecycleState::Ready, "g1 is Ready");
    assert_eq!(
        entry.pending_generation,
        Some(g2),
        "the damaged staged candidate is preserved (no revalidation I/O)"
    );
    let _ = host.shutdown();
}

/// The Ready transition clears only the candidate that is itself becoming
/// active; a different pending generation is preserved.
#[test]
fn ready_transition_clears_only_the_activated_candidate() {
    let scratch = ScratchDir::new("hp4-registry-ready-pending");
    let (mut host, _reports) = assemble_ledger_host(&scratch);
    let g1 = host.pinned_binding(PROD_PLUGIN_ID).expect("pin").generation;
    let g2 = stage_new_candidate(&mut host, &scratch, "1.2.4");

    // Activating g2 clears its own staged candidate.
    host.registry
        .mark_ready(PROD_PLUGIN_ID, g2, 5_000_000)
        .expect("activate g2");
    {
        let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
        assert_eq!(entry.active_generation, Some(g2));
        assert_eq!(entry.pending_generation, None, "the activated candidate is cleared");
    }

    // Ready-staging g1 (now the candidate) clears it too, because it is the
    // generation becoming active.
    host.registry.stage(PROD_PLUGIN_ID, g1).expect("stage g1");
    host.registry
        .mark_ready(PROD_PLUGIN_ID, g1, 6_000_000)
        .expect("activate g1");
    let entry = host.registry.entry(PROD_PLUGIN_ID).unwrap();
    assert_eq!(entry.active_generation, Some(g1));
    assert_eq!(entry.pending_generation, None, "the activated candidate itself is cleared");
    let _ = host.shutdown();
}

/// WEB.11 (`agalma-rwn.11`): `factory.read/v1` and `factory.control/v1` are
/// validated and invoked directly under a stopped factory, while an ordinary
/// effect capability keeps the fail-closed stop refusal and is neither
/// journaled nor invoked.
#[test]
fn factory_capabilities_are_direct_and_stop_exempt() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let read_grant = invoke_grant(PLUGIN_A, 1, FACTORY_READ_CAPABILITY, FAR_FUTURE_MS);
    let control_grant = invoke_grant(PLUGIN_A, 1, FACTORY_CONTROL_CAPABILITY, FAR_FUTURE_MS);
    let normal_grant = invoke_grant(PLUGIN_A, 1, CAPABILITY, FAR_FUTURE_MS);

    let mut router = CapabilityRouter::new(
        Box::new(InMemoryOperationJournal::new()),
        Arc::new(FakeStop(StopDecision::Stopped("latch".to_string()))) as Arc<dyn StopAdmission>,
    );
    for capability in [FACTORY_READ_CAPABILITY, FACTORY_CONTROL_CAPABILITY, CAPABILITY] {
        router.register_adapter(RecordingAdapter {
            capability: capability.to_string(),
            calls: calls.clone(),
            response: json!({ "ok": true }),
        });
    }
    router.install_grant(&read_grant);
    router.install_grant(&control_grant);
    router.install_grant(&normal_grant);
    let service = RouterService::new(router);

    let read_call = call(
        &read_grant,
        PLUGIN_A,
        1,
        FACTORY_READ_CAPABILITY,
        "fd-read",
        json!({ "format": "FactoryRuntime/v1", "operation": "snapshot" }),
        FAR_FUTURE_MS,
    );
    service
        .invoke_capability(serde_json::to_value(&read_call).unwrap())
        .expect("factory.read is admitted while the factory is stopped");

    let control_call = call(
        &control_grant,
        PLUGIN_A,
        1,
        FACTORY_CONTROL_CAPABILITY,
        "fd-control",
        json!({
            "format": "FactoryControl/v1",
            "operation": "pause",
            "action_id": "op-fd-1",
            "confirmation": null,
        }),
        FAR_FUTURE_MS,
    );
    service
        .invoke_capability(serde_json::to_value(&control_call).unwrap())
        .expect("factory.control is admitted while the factory is stopped");

    let normal_call = call(
        &normal_grant,
        PLUGIN_A,
        1,
        CAPABILITY,
        "fd-normal",
        json!({ "k": 1 }),
        FAR_FUTURE_MS,
    );
    let error = service
        .invoke_capability(serde_json::to_value(&normal_call).unwrap())
        .expect_err("an ordinary effect is still refused while stopped");
    assert_eq!(failure_code(&error), Some(FailureCode::Stopped));

    assert_eq!(
        call_count(&calls),
        2,
        "only the direct factory capabilities were invoked"
    );
    assert_eq!(
        service.router().journaled_operations(),
        0,
        "the factory direct path never touches the effect journal"
    );
}

