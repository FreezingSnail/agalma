//! HP.3 plugin registry, supervisor and production host acceptance tests
//! (`agalma-jmr.3`).
//!
//! Each case builds independent sealed-package fixtures and real `/bin/sh` fake
//! children under the persistent scratch root (no `/tmp`, no network, no paid
//! provider call). The lifecycle cases drive the common supervisor against those
//! fake children so startup failure cleanup, crash/backoff/restart and the
//! `READY` compatibility gate are exercised with real process groups; the
//! injected [`ManualClock`] proves health-gated readiness, launch-epoch fencing
//! and backoff without sleeping; and the property tests use a recorded
//! deterministic seed and case budget.
//!
//! Behavioral coverage: `READY` success/timeout/wrong identity; startup failure
//! cleanup (no leaked process group); crash/backoff/restart with a fresh launch
//! epoch and the same pinned generation; dependency unavailability; monotonic
//! generations; drain/removal refusal with unresolved pins; immutable installed
//! packages unaffected by later discovery-directory edits; SUN_LEN-safe socket
//! paths; desired-state persistence through the injected interface; built-in
//! component representation; and the production host entry/lifecycle wiring
//! (with the HP.4 RPC and HP.5 persistence attachments left pending).
//!
//! Not covered here: real plugin RPC (HP.4) and durable selected-ledger
//! persistence/reporting (HP.5); see the pending-attachment notes on
//! [`production_host_entry_assembles_and_starts_without_paid_task`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use serde_json::{json, Value};

use agalma_conductor::composition::{InMemoryInstallations, PluginHost};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveredPackage, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_registry::{
    plugin_socket_path, DesiredState, DesiredStateStore, HubInstallStore, InMemoryDesiredStateStore,
    InstalledGeneration, LifecycleState, PluginRegistry, SOCKET_PATH_LIMIT,
};
use agalma_conductor::plugin_supervisor::{
    BackoffPolicy, Clock, HealthProbe, LaunchRequest, ManualClock, PluginSupervisor,
    PLUGIN_BINDING_GENERATION_ENV, PLUGIN_CONFIG_DIGEST_ENV, PLUGIN_CONFIG_PATH_ENV,
};
use agalma_contracts::digest::{failure_code, sha256_digest, FailureCode};
use agalma_contracts::{ContractError, PluginInstallation, PluginManifest};
use agalma_testkit::ScratchDir;

/// Recorded deterministic property-test seed (xorshift64).
const PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_0001;
/// Recorded property-test case budget per property.
const PROPERTY_CASES: u64 = 24;
/// WEB.4 launcher property seed; retained verbatim for replay.
const LAUNCH_PROPERTY_SEED: u64 = 0x5745_4234;
/// WEB.4 launcher property budget.
const LAUNCH_PROPERTY_CASES: u64 = 64;

const CONFIG_SCHEMA_BYTES: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
const DEFAULT_CONFIG_BYTES: &[u8] = b"{}\n";
const REPORT_SCHEMA_BYTES: &[u8] = b"{\"type\":\"object\",\"required\":[\"ready\"]}\n";

/// The plugin id used by every standard fixture.
const PLUGIN_ID: &str = "example.reporter";

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

/// Deterministic xorshift64 generator so a property failure replays exactly.
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

/// A `PluginManifest/v1` whose artifact references match [`standard_files`].
fn base_manifest(plugin_id: &str) -> Value {
    json!({
        "format": "PluginManifest/v1",
        "plugin_id": plugin_id,
        "package_version": "1.2.3",
        "package_digest": format!("sha256:{}", "0".repeat(64)),
        "protocol_major": 1,
        "entrypoint": {
            "path": "bin/plugin",
            "argv": ["--serve", "--read-only"],
            "launch_profile": "reporter-read-only/v1"
        },
        "domain_apis": [],
        "requested_capabilities": ["report.publish"],
        "required_services": [],
        "optional_services": [],
        "config_schema": byte_ref("objects/config-schema.json", CONFIG_SCHEMA_BYTES, "application/json"),
        "default_configuration": byte_ref("objects/default-config.json", DEFAULT_CONFIG_BYTES, "application/json"),
        "report_schemas": [{
            "namespace": "example.reporter.status",
            "version": 1,
            "schema": byte_ref("objects/report-schema.json", REPORT_SCHEMA_BYTES, "application/json")
        }],
        "views": [{
            "view_id": "status",
            "schema_namespace": "example.reporter.status",
            "kind": "Status",
            "title": "Reporter status",
            "schema_version": 1,
            "fields": ["ready"]
        }],
        "actions": [],
        "state_compatibility": {"reads": ["reporter-state/v1"], "writes": "reporter-state/v1"}
    })
}

fn byte_ref(path: &str, bytes: &[u8], media_type: &str) -> Value {
    json!({
        "path": path,
        "digest": sha256_digest(bytes),
        "bytes": bytes.len() as u64,
        "media_type": media_type,
    })
}

/// The non-entrypoint files every fixture package ships.
fn support_files() -> Vec<(String, Vec<u8>)> {
    vec![
        (
            "objects/config-schema.json".to_string(),
            CONFIG_SCHEMA_BYTES.to_vec(),
        ),
        (
            "objects/default-config.json".to_string(),
            DEFAULT_CONFIG_BYTES.to_vec(),
        ),
        (
            "objects/report-schema.json".to_string(),
            REPORT_SCHEMA_BYTES.to_vec(),
        ),
    ]
}

/// A `READY` line for `impl_name` announcing `capabilities`.
fn ready_line(impl_name: &str, capabilities: &[&str]) -> String {
    let caps: Vec<String> = capabilities.iter().map(|c| (*c).to_string()).collect();
    format!(
        "READY {}\n",
        serde_json::to_string(&json!({
            "api": "plugin",
            "api_version": 1,
            "impl": impl_name,
            "impl_version": "1.2.3",
            "capabilities": caps,
        }))
        .unwrap()
    )
}

/// Write the package files and seal the manifest with its inventory digest.
///
/// The entrypoint script is chmod `0o755` **before** sealing so the digest
/// covers the executable mode the hub will install and later exec.
fn seal_package(
    package_dir: &Path,
    manifest: &Value,
    entrypoint_script: &str,
) -> PluginManifest {
    let mut files = support_files();
    files.push(("bin/plugin".to_string(), entrypoint_script.as_bytes().to_vec()));
    write_files(package_dir, &files);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let entry = package_dir.join("bin/plugin");
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let parsed: PluginManifest = serde_json::from_value(manifest.clone()).unwrap();
    let digest = package_inventory_digest(package_dir, &parsed).unwrap();
    let mut sealed = manifest.clone();
    sealed["package_digest"] = Value::String(digest);
    fs::write(
        package_dir.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&sealed).unwrap(),
    )
    .unwrap();
    serde_json::from_value(sealed).unwrap()
}

fn write_files(package_dir: &Path, files: &[(String, Vec<u8>)]) {
    fs::create_dir_all(package_dir).unwrap();
    for (relative, bytes) in files {
        let path = package_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();
    }
}

/// A fake child that announces `impl_name` and then stays alive until its stdin
/// closes (i.e. until the supervisor tears the group down).
fn live_child(impl_name: &str, capabilities: &[&str]) -> String {
    format!(
        "#!/bin/sh\nprintf '{}'\ncat >/dev/null\n",
        ready_line(impl_name, capabilities)
    )
}

/// A fake child that records its pid, announces `impl_name`, and stays alive.
fn pid_recording_child(pidfile: &Path, impl_name: &str, capabilities: &[&str]) -> String {
    format!(
        "#!/bin/sh\necho $$ > '{}'\nprintf '{}'\ncat >/dev/null\n",
        pidfile.display(),
        ready_line(impl_name, capabilities)
    )
}

/// A fake child that records its pid and never announces READY (times out).
fn pid_recording_silent_child(pidfile: &Path) -> String {
    format!(
        "#!/bin/sh\necho $$ > '{}'\nsleep 30\n",
        pidfile.display()
    )
}

/// Discover the single sealed package under a fresh root.
fn discover_one(root: &Path, plugin_id: &str, script: &str) -> DiscoveredPackage {
    let package_dir = root.join("pkg");
    seal_package(&package_dir, &base_manifest(plugin_id), script);
    let report = discover_plugins(&[PluginRoot::required(root)]).unwrap();
    report.package(plugin_id).unwrap().clone()
}

/// Install, stage and activate `package`, returning its generation.
fn install_active(
    registry: &mut PluginRegistry,
    store: &HubInstallStore,
    package: &DiscoveredPackage,
) -> u64 {
    let installation = installation_for(package);
    let generation = registry.install(store, package, &installation).unwrap();
    registry.stage(&package.plugin_id, generation).unwrap();
    registry.activate(&package.plugin_id, generation).unwrap();
    generation
}

fn installed(
    registry: &PluginRegistry,
    plugin_id: &str,
    generation: u64,
) -> InstalledGeneration {
    registry
        .entry(plugin_id)
        .unwrap()
        .generations
        .get(&generation)
        .unwrap()
        .clone()
}

fn code(err: &ContractError) -> FailureCode {
    failure_code(err).expect("HP.3 failures carry a structured code")
}

fn message(err: &ContractError) -> String {
    format!("{err:?}")
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// A scripted, recording health probe.
struct ScriptedProbe {
    healthy: bool,
    calls: RefCell<Vec<(String, u64)>>,
}

impl ScriptedProbe {
    fn healthy(healthy: bool) -> Self {
        ScriptedProbe {
            healthy,
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl HealthProbe for ScriptedProbe {
    fn probe(&mut self, plugin_id: &str, epoch: u64) -> Result<bool, ContractError> {
        self.calls
            .borrow_mut()
            .push((plugin_id.to_string(), epoch));
        Ok(self.healthy)
    }
}

fn supervisor(clock: Rc<ManualClock>, ready_timeout: Duration) -> PluginSupervisor {
    PluginSupervisor::new(
        clock,
        BackoffPolicy {
            base_ms: 100,
            factor: 2,
            max_ms: 1_000,
        },
        ready_timeout,
    )
}

fn request_for(
    plugin_id: &str,
    installed: &InstalledGeneration,
    socket: PathBuf,
) -> LaunchRequest {
    let mut request = LaunchRequest::from_installed(installed, socket);
    request.plugin_id = plugin_id.to_string();
    request
}

/// A valid `PluginInstallation/v1` pinning `package`.
fn installation_for(package: &DiscoveredPackage) -> PluginInstallation {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: package.plugin_id.clone(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: package.manifest.requested_capabilities.clone(),
        installation_digest: String::new(),
    };
    reseal_installation(&mut installation);
    installation
}

/// Recompute an installation's digest after a mutation (as an operator tool
/// would), so a tampered record still passes the digest gate and reaches the
/// deeper package/scope checks.
fn reseal_installation(installation: &mut PluginInstallation) {
    installation.installation_digest = installation.recomputed_installation_digest().unwrap();
}

/// The default in-memory desired-state store for host assembly.
fn default_desired() -> Box<dyn DesiredStateStore> {
    Box::new(InMemoryDesiredStateStore::new())
}

/// Assert an assembly was refused and return its error.
fn refusal(outcome: Result<PluginHost, ContractError>) -> ContractError {
    match outcome {
        Ok(_) => panic!("expected the installation to be refused"),
        Err(err) => err,
    }
}

/// A recording desired-state store proving the injected seam is used.
#[derive(Default)]
struct RecordingDesiredStore {
    seeded: BTreeMap<String, DesiredState>,
    saves: Rc<RefCell<Vec<(String, DesiredState)>>>,
}

impl RecordingDesiredStore {
    fn new(seeded: BTreeMap<String, DesiredState>) -> Self {
        RecordingDesiredStore {
            seeded,
            saves: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

impl DesiredStateStore for RecordingDesiredStore {
    fn load(&mut self) -> Result<BTreeMap<String, DesiredState>, ContractError> {
        Ok(self.seeded.clone())
    }

    fn save(&mut self, plugin_id: &str, desired: DesiredState) -> Result<(), ContractError> {
        self.saves
            .borrow_mut()
            .push((plugin_id.to_string(), desired));
        Ok(())
    }
}

/// Assemble a host for `package` with `installation`, asserting the assembly
/// outcome. `desired` seeds the injected desired-state store.
fn assemble_host(
    scratch: &ScratchDir,
    package: &DiscoveredPackage,
    installation: PluginInstallation,
    desired: Box<dyn DesiredStateStore>,
    clock: Rc<ManualClock>,
) -> Result<PluginHost, ContractError> {
    let root = package.root.clone();
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation);
    let config = PluginHostConfig {
        state_dir: scratch.child("state"),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    PluginHost::assemble(&config, &installations, desired, clock)
}

/// Tamper one installed file's bytes to break the pinned inventory digest.
fn tamper_installed(installed: &InstalledGeneration) {
    fs::write(installed.installed_dir.join("bin/plugin"), b"#!/bin/sh\nexit 7\n").unwrap();
}

// --- lifecycle: READY gate --------------------------------------------------

#[test]
fn ready_success_is_health_gated() {
    let scratch = fresh("hp-registry-ready");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let installed = installed(&registry, PLUGIN_ID, generation);

    let clock = Rc::new(ManualClock::new(1_000));
    let mut sup = supervisor(clock.clone(), Duration::from_secs(5));
    let request = request_for(PLUGIN_ID, &installed, state.join("sock/plugin.sock"));
    let spawned = sup.spawn(&mut registry, &request).unwrap();
    assert_eq!(spawned.epoch, 1);
    assert_eq!(
        registry.entry(PLUGIN_ID).unwrap().observed,
        LifecycleState::Starting,
        "READY alone must not establish readiness"
    );

    // An unhealthy probe leaves the entry Starting.
    let mut unhealthy = ScriptedProbe::healthy(false);
    assert!(!sup.confirm_ready(&mut registry, PLUGIN_ID, &mut unhealthy).unwrap());
    assert_eq!(registry.entry(PLUGIN_ID).unwrap().observed, LifecycleState::Starting);

    let mut healthy = ScriptedProbe::healthy(true);
    assert!(sup.confirm_ready(&mut registry, PLUGIN_ID, &mut healthy).unwrap());
    assert_eq!(registry.entry(PLUGIN_ID).unwrap().observed, LifecycleState::Ready);
    assert_eq!(healthy.calls.borrow().as_slice(), &[(PLUGIN_ID.to_string(), 1)]);

    let stop = sup.stop(PLUGIN_ID).unwrap();
    assert!(stop.process_group_gone, "{}", stop.detail);
    assert!(!sup.is_running(PLUGIN_ID));
}

#[test]
fn ready_timeout_cleans_up_the_process_group() {
    let scratch = fresh("hp-registry-timeout");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let pidfile = scratch.path().join("child.pid");
    // No READY line: the supervisor must time out and tear the group down.
    let package = discover_one(&root, PLUGIN_ID, &pid_recording_silent_child(&pidfile));
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let installed = installed(&registry, PLUGIN_ID, generation);

    let clock = Rc::new(ManualClock::new(0));
    // Generous READY bound: the fake child must record its pid before the
    // timeout tears the group down, even under concurrent test load.
    let mut sup = supervisor(clock, Duration::from_millis(1500));
    let request = request_for(PLUGIN_ID, &installed, state.join("sock/plugin.sock"));
    let err = sup.spawn(&mut registry, &request).unwrap_err();
    assert_eq!(code(&err), FailureCode::StateUnavailable, "{}", message(&err));
    assert_eq!(registry.entry(PLUGIN_ID).unwrap().observed, LifecycleState::Failed);
    assert!(!sup.is_running(PLUGIN_ID));

    // The pid the child recorded is gone: the whole process group was reaped.
    let pid: u32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert!(!alive(pid), "child pid {pid} leaked after a startup timeout");
}

#[test]
fn wrong_ready_identity_is_refused_and_cleaned_up() {
    let scratch = fresh("hp-registry-identity");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let pidfile = scratch.path().join("child.pid");
    let package = discover_one(
        &root,
        PLUGIN_ID,
        &pid_recording_child(&pidfile, "example.impostor", &["report.publish"]),
    );
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let installed = installed(&registry, PLUGIN_ID, generation);

    let clock = Rc::new(ManualClock::new(0));
    let mut sup = supervisor(clock, Duration::from_secs(5));
    let request = request_for(PLUGIN_ID, &installed, state.join("sock/plugin.sock"));
    let err = sup.spawn(&mut registry, &request).unwrap_err();
    assert_eq!(code(&err), FailureCode::IdentityConflict, "{}", message(&err));
    assert!(message(&err).contains("impostor"), "{}", message(&err));
    assert_eq!(registry.entry(PLUGIN_ID).unwrap().observed, LifecycleState::Failed);
    assert!(!sup.is_running(PLUGIN_ID));

    let pid: u32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert!(!alive(pid), "rejected child pid {pid} leaked");
}

#[test]
fn missing_announced_capability_is_refused() {
    let scratch = fresh("hp-registry-capability");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &[]));
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let installed = installed(&registry, PLUGIN_ID, generation);

    let clock = Rc::new(ManualClock::new(0));
    let mut sup = supervisor(clock, Duration::from_secs(5));
    let request = request_for(PLUGIN_ID, &installed, state.join("sock/plugin.sock"));
    let err = sup.spawn(&mut registry, &request).unwrap_err();
    assert_eq!(code(&err), FailureCode::IdentityConflict);
    assert!(message(&err).contains("capabilities"), "{}", message(&err));
    assert!(!sup.is_running(PLUGIN_ID));
}

// --- lifecycle: crash/backoff/restart + epoch fencing -----------------------

#[test]
fn crash_backoff_restart_keeps_generation_and_fences_stale_epochs() {
    let scratch = fresh("hp-registry-crash");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let launch_capture = scratch.path().join("launch-inputs.txt");
    let child_script = format!(
        "#!/bin/sh\n{{ printf 'PATH=%s\\n' \"$AGALMA_PLUGIN_CONFIG_PATH\"; printf 'GEN=%s\\n' \"$AGALMA_PLUGIN_BINDING_GENERATION\"; printf 'DIGEST=%s\\n' \"$AGALMA_PLUGIN_CONFIG_DIGEST\"; for arg in \"$@\"; do printf 'ARG=%s\\n' \"$arg\"; done; }} > '{}'\nprintf '{}'\ncat >/dev/null\n",
        launch_capture.display(),
        ready_line(PLUGIN_ID, &["report.publish"]),
    );
    let package = discover_one(&root, PLUGIN_ID, &child_script);
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let installed_gen = installed(&registry, PLUGIN_ID, generation);

    let clock = Rc::new(ManualClock::new(10_000));
    let mut sup = supervisor(clock.clone(), Duration::from_secs(5));
    let request = request_for(PLUGIN_ID, &installed_gen, state.join("sock/plugin.sock"));

    let first = sup.spawn(&mut registry, &request).unwrap();
    let expected_launch_capture = format!(
        "PATH={}\nGEN={}\nDIGEST={}\nARG=--serve\nARG=--read-only\nARG=--socket\nARG={}\n",
        installed_gen
            .installed_dir
            .join(&installed_gen.configuration.path)
            .display(),
        installed_gen.generation,
        installed_gen.config_digest,
        request.socket.display(),
    );
    assert_eq!(
        fs::read_to_string(&launch_capture).unwrap(),
        expected_launch_capture,
        "the child receives pinned private inputs and unchanged manifest argv"
    );
    let mut probe = ScriptedProbe::healthy(true);
    assert!(sup.confirm_ready(&mut registry, PLUGIN_ID, &mut probe).unwrap());
    registry.pin_operation(PLUGIN_ID, generation, "op-1").unwrap();

    // A crash schedules the first backoff delay (base 100ms) at the injected now.
    let plan = sup.handle_crash(&mut registry, PLUGIN_ID).unwrap();
    assert_eq!(plan.attempt, 1);
    assert_eq!(plan.delay_ms, 100);
    assert_eq!(plan.due_ms, 10_100);
    assert!(plan.stop.process_group_gone, "{}", plan.stop.detail);
    assert_eq!(registry.entry(PLUGIN_ID).unwrap().observed, LifecycleState::Failed);
    assert!(sup.due_restarts(clock.now_ms()).is_empty(), "not due yet");

    // The pinned generation keeps its unresolved pin across the crash.
    assert!(registry.entry(PLUGIN_ID).unwrap().has_unresolved_pins(generation));

    clock.advance(100);
    assert_eq!(sup.take_due_restarts(clock.now_ms()), vec![PLUGIN_ID.to_string()]);

    let restarted_request = request_for(
        PLUGIN_ID,
        &installed_gen,
        state.join("sock/plugin.sock"),
    );
    assert_eq!(restarted_request.args, request.args);
    assert_eq!(restarted_request.env, request.env);
    let second = sup.spawn(&mut registry, &restarted_request).unwrap();
    assert_eq!(second.generation, first.generation, "restart reuses the pin");
    assert!(second.epoch > first.epoch, "restart issues a fresh launch epoch");
    assert_eq!(
        fs::read_to_string(&launch_capture).unwrap(),
        expected_launch_capture,
        "restart reissues the same pinned path, generation, digest and argv"
    );
    assert_eq!(
        registry.entry(PLUGIN_ID).unwrap().active_generation,
        Some(generation)
    );

    // A stale-epoch observation is fenced out.
    let applied = sup
        .observe_health(&mut registry, PLUGIN_ID, first.epoch, false, "stale")
        .unwrap();
    assert!(!applied, "superseded epoch observation must be ignored");
    assert!(registry.pin_operation(PLUGIN_ID, generation, "op-1").is_ok());
    assert!(registry.entry(PLUGIN_ID).unwrap().has_unresolved_pins(generation));

    let mut probe = ScriptedProbe::healthy(true);
    assert!(sup.confirm_ready(&mut registry, PLUGIN_ID, &mut probe).unwrap());
    assert_eq!(registry.entry(PLUGIN_ID).unwrap().observed, LifecycleState::Ready);
    let stop = sup.stop(PLUGIN_ID).unwrap();
    assert!(stop.process_group_gone, "{}", stop.detail);
}

#[test]
fn backoff_grows_and_is_bounded_across_restarts() {
    let scratch = fresh("hp-registry-backoff");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    // A child that announces READY and immediately exits: each crash is real.
    let script = format!(
        "#!/bin/sh\nprintf '{}'\nexit 3\n",
        ready_line(PLUGIN_ID, &["report.publish"])
    );
    let package = discover_one(&root, PLUGIN_ID, &script);
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let installed_gen = installed(&registry, PLUGIN_ID, generation);

    let clock = Rc::new(ManualClock::new(0));
    let mut sup = supervisor(clock.clone(), Duration::from_secs(5));
    let request = request_for(PLUGIN_ID, &installed_gen, state.join("sock/plugin.sock"));

    let mut delays = Vec::new();
    for expected in [100u64, 200, 400, 800, 1_000] {
        sup.spawn(&mut registry, &request).unwrap();
        let plan = sup.handle_crash(&mut registry, PLUGIN_ID).unwrap();
        delays.push(plan.delay_ms);
        // The schedule accumulates on the injected clock.
        let taken = sup.take_due_restarts(clock.now_ms() + plan.delay_ms);
        assert_eq!(taken, vec![PLUGIN_ID.to_string()]);
        let _ = expected;
    }
    assert_eq!(delays, vec![100, 200, 400, 800, 1_000]);
}

// --- lifecycle: dependency readiness ----------------------------------------

#[test]
fn unmet_required_service_blocks_start_without_a_process() {
    let scratch = fresh("hp-registry-depend");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let installed_gen = installed(&registry, PLUGIN_ID, generation);

    let clock = Rc::new(ManualClock::new(0));
    let mut sup = supervisor(clock, Duration::from_secs(5));
    let mut request = request_for(PLUGIN_ID, &installed_gen, state.join("sock/plugin.sock"));
    request.required_services = vec!["component:ledger".to_string()];

    let err = sup.spawn(&mut registry, &request).unwrap_err();
    assert_eq!(code(&err), FailureCode::MissingCapability, "{}", message(&err));
    assert!(!sup.is_running(PLUGIN_ID), "no process may exist for a blocked start");

    // Once the dependency is Ready, the same start succeeds.
    registry.register_builtin("component:ledger", 0);
    assert_eq!(
        registry.entry("component:ledger").unwrap().observed,
        LifecycleState::Ready
    );
    let spawned = sup.spawn(&mut registry, &request).unwrap();
    assert!(spawned.epoch >= 1);
    let stop = sup.stop(PLUGIN_ID).unwrap();
    assert!(stop.process_group_gone, "{}", stop.detail);
}

// --- registry: generations, pins, drain/removal, immutability ---------------

#[test]
fn generations_are_monotonic_and_never_reused() {
    let scratch = fresh("hp-registry-generations");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();

    let first = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let installation = installation_for(&first);
    let g1 = registry.install(&store, &first, &installation).unwrap();
    assert_eq!(g1, 1);

    // Reinstall the same content as a new generation, then remove it.
    let g2 = registry.install(&store, &first, &installation).unwrap();
    assert_eq!(g2, 2);
    registry.remove(PLUGIN_ID, g2, 0).unwrap();

    let g3 = registry.install(&store, &first, &installation).unwrap();
    assert_eq!(g3, 3, "a removed generation number is never reallocated");
    assert_eq!(registry.entry(PLUGIN_ID).unwrap().max_generation, 3);
}

#[test]
fn active_generation_and_pins_refuse_removal_until_resolved() {
    let scratch = fresh("hp-registry-pins");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let g1 = install_active(&mut registry, &store, &package);

    // The active generation cannot be removed before retirement.
    let err = registry.remove(PLUGIN_ID, g1, 0).unwrap_err();
    assert_eq!(code(&err), FailureCode::RevisionConflict, "{}", message(&err));

    let g2 = registry.install(&store, &package, &installation_for(&package)).unwrap();
    registry.stage(PLUGIN_ID, g2).unwrap();
    // A replacement activation drains the prior generation, covering pinned work.
    registry.activate(PLUGIN_ID, g2).unwrap();
    registry.pin_operation(PLUGIN_ID, g1, "op-1").unwrap();

    let err = registry.remove(PLUGIN_ID, g1, 0).unwrap_err();
    assert_eq!(code(&err), FailureCode::EffectUnconfirmed, "{}", message(&err));
    assert!(registry.entry(PLUGIN_ID).unwrap().has_unresolved_pins(g1));

    registry.resolve_pin(PLUGIN_ID, g1, "op-1").unwrap();
    registry.begin_drain(PLUGIN_ID, g2, 0).unwrap();
    registry.retire(PLUGIN_ID, g2, 0).unwrap();
    // g1 is no longer active and has no pins -> removable.
    registry.remove(PLUGIN_ID, g1, 0).unwrap();
    assert!(registry.entry(PLUGIN_ID).unwrap().generations.get(&g1).is_none());
}

#[test]
fn installed_generation_is_immutable_under_discovery_edits() {
    let scratch = fresh("hp-registry-immutable");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let gen = installed(&registry, PLUGIN_ID, generation);

    let before = fs::read(&gen.entrypoint).unwrap();
    assert_eq!(gen.entrypoint, gen.installed_dir.join("bin/plugin"));
    assert_eq!(
        package_inventory_digest(&gen.installed_dir, &package.manifest).unwrap(),
        gen.package_digest
    );

    // Edit the discovery copy: the hub-owned installed copy must be unaffected.
    fs::write(root.join("pkg/bin/plugin"), b"#!/bin/sh\nexit 1\n").unwrap();
    let after = fs::read(&gen.entrypoint).unwrap();
    assert_eq!(before, after, "installed package must not follow discovery edits");
    assert_eq!(
        package_inventory_digest(&gen.installed_dir, &package.manifest).unwrap(),
        gen.package_digest,
        "installed digest stays pinned"
    );
    // The edited discovery copy no longer validates.
    let err = discover_plugins(&[PluginRoot::required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::IntegrityMismatch);

    // Remove the discovery root entirely: the pinned generation still installs
    // and launches from hub-owned storage.
    fs::remove_dir_all(&root).unwrap();
    assert!(gen.entrypoint.is_file());
}

#[test]
fn socket_paths_fit_sun_len_under_a_long_state_dir() {
    let long = PathBuf::from("/".to_string() + &"x".repeat(220));
    let path = plugin_socket_path(&long, "example.really-long-plugin-id/v1", 7);
    assert!(
        path.as_os_str().len() <= SOCKET_PATH_LIMIT,
        "{} len {}",
        path.display(),
        path.as_os_str().len()
    );

    // Under a short state dir the readable direct path is used.
    let short = PathBuf::from("/state");
    let direct = plugin_socket_path(&short, PLUGIN_ID, 1);
    assert!(direct.starts_with(&short));
    assert!(direct.as_os_str().len() <= SOCKET_PATH_LIMIT);
}

// --- desired state + built-ins ----------------------------------------------

#[test]
fn desired_state_persists_through_the_injected_store() {
    let scratch = fresh("hp-registry-desired");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let store = HubInstallStore::new(&state);
    let mut registry = PluginRegistry::new();
    install_active(&mut registry, &store, &package);

    let mut desired = InMemoryDesiredStateStore::new();
    registry
        .set_desired(PLUGIN_ID, DesiredState::Running, &mut desired, 5)
        .unwrap();
    assert_eq!(registry.desired(PLUGIN_ID), DesiredState::Running);

    // Simulate a hub restart: a fresh registry restores the recorded intent.
    let mut restarted = PluginRegistry::new();
    let restored = restarted.load_desired(&mut desired).unwrap();
    assert_eq!(restored.get(PLUGIN_ID), Some(&DesiredState::Running));

    // An unrecorded plugin defaults to Stopped.
    assert_eq!(registry.desired("example.unseen"), DesiredState::Stopped);
}

#[test]
fn builtin_components_are_represented_without_installs() {
    let mut registry = PluginRegistry::new();
    assert!(registry.register_builtin("component:ledger", 0));
    assert!(!registry.register_builtin("component:ledger", 0), "idempotent");
    let entry = registry.entry("component:ledger").unwrap();
    assert_eq!(
        entry.origin,
        agalma_conductor::plugin_registry::ComponentOrigin::BuiltIn
    );
    assert!(entry.generations.is_empty(), "no install is claimed");
    assert_eq!(entry.observed, LifecycleState::Ready);
}

// --- production host wiring --------------------------------------------------

/// `agalma plugins --state-dir <dir> --plugin-root <root> --once`
///
/// The bundled host entry is exercised here through its real assembly/lifecycle
/// functions (no paid queue claim). Pending attachments: HP.4 supplies the real
/// Connect RPC health probe; HP.5 supplies the durable selected-ledger
/// installation/desired-state stores. Until then the host uses the in-memory
/// seams and the always-healthy stand-in probe.
#[test]
fn production_host_entry_assembles_and_starts_without_paid_task() {
    let scratch = fresh("hp-registry-host");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));

    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_for(&package));

    let config = PluginHostConfig {
        state_dir: state.clone(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    let clock = Rc::new(ManualClock::new(42));
    let mut host =
        PluginHost::assemble(&config, &installations, default_desired(), clock).unwrap();

    // Built-ins are represented; the discovered package is installed.
    assert!(host.registry.entry("component:ledger").is_some());
    assert_eq!(host.installed.get(PLUGIN_ID), Some(&1));
    assert!(host.diagnostics.is_empty(), "{:?}", host.diagnostics);

    host.set_desired(PLUGIN_ID, DesiredState::Running).unwrap();
    let mut probe = ScriptedProbe::healthy(true);
    let started = host.start_desired(&mut probe).unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(registry_observed(&host, PLUGIN_ID), LifecycleState::Ready);

    let evidence = host.shutdown();
    assert_eq!(evidence.len(), 1);
    assert!(evidence[0].1.process_group_gone, "{}", evidence[0].1.detail);
}

#[test]
fn host_reports_unapproved_plugin_as_a_diagnostic_not_a_start() {
    let scratch = fresh("hp-registry-host-unapproved");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let _package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));

    // No installation supplied -> discovered but never started.
    let installations = InMemoryInstallations::new();
    let config = PluginHostConfig {
        state_dir: state.clone(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    let clock = Rc::new(ManualClock::new(0));
    let mut host =
        PluginHost::assemble(&config, &installations, default_desired(), clock).unwrap();
    assert!(host.installed.is_empty());
    assert_eq!(host.diagnostics.len(), 1);
    assert!(host.diagnostics[0].message.contains("no approved installation"));

    let mut probe = ScriptedProbe::healthy(true);
    assert!(host.start_desired(&mut probe).unwrap().is_empty());
}

fn registry_observed(host: &PluginHost, plugin_id: &str) -> LifecycleState {
    host.registry.entry(plugin_id).unwrap().observed
}

// --- repair findings (Sol review) -------------------------------------------

#[test]
fn installation_pin_validates_and_carries_config_profile_and_grants() {
    let scratch = fresh("hp-registry-install-pin");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let installation = installation_for(&package);

    // A valid pin installs; the generation carries the frozen ref + grants.
    let store = HubInstallStore::new(&scratch.child("state"));
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let gen = installed(&registry, PLUGIN_ID, generation);
    assert_eq!(gen.configuration, package.manifest.default_configuration);
    assert_eq!(
        gen.config_digest,
        package.manifest.default_configuration.digest
    );
    assert_eq!(gen.launch_profile, package.manifest.entrypoint.launch_profile);
    assert_eq!(gen.requested_capabilities, vec!["report.publish".to_string()]);
    assert_eq!(gen.approved_capabilities, vec!["report.publish".to_string()]);

    // The launch request carries the pinned config/profile and grants as scope.
    let socket = scratch.child("sock.sock");
    let request = LaunchRequest::from_installed(&gen, socket.clone());
    assert_eq!(request.configuration, installation.configuration);
    assert_eq!(request.config_digest, gen.config_digest);
    let mut expected_args = gen.argv.clone();
    expected_args.extend([
        "--socket".to_string(),
        socket.to_string_lossy().into_owned(),
    ]);
    assert_eq!(
        request.args, expected_args,
        "only the existing socket args append"
    );
    assert_eq!(
        request.env.get(PLUGIN_CONFIG_PATH_ENV).map(String::as_str),
        Some(
            gen.installed_dir
                .join(&gen.configuration.path)
                .to_str()
                .unwrap()
        )
    );
    let expected_generation = gen.generation.to_string();
    assert_eq!(
        request
            .env
            .get(PLUGIN_BINDING_GENERATION_ENV)
            .map(String::as_str),
        Some(expected_generation.as_str())
    );
    assert_eq!(
        request.env.get(PLUGIN_CONFIG_DIGEST_ENV).map(String::as_str),
        Some(gen.config_digest.as_str())
    );
    assert_eq!(request.launch_profile, gen.launch_profile);
    assert_eq!(request.approved_capabilities, gen.approved_capabilities);
    assert!(request.required_capabilities.contains("report.publish"));

    // A tampered configuration ref (resealed to pass the digest gate) fails
    // against the package bytes.
    let mut bad_config = installation.clone();
    bad_config.configuration.digest = sha256_digest(b"tampered-configuration\n");
    reseal_installation(&mut bad_config);
    let err = refusal(assemble_host(
        &scratch,
        &package,
        bad_config,
        default_desired(),
        Rc::new(ManualClock::new(0)),
    ));
    assert_eq!(code(&err), FailureCode::IntegrityMismatch, "{}", message(&err));

    // A grant the package never requested is a scope escalation.
    let mut escalated = installation.clone();
    escalated.approved_capabilities = vec!["report.publish".into(), "admin.everything".into()];
    reseal_installation(&mut escalated);
    let err = refusal(assemble_host(
        &scratch,
        &package,
        escalated,
        default_desired(),
        Rc::new(ManualClock::new(0)),
    ));
    assert_eq!(code(&err), FailureCode::ScopeDenied, "{}", message(&err));

    // A launch-profile mismatch is refused.
    let mut wrong_profile = installation.clone();
    wrong_profile.launch_profile = "other-profile/v9".into();
    reseal_installation(&mut wrong_profile);
    let err = refusal(assemble_host(
        &scratch,
        &package,
        wrong_profile,
        default_desired(),
        Rc::new(ManualClock::new(0)),
    ));
    assert_eq!(code(&err), FailureCode::InvalidRecord, "{}", message(&err));
}

#[test]
fn launch_refuses_malformed_private_configuration_pins_before_spawn() {
    let bad_pins = [
        (PLUGIN_CONFIG_PATH_ENV, "../outside/config.json".to_string()),
        (PLUGIN_BINDING_GENERATION_ENV, "not-a-generation".to_string()),
        (PLUGIN_CONFIG_DIGEST_ENV, format!("sha256:{}", "0".repeat(64))),
    ];

    for (index, (key, value)) in bad_pins.iter().enumerate() {
        let scratch = fresh(&format!("hp-registry-invalid-launch-pin-{index}"));
        let state = scratch.child("state");
        let root = scratch.child("plugins");
        let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
        let store = HubInstallStore::new(&state);
        let mut registry = PluginRegistry::new();
        let generation = install_active(&mut registry, &store, &package);
        let installed_gen = installed(&registry, PLUGIN_ID, generation);
        let mut request =
            LaunchRequest::from_installed(&installed_gen, scratch.child("sock.sock"));
        request.env.insert((*key).to_string(), value.clone());

        let mut sup = supervisor(Rc::new(ManualClock::new(0)), Duration::from_secs(5));
        let error = sup.spawn(&mut registry, &request).unwrap_err();
        assert_eq!(code(&error), FailureCode::IntegrityMismatch, "{key}");
        assert!(!sup.is_running(PLUGIN_ID), "malformed launch pin spawned a child");
    }
}

#[test]
fn stage_and_launch_rehash_installed_bytes() {
    let scratch = fresh("hp-registry-rehash");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let store = HubInstallStore::new(&state);

    // Stage rehash: install without staging, tamper, then stage refuses.
    let mut registry = PluginRegistry::new();
    let generation = registry
        .install(&store, &package, &installation_for(&package))
        .unwrap();
    let gen = installed(&registry, PLUGIN_ID, generation);
    tamper_installed(&gen);
    let err = registry.stage(PLUGIN_ID, generation).unwrap_err();
    assert_eq!(code(&err), FailureCode::IntegrityMismatch, "{}", message(&err));

    // Launch rehash: clean install+stage+activate, then tamper the bytes.
    let mut registry = PluginRegistry::new();
    let generation = install_active(&mut registry, &store, &package);
    let gen = installed(&registry, PLUGIN_ID, generation);
    tamper_installed(&gen);
    let clock = Rc::new(ManualClock::new(0));
    let mut sup = supervisor(clock, Duration::from_secs(5));
    let request = request_for(PLUGIN_ID, &gen, state.join("sock/plugin.sock"));
    let err = sup.spawn(&mut registry, &request).unwrap_err();
    assert_eq!(code(&err), FailureCode::IntegrityMismatch, "{}", message(&err));
    assert!(!sup.is_running(PLUGIN_ID));

    // The host's launch_request refuses a tampered active generation too.
    let mut host = assemble_host(
        &scratch,
        &package,
        installation_for(&package),
        default_desired(),
        Rc::new(ManualClock::new(0)),
    )
    .unwrap();
    let gen2 = installed(&host.registry, PLUGIN_ID, 1);
    tamper_installed(&gen2);
    let err = host.launch_request(PLUGIN_ID).unwrap_err();
    assert_eq!(code(&err), FailureCode::IntegrityMismatch, "{}", message(&err));
}

#[test]
fn production_loop_observes_exit_and_restarts_under_backoff() {
    let scratch = fresh("hp-registry-supervise");
    let root = scratch.child("plugins");
    // A real child that announces READY and immediately exits.
    let exit_script = format!(
        "#!/bin/sh\nprintf '{}'\nexit 0\n",
        ready_line(PLUGIN_ID, &["report.publish"])
    );
    let package = discover_one(&root, PLUGIN_ID, &exit_script);

    let clock = Rc::new(ManualClock::new(1_000));
    let mut seeded = BTreeMap::new();
    seeded.insert(PLUGIN_ID.to_string(), DesiredState::Running);
    let mut host = assemble_host(
        &scratch,
        &package,
        installation_for(&package),
        Box::new(RecordingDesiredStore::new(seeded)),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(host.registry.desired(PLUGIN_ID), DesiredState::Running);

    let mut probe = ScriptedProbe::healthy(true);
    assert_eq!(host.start_desired(&mut probe).unwrap().len(), 1);
    let epoch_before = host.registry.entry(PLUGIN_ID).unwrap().launch_epoch;

    // Poll for the production loop to observe the real child's exit.
    let mut crashed = Vec::new();
    for _ in 0..80 {
        let outcome = host.supervise_tick(&mut probe).unwrap();
        if !outcome.crashed.is_empty() {
            crashed = outcome.crashed;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(crashed.len(), 1, "child exit was not observed");
    assert_eq!(crashed[0].delay_ms, 250, "default backoff base");
    assert_eq!(
        host.registry.entry(PLUGIN_ID).unwrap().observed,
        LifecycleState::Failed
    );

    // Nothing restarts before the backoff is due.
    assert!(host.supervise_tick(&mut probe).unwrap().restarted.is_empty());
    clock.advance(250);
    let outcome = host.supervise_tick(&mut probe).unwrap();
    assert_eq!(outcome.restarted.len(), 1, "scheduled restart did not run");
    assert!(host.registry.entry(PLUGIN_ID).unwrap().launch_epoch > epoch_before);
    let _ = host.shutdown();
}

#[test]
fn injected_desired_store_is_loaded_and_used_by_assembly() {
    let scratch = fresh("hp-registry-desired-inject");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));

    let mut seeded = BTreeMap::new();
    seeded.insert(PLUGIN_ID.to_string(), DesiredState::Running);
    let store = RecordingDesiredStore::new(seeded);
    let saves = Rc::clone(&store.saves);

    let clock = Rc::new(ManualClock::new(7));
    let mut host = assemble_host(
        &scratch,
        &package,
        installation_for(&package),
        Box::new(store),
        clock,
    )
    .unwrap();

    // Loaded from the injected store at assembly.
    assert_eq!(host.registry.desired(PLUGIN_ID), DesiredState::Running);

    // Used by start_desired with no explicit set.
    let mut probe = ScriptedProbe::healthy(true);
    assert_eq!(host.start_desired(&mut probe).unwrap().len(), 1);

    // Writes flow through the injected store.
    host.set_desired(PLUGIN_ID, DesiredState::Stopped).unwrap();
    assert_eq!(
        saves.borrow().as_slice(),
        &[(PLUGIN_ID.to_string(), DesiredState::Stopped)]
    );
    let _ = host.shutdown();
}

#[test]
fn candidate_stays_pending_until_ready_then_activates() {
    let scratch = fresh("hp-registry-pending-activate");
    let root = scratch.child("plugins");
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));

    // Assembly installs and stages the candidate; it is NOT active yet.
    let mut host = assemble_host(
        &scratch,
        &package,
        installation_for(&package),
        default_desired(),
        Rc::new(ManualClock::new(0)),
    )
    .unwrap();
    let entry = host.registry.entry(PLUGIN_ID).unwrap();
    assert_eq!(entry.pending_generation, Some(1), "candidate is staged");
    assert_eq!(
        entry.active_generation, None,
        "a candidate must not be active before startup/health gating"
    );
    assert_eq!(entry.observed, LifecycleState::Installed);

    // Spawn the candidate: still pending, still not active while Starting.
    let request = host.launch_request(PLUGIN_ID).unwrap();
    assert_eq!(request.generation, 1, "launch uses the staged candidate");
    let spawned = host
        .supervisor
        .spawn(&mut host.registry, &request)
        .unwrap();
    assert_eq!(spawned.epoch, 1);
    let entry = host.registry.entry(PLUGIN_ID).unwrap();
    assert_eq!(entry.observed, LifecycleState::Starting);
    assert_eq!(entry.pending_generation, Some(1));
    assert_eq!(
        entry.active_generation, None,
        "still not active while startup/health gating is incomplete"
    );

    // An unhealthy probe leaves the candidate pending and inactive.
    let mut unhealthy = ScriptedProbe::healthy(false);
    assert!(!host
        .supervisor
        .confirm_ready(&mut host.registry, PLUGIN_ID, &mut unhealthy)
        .unwrap());
    let entry = host.registry.entry(PLUGIN_ID).unwrap();
    assert_eq!(entry.observed, LifecycleState::Starting);
    assert_eq!(entry.pending_generation, Some(1));
    assert_eq!(entry.active_generation, None);

    // Health success performs the pending-to-active transition.
    let mut healthy = ScriptedProbe::healthy(true);
    assert!(host
        .supervisor
        .confirm_ready(&mut host.registry, PLUGIN_ID, &mut healthy)
        .unwrap());
    let entry = host.registry.entry(PLUGIN_ID).unwrap();
    assert_eq!(entry.observed, LifecycleState::Ready);
    assert_eq!(
        entry.active_generation,
        Some(1),
        "pending candidate becomes active only after readiness"
    );
    assert_eq!(entry.pending_generation, None);
    let _ = host.shutdown();
}

#[test]
fn plugin_host_entry_round_trips_through_common_config() {
    use agalma_conductor::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::try_parse_from([
        "agalma",
        "plugins",
        "--state-dir",
        "/state",
        "--plugin-root",
        "/roots/one",
        "--plugin-root-optional",
        "/roots/optional",
        "--once",
    ])
    .unwrap();
    let Command::Plugins(args) = cli.command else {
        panic!("expected plugins command");
    };
    let config = PluginHostConfig::resolve(&args).unwrap();
    assert_eq!(config.state_dir, PathBuf::from("/state"));
    assert_eq!(
        config.plugin_roots,
        vec![
            PluginRoot::required("/roots/one"),
            PluginRoot::optional("/roots/optional"),
        ]
    );
    assert!(config.once);
    assert_eq!(
        config.state_dir,
        PluginHostConfig::resolve(&args).unwrap().state_dir
    );
}

// --- property tests ----------------------------------------------------------

#[test]
fn property_backoff_is_monotonic_and_bounded() {
    let mut rng = XorShift64::new(PROPERTY_SEED);
    for _ in 0..PROPERTY_CASES {
        let base = 1 + rng.below(500);
        let factor = 2 + rng.below(3);
        let max = base + rng.below(5_000);
        let policy = BackoffPolicy {
            base_ms: base,
            factor,
            max_ms: max,
        };
        let mut previous = 0;
        for attempt in 0..12 {
            let delay = policy.delay_ms(attempt);
            assert!(delay <= max, "delay {delay} exceeds max {max}");
            assert!(delay >= previous, "backoff must not shrink");
            previous = delay;
        }
    }
}

#[test]
fn property_socket_paths_always_fit_sun_len() {
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x5150);
    for case in 0..PROPERTY_CASES {
        let depth = 1 + rng.below(180) as usize;
        let state_dir = PathBuf::from("/".to_string() + &"s".repeat(depth));
        let id_len = 1 + rng.below(64) as usize;
        let raw: String = (0..id_len)
            .map(|_| char::from(b'a' + rng.below(26) as u8))
            .collect();
        let plugin_id = format!("{raw}-{case}");
        let path = plugin_socket_path(&state_dir, &plugin_id, rng.next_u64());
        assert!(
            path.as_os_str().len() <= SOCKET_PATH_LIMIT,
            "case {case}: {} len {}",
            path.display(),
            path.as_os_str().len()
        );
    }
}

#[test]
fn property_generation_allocation_is_strictly_increasing() {
    let scratch = fresh("hp-registry-prop-generation");
    let state = scratch.child("state");
    let root = scratch.child("plugins");
    let store = HubInstallStore::new(&state);
    let package = discover_one(&root, PLUGIN_ID, &live_child(PLUGIN_ID, &["report.publish"]));
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x9e37);

    let mut registry = PluginRegistry::new();
    let installation = installation_for(&package);
    let mut last = 0;
    for _ in 0..PROPERTY_CASES {
        let generation = registry.install(&store, &package, &installation).unwrap();
        assert!(generation > last, "generation {generation} <= {last}");
        last = generation;
        let max_recorded = registry.entry(PLUGIN_ID).unwrap().max_generation;
        assert_eq!(max_recorded, last);
        if rng.below(3) == 0 {
            // Remove a non-active candidate; the number is retired, never reused.
            registry.remove(PLUGIN_ID, generation, 0).unwrap();
        }
    }
    assert_eq!(last, PROPERTY_CASES);
}

#[test]
fn property_installed_launch_preserves_argv_and_exact_private_pins() {
    let scratch = fresh("hp-registry-launch-properties");
    let mut rng = XorShift64::new(LAUNCH_PROPERTY_SEED);

    for case in 0..LAUNCH_PROPERTY_CASES {
        let root = scratch.child(&format!("plugins-{case}"));
        let package_dir = root.join("pkg");
        let argv: Vec<String> = (0..=rng.below(4))
            .map(|index| format!("--generated-{case}-{index}-{}", rng.next_u64()))
            .collect();
        let mut manifest = base_manifest(PLUGIN_ID);
        manifest["entrypoint"]["argv"] = json!(argv);
        seal_package(
            &package_dir,
            &manifest,
            &live_child(PLUGIN_ID, &["report.publish"]),
        );
        let package = discover_plugins(&[PluginRoot::required(&root)])
            .unwrap()
            .package(PLUGIN_ID)
            .unwrap()
            .clone();
        let state = scratch.child(&format!("state-{case}"));
        let store = HubInstallStore::new(&state);
        let mut registry = PluginRegistry::new();
        let generation = install_active(&mut registry, &store, &package);
        let installed_gen = installed(&registry, PLUGIN_ID, generation);
        let socket = scratch.child(&format!("socket-{case}.sock"));
        let request = LaunchRequest::from_installed(&installed_gen, socket.clone());

        let mut expected_args = installed_gen.manifest.entrypoint.argv.clone();
        expected_args.extend([
            "--socket".to_string(),
            socket.to_string_lossy().into_owned(),
        ]);
        assert_eq!(
            request.args, expected_args,
            "seed={LAUNCH_PROPERTY_SEED:#x}, case={case}"
        );
        assert_eq!(
            request.env.get(PLUGIN_CONFIG_PATH_ENV).map(String::as_str),
            Some(
                installed_gen
                    .installed_dir
                    .join(&installed_gen.configuration.path)
                    .to_str()
                    .unwrap()
            )
        );
        let expected_generation = installed_gen.generation.to_string();
        assert_eq!(
            request
                .env
                .get(PLUGIN_BINDING_GENERATION_ENV)
                .map(String::as_str),
            Some(expected_generation.as_str())
        );
        assert_eq!(
            request
                .env
                .get(PLUGIN_CONFIG_DIGEST_ENV)
                .map(String::as_str),
            Some(installed_gen.config_digest.as_str())
        );

        // Every eighth input is deliberately malformed and must be refused
        // before a process exists; the remaining generated launch descriptors
        // are valid installed pins. This records both sides of the property.
        if case % 8 == 0 {
            let key = match (case / 8) % 3 {
                0 => PLUGIN_CONFIG_PATH_ENV,
                1 => PLUGIN_BINDING_GENERATION_ENV,
                _ => PLUGIN_CONFIG_DIGEST_ENV,
            };
            let invalid_value = match key {
                PLUGIN_CONFIG_PATH_ENV => format!("../escape-{case}/config.json"),
                PLUGIN_BINDING_GENERATION_ENV => "0".to_string(),
                _ => format!("sha256:{}", "f".repeat(64)),
            };
            let mut invalid_request = request.clone();
            invalid_request.env.insert(key.to_string(), invalid_value);
            let mut sup = supervisor(Rc::new(ManualClock::new(case)), Duration::from_secs(5));
            let error = sup
                .spawn(&mut registry, &invalid_request)
                .expect_err("malformed private pin must fail closed");
            assert_eq!(code(&error), FailureCode::IntegrityMismatch);
            assert!(!sup.is_running(PLUGIN_ID));
        }
    }
}

#[test]
fn property_installed_copy_matches_its_pinned_digest_after_edits() {
    let scratch = fresh("hp-registry-prop-immutable");
    let state = scratch.child("state");
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0xc0ffee);
    let store = HubInstallStore::new(&state);

    for case in 0..PROPERTY_CASES {
        let root = scratch.child(&format!("case-{case}/plugins"));
        let plugin_id = format!("example.prop-{case}");
        // Vary the entrypoint body so each digest differs.
        let body: String = (0..(8 + rng.below(40)))
            .map(|_| char::from(b'a' + rng.below(26) as u8))
            .collect();
        let script = format!(
            "#!/bin/sh\n{}\nprintf '{}'\ncat >/dev/null\n",
            body,
            ready_line(&plugin_id, &["report.publish"])
        );
        let package = discover_one(&root, &plugin_id, &script);
        let mut registry = PluginRegistry::new();
        let generation = install_active(&mut registry, &store, &package);
        let gen = installed(&registry, &plugin_id, generation);

        let before = fs::read(&gen.entrypoint).unwrap();
        // A discovery-directory edit must not change the installed copy.
        fs::write(root.join("pkg/bin/plugin"), b"#!/bin/sh\nexit 9\n").unwrap();
        let after = fs::read(&gen.entrypoint).unwrap();
        assert_eq!(before, after, "case {case}: installed copy changed");
        assert_eq!(
            package_inventory_digest(&gen.installed_dir, &package.manifest).unwrap(),
            gen.package_digest
        );
    }
}
