//! HP.8 independent plugin install, UI reporting, action, restart and
//! replacement proof (`agalma-jmr.8`).
//!
//! This proof drives the **real production host wiring** — the same
//! `PluginHost::assemble_with_ledger` the shipped `agalma plugins` entry calls —
//! together with the shipped `agalma plugins` binary itself, a real
//! out-of-process plugin package (`fixtures/plugin-pack`, its own Cargo
//! workspace, speaking the common Connect plugin protocol over a Unix socket)
//! and the **same selected ledger** the production read model and action
//! service use. No test-only hub, no mock action driver, and no substitute
//! executor are used for the required production path: reports arrive over the
//! real `hub.PublishReport` RPC, actions run through the production
//! `ServiceActionDriver` + `PluginRpcActionExecutor` and are dispatched through
//! the actual `watch::App::handle_key_with` UI key path.
//!
//! ## How the production path is reached
//!
//! * Install/startup/shutdown: the shipped `agalma plugins --once` binary.
//! * Reporting, generic-UI rendering, actions, restart and replacement: the
//!   production host assembled in-process (exactly as `run_plugin_host` does,
//!   including serving the hub capability socket), because the shipped entry
//!   only attaches the action service on an interactive TTY and this test cannot
//!   allocate one without a new dependency. The in-process assembly is the same
//!   production function, not a re-implementation.
//!
//! ## Known production-path limits surfaced by this proof
//!
//! These are recorded as remaining uncertainty (see
//! `docs/plugin-platform-evidence.md`); HP.8 owns proof only and did not change
//! production files:
//!
//! 1. The private launch channel conveys the per-launch grant tokens and the hub
//!    socket, but **not** the spoke's pinned `BindingPin/v1`. A report
//!    publication must carry the exact authenticated binding, so the
//!    HP.8-owned fixture derives its own binding from its hub-owned installed
//!    location instead of a host-provided value.
//! 2. Configured-root auto-install installs a generation but does not set its
//!    desired state to `Running`; a launch requires an explicit
//!    `set_desired(Running)` (production API). The proof does this.
//! 3. The HP.5 registry-restore repair (`3eabeed`) now restores the durable
//!    `max_generation`/`active_generation` checkpoint across a hub reassembly:
//!    a restart reuses the prior generation for the same package and a changed
//!    package allocates a strictly higher generation, so an operation pinned to
//!    the previous generation is never redirected. Case 8
//!    (`hub_reassembly_must_not_reuse_a_generation_for_a_replacement`) asserts
//!    this over the shipped `agalma plugins --once` binary. Case 4 covers a
//!    *child* restart under one long-lived host. Case 5 drives a production
//!    upgrade/rollback (real delayed action pinned to g1, sealed g2 install/stage
//!    through the supervisor, reconcile on g1, rollback, further action) and case
//!    9 reassembles the actual host to prove the binding/receipt survive.
//!
//! The hub binary digest is pinned before any plugin generation is built and
//! re-asserted after every case; the fixture's effects live in an independent
//! plugin-side journal the hub cannot write.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agalma_conductor::components::{
    derive_plugin_components_from_sources, PluginComponent, PluginSource, Transports,
};
use agalma_conductor::composition::{
    auto_approved_capabilities, default_installation, InMemoryInstallations, PluginHost,
    SAFE_CAPABILITY_ALLOWLIST,
};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::control::{ControlOutcome, ControlRequest};
use agalma_conductor::ledger_process::{LedgerHandle, LedgerTransport};
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_registry::{
    plugin_socket_path, read_registry_checkpoint, ComponentOrigin, DesiredState, HubInstallStore,
    LifecycleState, SOCKET_PATH_LIMIT,
};
use agalma_conductor::plugin_reports::{seal_publication, Freshness, ReportIngestor};
use agalma_conductor::plugin_router::RpcHealthProbe;
use agalma_conductor::watch::{render_to_string, ActionDriver, App, ControlDriver};
use agalma_contracts::digest::{failure_code, sha256_digest, FailureCode};
use agalma_contracts::task_package::BindingPin;
use agalma_contracts::{ComponentOperationState, Durability, LedgerApi, PluginManifest};
use agalma_testkit::ScratchDir;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The fixture plugin id (unknown to any hub before discovery).
const PLUGIN_ID: &str = "example.plugin-pack";
/// The fixture's own report namespace.
const REPORT_NAMESPACE: &str = "example.plugin-pack.load";
/// The permitted action (requires `ledger.read`).
const FLUSH_ACTION: &str = "flush";
/// The denied action (requires `ledger.write`, never auto-granted).
const DANGER_ACTION: &str = "danger";
/// The independent plugin-side effect journal path (child env).
const EFFECT_JOURNAL_ENV: &str = "AGALMA_EFFECT_JOURNAL";
/// The fixture's delayed-action list (child env): those actions apply the effect
/// but withhold the terminal report, so the hub settles them via reconcile.
const DELAY_ACTIONS_ENV: &str = "AGALMA_PLUGIN_DELAY_ACTIONS";

// ---------------------------------------------------------------------------
// Environment / serialization
// ---------------------------------------------------------------------------

/// Serializes the cases that set the process-global child launch environment
/// (`AGALMA_EFFECT_JOURNAL`), so a concurrently running case cannot observe or
/// leak another case's journal path through the inherited process environment.
static CHILD_ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_child_env() -> std::sync::MutexGuard<'static, ()> {
    CHILD_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Repository root (`<root>/crates/agalma-conductor` → `<root>`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("conductor lives under <root>/crates/")
        .to_path_buf()
}

/// The production hub binary built from this package.
fn hub_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agalma"))
}

/// A retained scratch directory under the persistent evidence root.
fn scratch(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(name);
    dir.keep();
    dir
}

/// Build the standalone fixture package with the documented offline command.
fn build_fixture() -> PathBuf {
    let root = repo_root();
    let status = Command::new(env!("CARGO"))
        .current_dir(&root)
        .args([
            "build",
            "--offline",
            "--manifest-path",
            "fixtures/plugin-pack/Cargo.toml",
            "--target-dir",
            "target/plugin-pack-build",
        ])
        .status()
        .expect("run fixture cargo build");
    assert!(status.success(), "fixtures/plugin-pack build failed: {status}");
    root.join("target/plugin-pack-build/debug/plugin-pack")
}

/// Assemble a runtime package directory around the built executable.
///
/// Copies the fixture's declared artifacts and executable into `pkg`, seals the
/// `package_digest` over the ordered inventory, and writes
/// `plugin-manifest.json`.
fn assemble_package(pkg: &Path, built_binary: &Path) -> PluginManifest {
    assemble_package_with_version(pkg, built_binary, None)
}

/// As [`assemble_package`], optionally overriding the declared package version
/// so a second generation has distinct sealed bytes.
fn assemble_package_with_version(
    pkg: &Path,
    built_binary: &Path,
    version: Option<&str>,
) -> PluginManifest {
    let fixture = repo_root().join("fixtures/plugin-pack");
    std::fs::create_dir_all(pkg.join("bin")).unwrap();
    for artifact in ["config-schema.json", "default-config.json", "report-schema.json"] {
        std::fs::copy(fixture.join(artifact), pkg.join(artifact)).unwrap();
    }
    let entry = pkg.join("bin/plugin-pack");
    std::fs::copy(built_binary, &entry).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let template: PluginManifest =
        serde_json::from_slice(&std::fs::read(fixture.join("manifest.json")).unwrap()).unwrap();
    let mut sealed = serde_json::to_value(&template).unwrap();
    if let Some(version) = version {
        sealed["package_version"] = Value::String(version.to_string());
    }
    let sealed_manifest: PluginManifest = serde_json::from_value(sealed.clone()).unwrap();
    let digest = package_inventory_digest(pkg, &sealed_manifest).unwrap();
    sealed["package_digest"] = Value::String(digest);
    std::fs::write(
        pkg.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&sealed).unwrap(),
    )
    .unwrap();
    serde_json::from_value(sealed).unwrap()
}

/// The recorded hub binary digest (immutable proof input).
fn hub_digest() -> String {
    sha256_digest(&std::fs::read(hub_bin()).expect("read hub binary"))
}

/// Assert the hub binary is byte-identical to `before`.
fn assert_hub_unchanged(before: &str) {
    assert_eq!(before, hub_digest(), "hub binary digest changed");
}

/// A `Send + Sync` manual clock over an atomic counter.
struct ManualClock {
    now: Arc<AtomicU64>,
}

impl ManualClock {
    fn new(start: u64) -> Self {
        ManualClock {
            now: Arc::new(AtomicU64::new(start)),
        }
    }

    fn clock(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let now = Arc::clone(&self.now);
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    fn advance(&self, by: u64) {
        self.now.fetch_add(by, Ordering::SeqCst);
    }
}

/// A no-op control driver: the action key path never touches it.
struct NoopControl;

impl ControlDriver for NoopControl {
    fn apply(&mut self, _request: &ControlRequest) -> Result<ControlOutcome, agalma_contracts::ContractError> {
        Ok(ControlOutcome::Applied)
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The plugin-side effect journal lines (`operation_id`, `generation`).
fn read_effects(path: &Path) -> Vec<(String, u64)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            let operation_id = value.get("operation_id")?.as_str()?.to_string();
            let generation = value.get("generation")?.as_u64()?;
            Some((operation_id, generation))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// In-process production host harness
// ---------------------------------------------------------------------------

/// Assemble the production host over a sealed package and serve its hub socket.
///
/// Mirrors the shipped `agalma plugins` entry (assemble → serve hub socket →
/// start desired), except that the desired state is set to `Running` explicitly
/// via the production API (configured-root auto-install installs but does not
/// start — see the module notes). Synchronous: the owned runtimes (the action
/// executor and health probe) must not be dropped inside an async context, and
/// the production entry does exactly this work from a blocking context.
fn start_production_host(
    rt: &tokio::runtime::Runtime,
    state: &Path,
    root: &Path,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> (PluginHost, RpcHealthProbe, LedgerHandle) {
    let config = PluginHostConfig {
        state_dir: state.to_path_buf(),
        plugin_roots: vec![PluginRoot::required(root)],
        once: false,
    };
    let ledger = LedgerHandle::in_process(state).expect("open selected ledger");
    let mut host = PluginHost::assemble_with_ledger(
        &config,
        &InMemoryInstallations::new(),
        ledger.clone(),
        clock,
    )
    .expect("assemble production host");
    assert!(
        host.installed.contains_key(PLUGIN_ID),
        "the unknown package auto-installs from the configured root: diagnostics={:?}",
        host.diagnostics
    );

    // Serve the hub capability RPC exactly as the production entry does, so the
    // spoke's `hub.PublishReport` callback has a route.
    let hub_socket = host.hub_socket.clone();
    let router = host.router.clone();
    rt.spawn(async move {
        if let Err(err) = agalma_conductor::plugin_router::serve_hub(&hub_socket, router).await {
            eprintln!("test: hub socket stopped: {err}");
        }
    });

    host.set_desired(PLUGIN_ID, DesiredState::Running)
        .expect("set desired running");
    let mut probe = RpcHealthProbe::new(host.state_dir.clone());
    let started = host.start_desired(&mut probe).expect("start desired plugins");
    assert_eq!(started.len(), 1, "the unknown package starts exactly once");
    (host, probe, ledger)
}

/// A shared runtime for the in-process hub capability socket.
fn hub_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("hub runtime")
}

/// Poll until at least one durable report is ingested for the plugin, or panic.
fn wait_for_report(host: &PluginHost, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if report_entry_count(host) > 0 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("no durable report was ingested within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The live host's generic plugin read-model components.
///
/// Calls the shipped production `PluginHost::plugin_components()` (HP.6): the
/// read model the watch UI overlays each tick, derived from the **running**
/// host's registry and its attached report/health ingestor, rendering
/// `PluginComponent` (including the hub-observed `health` view and the
/// supervisor-observed `disconnected` flag).
fn live_components(host: &PluginHost) -> Vec<PluginComponent> {
    host.plugin_components()
}

/// The live `PluginComponent` for the fixture plugin.
fn live_component(host: &PluginHost) -> PluginComponent {
    live_components(host)
        .into_iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .unwrap_or_else(|| panic!("the running host exposes `{PLUGIN_ID}` in its read model"))
}

/// The number of durable report entries currently ingested for the plugin.
fn report_entry_count(host: &PluginHost) -> usize {
    let reports = host.reports.clone().expect("report ingestor attached");
    let ingestor = reports.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    ingestor
        .view(PLUGIN_ID)
        .map(|view| view.entries.len())
        .unwrap_or(0)
}

/// Build the production generic read model over the selected in-process ledger.
fn read_model(state: &Path) -> agalma_conductor::status::Snapshot {
    agalma_conductor::status::snapshot_with_selected_transport(
        state,
        &Transports::all_in_process(),
        LedgerTransport::InProcess,
    )
    .expect("production read model")
}

fn find_plugin<'a>(snapshot: &'a agalma_conductor::status::Snapshot) -> &'a PluginComponent {
    snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .unwrap_or_else(|| {
            panic!(
                "the unknown plugin `{PLUGIN_ID}` did not appear in the production read model; \
                 components: {:?}",
                snapshot
                    .plugins
                    .iter()
                    .map(|c| c.plugin_id.clone())
                    .collect::<Vec<_>>()
            )
        })
}

/// Push the host's current live panel through the shipped watch overlay and
/// render one frame (`ratatui::TestBackend`).
///
/// This is exactly what the production `run_interactive_with` does each tick: it
/// overlays `PluginHost::plugin_components()` onto the durable snapshot via
/// [`App::overlay_plugins`] before drawing, so a rendered frame proves the
/// unknown plugin's live health reaches the shipped UI.
fn overlay_frame(host: &PluginHost, state: &Path) -> String {
    let mut app = App::new(read_model(state), Transports::all_in_process(), 1000);
    app.overlay_plugins(host.plugin_components());
    render_to_string(&app, 160, 80)
}

/// The structured failure code of a registry rejection.
fn code(err: &agalma_contracts::ContractError) -> FailureCode {
    failure_code(err).expect("registry failures carry a structured code")
}

// ---------------------------------------------------------------------------
// Case 1: the shipped production host installs an unknown package and exits
// ---------------------------------------------------------------------------

#[test]
fn production_cli_host_installs_an_unknown_package_and_exits_cleanly() {
    let digest_before = hub_digest();
    let scratch = scratch("hp8-cli-install");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let package_dir = root.join(PLUGIN_ID);
    let built = build_fixture();
    let manifest = assemble_package(&package_dir, &built);
    assert_eq!(manifest.plugin_id, PLUGIN_ID);
    assert!(manifest.package_digest.starts_with("sha256:"));

    let output = Command::new(hub_bin())
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .args([
            "plugins",
            "--once",
            "--state-dir",
            state.to_str().unwrap(),
            "--plugin-root",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("run agalma plugins --once");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "production `agalma plugins --once` must exit cleanly (exit {:?}); stderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("roots=1") && stderr.contains("installed=1"),
        "the configured root installs exactly one package; stderr:\n{stderr}"
    );

    // The hub binary is byte-identical to the digest pinned before any plugin
    // generation was built.
    assert_hub_unchanged(&digest_before);

    // The selected ledger records the installed generation durably.
    let mut ledger = LedgerHandle::from_transport_existing(&state, LedgerTransport::InProcess)
        .unwrap_or_else(|err| panic!("selected ledger missing: {err:?}; stderr:\n{stderr}"));
    let (_, checkpoint) = read_registry_checkpoint(&mut ledger).expect("registry checkpoint");
    assert_eq!(
        checkpoint.active_generations.get(PLUGIN_ID).copied(),
        Some(1),
        "the production host durably records the installed generation: {checkpoint:?}"
    );
}

// ---------------------------------------------------------------------------
// Case 2: an unknown plugin reports data through the production read model
// ---------------------------------------------------------------------------

#[test]
fn unknown_plugin_reports_data_through_the_generic_read_model() {
    let digest_before = hub_digest();
    let scratch = scratch("hp8-report");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (host, _probe, _ledger) = start_production_host(&rt, &state, &root, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));

    let snapshot = read_model(&state);
    let component = find_plugin(&snapshot);
    assert_eq!(component.generation, Some(1), "active generation");
    assert_ne!(component.lifecycle, "failed", "the plugin did not fail");
    assert!(
        !component.disconnected,
        "a freshly reporting plugin is not disconnected"
    );
    assert!(
        component.freshness == "fresh",
        "a just-ingested report is fresh: {}",
        component.freshness
    );

    // The declared view renders the real plugin payload (not a placeholder).
    let view = component
        .views
        .iter()
        .find(|view| view.schema_namespace == REPORT_NAMESPACE)
        .expect("the plugin's declared view is present");
    assert_eq!(view.state, "rendered", "the plugin data is rendered: {view:?}");
    let rendered = format!("{:?}", view.blocks);
    assert!(
        rendered.contains("\"items\"") && rendered.contains("1"),
        "the generic metrics block shows the plugin's reported data: {rendered}"
    );

    // Both declared actions are advertised read-only (execution is case 3).
    let action_ids: BTreeSet<&str> = component
        .actions
        .iter()
        .map(|action| action.action_id.as_str())
        .collect();
    assert!(
        action_ids.contains(FLUSH_ACTION) && action_ids.contains(DANGER_ACTION),
        "both declared actions are advertised: {action_ids:?}"
    );

    // The live production read model exposes the plugin's exact host-pinned
    // identity with a Ready health view and its detail. HP.4 records the typed
    // `plugin.Health` observation into the host's attached ingestor;
    // `PluginHost::plugin_components()` renders it as `PluginComponent.health`.
    let pinned = host.pinned_binding(PLUGIN_ID).expect("host-pinned binding");
    let live = live_component(&host);
    assert_eq!(live.plugin_id, PLUGIN_ID, "read model plugin identity");
    assert_eq!(
        live.generation,
        Some(pinned.generation),
        "the read model reflects the exact host-pinned binding generation"
    );
    assert_eq!(
        live.lifecycle, "ready",
        "the health gate marked the plugin ready"
    );
    let health = live
        .health
        .as_ref()
        .expect("the live read model exposes a plugin health view");
    assert_eq!(health.state, "ready", "the health view reports Ready");
    assert!(
        health.detail.contains("plugin-pack serving"),
        "the plugin's health detail is preserved: {}",
        health.detail
    );

    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 3: permitted action runs once, denied action zero effects — through the
// real UI key path and the production service action driver
// ---------------------------------------------------------------------------

/// Select the plugin's `action_id` in the UI app and press `a`.
fn dispatch_action(
    state: &Path,
    driver: &mut dyn ActionDriver,
    action_id: &str,
) -> (Option<String>, bool) {
    let snapshot = read_model(state);
    let component = find_plugin(&snapshot);
    let plugin_index = snapshot
        .plugins
        .iter()
        .position(|c| c.plugin_id == PLUGIN_ID)
        .expect("plugin row");
    let action_index = component
        .actions
        .iter()
        .position(|a| a.action_id == action_id)
        .unwrap_or_else(|| panic!("action `{action_id}` advertised: {:?}", component.actions));
    let mut app = App::new(snapshot, Transports::all_in_process(), 1000);
    app.plugin_selection = plugin_index;
    app.action_selection = action_index;
    let mut control = NoopControl;
    app.handle_key_with(key(KeyCode::Char('a')), &mut control, driver);
    (app.status.clone(), app.refresh_requested)
}

#[test]
fn permitted_action_runs_once_and_denied_action_has_zero_effects() {
    let _serial = lock_child_env();
    let digest_before = hub_digest();
    let scratch = scratch("hp8-actions");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);

    let journal = scratch.child("effects").join("plugin-effects.jsonl");
    std::env::set_var(EFFECT_JOURNAL_ENV, &journal);

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (host, _probe, _ledger) = start_production_host(&rt, &state, &root, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));

    let mut driver = host
        .action_driver()
        .expect("build production action driver")
        .expect("the host has a selected ledger, so the driver is attached");

    // Permitted action: exactly one independent effect and a terminal status.
    let (status, refreshed) = dispatch_action(&state, &mut *driver, FLUSH_ACTION);
    let effects = read_effects(&journal);
    assert_eq!(
        effects.len(),
        1,
        "the permitted action runs exactly once; status={status:?}"
    );
    assert_eq!(effects[0].1, 1, "the effect is pinned to generation 1");
    assert!(refreshed, "a dispatched action refreshes the view");
    let status = status.unwrap_or_default();
    assert!(
        status.contains(FLUSH_ACTION) && status.contains("Completed"),
        "the UI surfaces the terminal outcome: {status}"
    );

    // Denied action: no effect, an enforcement status, no refresh.
    let (denied_status, denied_refreshed) =
        dispatch_action(&state, &mut *driver, DANGER_ACTION);
    assert_eq!(
        read_effects(&journal).len(),
        1,
        "the denied action performs zero effects"
    );
    assert!(!denied_refreshed, "a denied action does not refresh");
    let denied_status = denied_status.unwrap_or_default();
    assert!(
        denied_status.contains(DANGER_ACTION) && denied_status.contains("error"),
        "the UI surfaces the denial: {denied_status}"
    );

    // The permitted effect's durable receipt is recoverable from the selected
    // ledger through the production action service.
    let op_id = effects[0].0.clone();
    let service = host
        .action_service_with_ledger(&_ledger)
        .expect("reopen production action service over the selected ledger");
    let record = service
        .record(PLUGIN_ID, 1, &op_id)
        .expect("read journaled operation")
        .expect("the permitted operation is journaled");
    assert!(
        record.receipt.is_some(),
        "a completed action carries a durable receipt: {record:?}"
    );

    // The hub binary is byte-identical to the digest pinned before any plugin
    // generation was built.
    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 4: a child crash/restart preserves the plugin identity and its receipts
// (this is a *child* restart under one long-lived host — the real hub
// reassembly case is case 8)
// ---------------------------------------------------------------------------

/// Kill the owned child (its exit is observed but not yet reaped).
fn kill_child(host: &PluginHost) {
    let pid = host.supervisor.pid(PLUGIN_ID).expect("owned child pid");
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill plugin child");
    assert!(status.success(), "the plugin child is killed");
    std::thread::sleep(Duration::from_millis(150));
}

/// Reap a killed child and drive one production restart under backoff.
fn restart_after_crash(
    host: &mut PluginHost,
    probe: &mut RpcHealthProbe,
    clock: &ManualClock,
) {
    // Observe the crash (schedules the backoff restart), then advance to due.
    let _ = host.supervise_tick(probe).expect("supervise tick after crash");
    clock.advance(2_000);
    let outcome = host.supervise_tick(probe).expect("supervise tick at due");
    assert!(
        !outcome.restarted.is_empty(),
        "the crashed plugin restarts under backoff: {outcome:?}"
    );
    host.refresh_action_sockets();
}

/// Kill the owned child and drive one production restart under backoff.
fn crash_and_restart(
    host: &mut PluginHost,
    probe: &mut RpcHealthProbe,
    clock: &ManualClock,
) {
    kill_child(host);
    restart_after_crash(host, probe, clock);
}

#[test]
fn child_restart_preserves_plugin_identity_and_receipts() {
    let _serial = lock_child_env();
    let digest_before = hub_digest();
    let scratch = scratch("hp8-restart");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);
    let journal = scratch.child("effects").join("plugin-effects.jsonl");
    std::env::set_var(EFFECT_JOURNAL_ENV, &journal);

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (mut host, mut probe, ledger) = start_production_host(&rt, &state, &root, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));

    let mut driver = host.action_driver().unwrap().unwrap();
    let (_, _) = dispatch_action(&state, &mut *driver, FLUSH_ACTION);
    let first = read_effects(&journal);
    assert_eq!(first.len(), 1, "the first action runs once");
    let first_op = first[0].0.clone();

    // Identity before the crash.
    let generation_before = host
        .registry
        .entry(PLUGIN_ID)
        .and_then(|entry| entry.active_generation);
    assert_eq!(generation_before, Some(1));

    crash_and_restart(&mut host, &mut probe, &clock);

    // Identity is preserved: same plugin id, same pinned generation.
    let entry = host.registry.entry(PLUGIN_ID).expect("plugin entry");
    assert_eq!(entry.plugin_id, PLUGIN_ID);
    assert_eq!(entry.active_generation, Some(1), "pinned generation preserved");
    assert!(entry.launch_epoch >= 2, "the restart advanced the launch epoch");

    // A second dispatch runs once, under a distinct operation identity.
    let (status_after, _) = dispatch_action(&state, &mut *driver, FLUSH_ACTION);
    let after = read_effects(&journal);
    assert_eq!(after.len(), 2, "one effect per dispatch across the restart");
    assert!(after.iter().all(|(_, generation)| *generation == 1));
    let mut op_ids: BTreeSet<&str> = BTreeSet::new();
    for (op_id, _) in &after {
        assert!(op_ids.insert(op_id), "operation identity is unique: {op_id}");
    }
    assert!(
        !status_after.unwrap_or_default().contains("error"),
        "the post-restart action completes"
    );

    // The pre-restart receipt is still durable and terminal.
    let service = host.action_service_with_ledger(&ledger).unwrap();
    let record = service
        .record(PLUGIN_ID, 1, &first_op)
        .unwrap()
        .expect("the first operation survives the restart");
    assert!(record.receipt.is_some(), "its receipt is durable: {record:?}");

    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 4b: the supervisor's own crash observation drives the live production
// read model from Ready -> failed/disconnected -> Ready on restart.
//
// The failure state is **not** synthesized: no test-side
// `record_health(Unavailable)` or `confirm_ready_health` is injected. The killed
// child is observed by the real `PluginHost::supervise_tick` (`reap_exited` ->
// `handle_crash` -> `registry.mark_failed`), and the pinned-generation
// `PluginComponent` is read through the shipped `PluginHost::plugin_components()`
// -- the exact panel the watch UI overlays (HP.6). Recovery is the real
// supervisor restart + the identity-checked `RpcHealthProbe` gate inside
// `supervise_tick`.
// ---------------------------------------------------------------------------

#[test]
fn supervisor_observed_crash_then_restart_drives_the_live_read_model() {
    let digest_before = hub_digest();
    let scratch = scratch("hp8-health");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (mut host, mut probe, _ledger) = start_production_host(&rt, &state, &root, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));

    // Baseline: the live production read model exposes Ready for the pinned
    // generation (the launch health gate recorded the Ready observation).
    let pinned = host.pinned_binding(PLUGIN_ID).expect("host-pinned binding");
    let before = live_component(&host);
    assert_eq!(before.generation, Some(pinned.generation));
    assert!(!before.disconnected, "baseline is connected: {before:?}");
    assert_eq!(before.lifecycle, "ready", "baseline is ready: {before:?}");
    // The shipped watch overlay renders the unknown plugin's live Ready health.
    let ready_frame = overlay_frame(&host, &state);
    assert!(
        ready_frame.contains(PLUGIN_ID) && ready_frame.contains("health: ready"),
        "the rendered watch UI shows the unknown plugin's live Ready health: {ready_frame}"
    );
    assert!(!ready_frame.contains("DISCONNECTED"), "{ready_frame}");

    // Kill the owned child. Nothing probes or records health afterwards: the
    // *supervisor's own* tick must observe the exit.
    kill_child(&host);

    // One production tick observes the crash (`reap_exited` -> `handle_crash`)
    // and schedules the backoff restart; it is not yet due, so the supervisor
    // has observed the failure but has not restarted the child.
    let observed = host
        .supervise_tick(&mut probe)
        .expect("supervise tick observes the crash");
    assert!(
        !observed.crashed.is_empty(),
        "the supervisor observes the exited child: {observed:?}"
    );
    assert!(
        observed.restarted.is_empty(),
        "the restart is scheduled under backoff, not immediate: {observed:?}"
    );

    // The live read model now reflects the supervisor-observed failure with no
    // injected observation: the plugin is failed/disconnected at its pinned
    // generation.
    let crash = live_component(&host);
    assert_eq!(
        crash.generation,
        Some(pinned.generation),
        "the crash never changes the pinned generation: {crash:?}"
    );
    assert_eq!(
        crash.lifecycle, "failed",
        "the supervisor's crash observation marks the plugin failed: {crash:?}"
    );
    assert!(
        crash.disconnected,
        "the read model flags the supervisor-observed disconnect: {crash:?}"
    );
    assert!(
        crash
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("child exited")),
        "the supervisor's crash reason is surfaced: {crash:?}"
    );

    // The shipped watch overlay renders the unknown plugin's live failure: the
    // host panel is overlaid through the production `App::overlay_plugins` and
    // drawn through `render_to_string` (the exact `run_interactive_with` path
    // applies each tick), with no test-side health injection.
    let crash_frame = overlay_frame(&host, &state);
    assert!(
        crash_frame.contains(PLUGIN_ID),
        "the rendered watch UI shows the unknown plugin: {crash_frame}"
    );
    assert!(
        crash_frame.contains("DISCONNECTED"),
        "the rendered watch UI flags the supervisor-observed disconnect: {crash_frame}"
    );
    assert!(
        crash_frame.contains("child exited"),
        "the rendered watch UI shows the supervisor's crash reason: {crash_frame}"
    );

    // Recovery: advance to the backoff due time; the production supervisor
    // restarts the child and re-runs the identity-checked health gate.
    clock.advance(2_000);
    let restarted = host
        .supervise_tick(&mut probe)
        .expect("supervise tick at due");
    assert!(
        !restarted.restarted.is_empty(),
        "the crashed plugin restarts under backoff: {restarted:?}"
    );
    host.refresh_action_sockets();

    let recovered = live_component(&host);
    assert_eq!(recovered.generation, Some(pinned.generation));
    assert!(
        !recovered.disconnected,
        "recovery clears the supervisor-observed disconnect: {recovered:?}"
    );
    assert_eq!(
        recovered.lifecycle, "ready",
        "recovery restores Ready: {recovered:?}"
    );
    let recovered_health = recovered
        .health
        .as_ref()
        .expect("the live read model exposes a recovered health view");
    assert_eq!(
        recovered_health.state, "ready",
        "recovery restores a Ready health observation: {recovered_health:?}"
    );

    // The rendered watch UI clears the disconnect and shows Ready again.
    let recovered_frame = overlay_frame(&host, &state);
    assert!(
        recovered_frame.contains("health: ready"),
        "the rendered watch UI shows the recovered Ready health: {recovered_frame}"
    );
    assert!(
        !recovered_frame.contains("DISCONNECTED"),
        "recovery clears the rendered disconnect: {recovered_frame}"
    );

    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 5: production upgrade/rollback over real processes — a delayed action
// pinned to g1 drains g1 and stages g2; while the pin is unresolved the registry
// refuses to remove g1 (`EffectUnconfirmed`); the pinned work reconciles on g1
// (never the g2 replacement) and only then does g2 health-gate active; rollback
// restores g1 and a further action completes; the independent journal matches the
// durable receipts.
// ---------------------------------------------------------------------------

#[test]
fn upgrade_reconciles_pinned_g1_work_and_rollback_serves_a_further_action() {
    let _serial = lock_child_env();
    let digest_before = hub_digest();
    let scratch = scratch("hp8-replace");
    let root1 = scratch.child("root1");
    let root2 = scratch.child("root2");
    let state = scratch.child("state");
    let built = build_fixture();
    let manifest1 = assemble_package(&root1.join(PLUGIN_ID), &built);
    let manifest2 = assemble_package_with_version(&root2.join(PLUGIN_ID), &built, Some("0.2.0"));
    assert_ne!(
        manifest1.package_digest, manifest2.package_digest,
        "the two generations are distinct sealed packages"
    );

    let journal = scratch.child("effects").join("plugin-effects.jsonl");
    std::env::set_var(EFFECT_JOURNAL_ENV, &journal);
    // The first generation withholds the terminal `flush` report, so the hub
    // parks the operation unresolved on g1 and settles it via reconcile.
    std::env::set_var(DELAY_ACTIONS_ENV, FLUSH_ACTION);

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (mut host, mut probe, ledger) = start_production_host(&rt, &state, &root1, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));
    let schema_before = ledger.schema_version().expect("schema version");

    let g1 = *host.installed.get(PLUGIN_ID).expect("g1 installed");
    assert_eq!(g1, 1);
    let pin1 = host.pinned_binding(PLUGIN_ID).expect("g1 pin");
    assert_eq!(pin1.generation, 1);
    assert_eq!(pin1.implementation_version, "0.1.0");

    // A real delayed action on the live g1 process: the effect is applied but no
    // terminal report is delivered, so the hub parks it unresolved on g1.
    let mut driver = host.action_driver().expect("driver").expect("ledger-backed driver");
    let (status, _) = dispatch_action(&state, &mut *driver, FLUSH_ACTION);
    assert!(
        !status.as_deref().unwrap_or_default().contains("error"),
        "the delayed action is accepted"
    );
    let after_dispatch = read_effects(&journal);
    assert_eq!(after_dispatch.len(), 1, "the delayed effect is applied exactly once");
    let op1 = after_dispatch[0].0.clone();
    assert_eq!(after_dispatch[0].1, 1, "the effect is pinned to g1");
    let mut service = host.action_service_with_ledger(&ledger).expect("action service");
    let unresolved = service
        .record(PLUGIN_ID, 1, &op1)
        .expect("read journaled operation")
        .expect("the delayed operation is journaled");
    assert!(
        unresolved.receipt.is_none(),
        "the delayed action has no terminal receipt yet: {unresolved:?}"
    );

    // The unresolved operation is pinned work on g1 in the registry's pin ledger
    // (the platform's removal/drain guard; the HP.7 action service journals the
    // operation in its own component store).
    host.registry
        .pin_operation(PLUGIN_ID, g1, &op1)
        .expect("pin the unresolved g1 operation");
    assert!(
        host.registry.entry(PLUGIN_ID).unwrap().has_unresolved_pins(g1),
        "g1 carries the unresolved pinned operation"
    );

    // Begin draining the active g1 generation. New work switches to a candidate
    // only after it is Ready and activation is receipted, so the pinned g1 work
    // stays reachable on g1 throughout the drain.
    let now = host.supervisor.now_ms();
    host.registry.begin_drain(PLUGIN_ID, g1, now).expect("drain g1");
    assert_eq!(
        host.registry.entry(PLUGIN_ID).unwrap().observed,
        LifecycleState::Draining,
        "the active g1 generation is draining"
    );

    // Upgrade: install the sealed g2 into the same production registry and stage
    // it as the replacement candidate.
    let discovered2 = discover_plugins(&[PluginRoot::required(&root2)]).expect("discover g2");
    let package2 = discovered2
        .packages
        .iter()
        .find(|package| package.plugin_id == PLUGIN_ID)
        .expect("g2 package");
    let installation2 = default_installation(package2).expect("g2 default pin");
    let g2 = host
        .registry
        .install(&host.install_store, package2, &installation2)
        .expect("install g2");
    assert_eq!(g2, 2, "the replacement is a new monotonic generation");
    host.registry.stage(PLUGIN_ID, g2).expect("stage g2");

    // Unresolved g1 work blocks unsafe removal: the registry refuses to remove a
    // generation with unresolved pins. Staging alone does not switch the active
    // generation, so the pinned g1 work stays reachable and is never redirected.
    let blocked = host
        .registry
        .remove(PLUGIN_ID, g1, now)
        .expect_err("removing a pinned generation is refused");
    assert_eq!(
        code(&blocked),
        FailureCode::EffectUnconfirmed,
        "unresolved pinned work refuses removal: {blocked:?}"
    );
    assert_eq!(
        host.registry.entry(PLUGIN_ID).unwrap().active_generation,
        Some(g1),
        "the staged g2 candidate has not activated while g1 work is unresolved"
    );
    assert_eq!(
        host.registry.entry(PLUGIN_ID).unwrap().pending_generation,
        Some(g2),
        "g2 is staged as the pending candidate"
    );
    assert!(
        host.registry.entry(PLUGIN_ID).unwrap().has_unresolved_pins(g1),
        "the g1 pin is still unresolved"
    );

    // Reconcile the pinned g1 work against g1 (the still-live generation): it
    // settles exactly once, repeats no effect, and resolves the registry pin.
    let settled = service.recover().expect("reconcile pinned work");
    assert_eq!(settled.len(), 1, "the pinned g1 operation reconciles");
    assert_eq!(settled[0].state, ComponentOperationState::Completed);
    assert_eq!(settled[0].intent.binding, pin1, "settled on the g1 pin");
    assert!(settled[0].receipt.is_some(), "reconcile produces a terminal receipt");
    assert_eq!(read_effects(&journal).len(), 1, "reconcile does not repeat the effect");
    host.registry
        .resolve_pin(PLUGIN_ID, g1, &op1)
        .expect("resolve the settled g1 pin");
    assert!(
        !host.registry.entry(PLUGIN_ID).unwrap().has_unresolved_pins(g1),
        "the g1 pin is resolved after reconcile"
    );
    // Only then does the pin guard clear: g1 is refused solely because it is the
    // active generation, proving the earlier refusal was the unresolved pin.
    let still_active = host
        .registry
        .remove(PLUGIN_ID, g1, now)
        .expect_err("the active generation is never removable");
    assert_eq!(
        code(&still_active),
        FailureCode::RevisionConflict,
        "with the pin resolved the only refusal is the active generation: {still_active:?}"
    );

    // Allow g2 health/activation through the production supervisor: stop g1's
    // owned child and start the staged candidate, which must health-gate Ready.
    let _ = host.shutdown();
    host.refresh_action_sockets();
    let started = host.start_desired(&mut probe).expect("start g2");
    assert_eq!(started.len(), 1, "the g2 candidate starts");
    assert_eq!(
        host.registry.entry(PLUGIN_ID).unwrap().active_generation,
        Some(g2),
        "g2 becomes active after its health gate passes"
    );
    let pin2 = host.pinned_binding(PLUGIN_ID).expect("g2 pin");
    assert_eq!(pin2.generation, 2);
    assert_eq!(
        pin2.implementation_version, manifest2.package_version,
        "the pinned version is the exact replacement package version, not a default"
    );
    assert_eq!(
        pin2.implementation_version, "0.2.0",
        "the fixture announces its real installed version"
    );
    host.refresh_action_sockets();

    // The reconciled g1 work is never redirected: the g1 receipt survives and a
    // reconcile pass after g2 is active settles nothing new and repeats no effect.
    let mut service_after =
        host.action_service_with_ledger(&ledger).expect("action service after g2");
    let leftover = service_after.recover().expect("reconcile after g2");
    assert!(
        leftover.is_empty(),
        "no unsettled or redirected work after g2 activates"
    );
    let surviving = service_after
        .record(PLUGIN_ID, 1, &op1)
        .expect("read g1 operation")
        .expect("the g1 receipt survives the upgrade");
    assert!(surviving.receipt.is_some());
    assert_eq!(read_effects(&journal).len(), 1, "no effect during the g2 transition");

    // Rollback: clear the delay so the restored g1 serves actions immediately,
    // re-stage g1, stop g2 and start g1 through the production supervisor.
    std::env::remove_var(DELAY_ACTIONS_ENV);
    host.registry.stage(PLUGIN_ID, g1).expect("re-stage g1");
    let _ = host.shutdown();
    host.refresh_action_sockets();
    let rolled = host.start_desired(&mut probe).expect("start g1 rollback");
    assert_eq!(rolled.len(), 1, "the g1 rollback candidate starts");
    let entry = host.registry.entry(PLUGIN_ID).unwrap();
    assert_eq!(entry.active_generation, Some(g1), "rollback restores g1");
    assert_eq!(
        entry.generations.get(&g1).unwrap().package_digest,
        manifest1.package_digest,
        "the restored generation is the previous sealed package"
    );
    assert_eq!(
        host.pinned_binding(PLUGIN_ID).unwrap().implementation_version,
        "0.1.0"
    );
    host.refresh_action_sockets();

    // A further action on the restored g1 completes immediately and is a distinct
    // durable effect.
    let mut driver2 = host.action_driver().expect("driver").expect("ledger-backed driver");
    let (status2, _) = dispatch_action(&state, &mut *driver2, FLUSH_ACTION);
    assert!(
        status2.as_deref().unwrap_or_default().contains("Completed"),
        "the further g1 action completes: {status2:?}"
    );
    let effects_after = read_effects(&journal);
    assert_eq!(effects_after.len(), 2, "the further action is a second distinct effect");
    assert_eq!(effects_after[1].1, 1, "the further action is pinned to g1");
    assert_ne!(
        effects_after[0].0, effects_after[1].0,
        "distinct operation identity per dispatch"
    );
    let op2 = effects_after[1].0.clone();

    // Independent journal vs durable receipts: two effects, both terminal.
    let mut service_final =
        host.action_service_with_ledger(&ledger).expect("final action service");
    let r2 = service_final
        .record(PLUGIN_ID, 1, &op2)
        .expect("read op2")
        .expect("op2 is journaled");
    assert!(r2.receipt.is_some(), "the further action carries a durable receipt");
    let r1 = service_final
        .record(PLUGIN_ID, 1, &op1)
        .expect("read op1")
        .expect("op1 is journaled");
    assert!(r1.receipt.is_some(), "the reconciled g1 receipt is durable");
    let remaining = service_final.recover().expect("final reconcile");
    assert!(remaining.is_empty(), "no unsettled work remains");

    // No schema rewind and an unchanged hub binary.
    assert_eq!(
        ledger.schema_version().unwrap(),
        schema_before,
        "the ledger schema is never rewound"
    );
    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 6: missing/unknown capabilities, a long state path, and a stale report
// ---------------------------------------------------------------------------

#[test]
fn missing_and_unknown_capabilities_are_never_auto_granted() {
    let requested = vec![
        "report.publish".to_string(),
        "ledger.read".to_string(),
        "ledger.write".to_string(),
        "mystery.widgets".to_string(),
    ];
    let approved = auto_approved_capabilities(&requested);
    assert_eq!(
        approved,
        vec!["report.publish".to_string(), "ledger.read".to_string()],
        "the automatic grant is exactly requested ∩ the safe allowlist"
    );
    assert!(!SAFE_CAPABILITY_ALLOWLIST.contains(&"ledger.write"));
    assert!(!SAFE_CAPABILITY_ALLOWLIST.contains(&"mystery.widgets"));
    assert!(!approved.iter().any(|c| c == "ledger.write"));
    assert!(!approved.iter().any(|c| c == "mystery.widgets"));
}

#[test]
fn long_state_path_still_launches_and_reports() {
    let _serial = lock_child_env();
    let scratch = scratch("hp8-long-path");
    let root = scratch.child("root");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);

    // A state dir long enough that the direct socket path would exceed the
    // SUN_LEN-safe limit, forcing the short hashed fallback.
    let state = scratch.child("deep").join("d".repeat(96));
    std::fs::create_dir_all(&state).unwrap();
    assert!(
        plugin_socket_path(&state, PLUGIN_ID, 1).as_os_str().len() <= SOCKET_PATH_LIMIT,
        "the long state path still yields a SUN_LEN-safe socket"
    );

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (host, _probe, _ledger) = start_production_host(&rt, &state, &root, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));
    let snapshot = read_model(&state);
    let component = find_plugin(&snapshot);
    assert_eq!(component.generation, Some(1));
}

#[test]
fn stale_report_is_marked_stale_in_the_read_model() {
    let scratch = scratch("hp8-stale");
    let state = scratch.child("state");
    let ledger = LedgerHandle::in_process(&state).expect("selected ledger");

    let t0 = 1_800_000_000_000u64;
    let now = Arc::new(AtomicU64::new(t0));
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let mut active = BTreeMap::new();
    active.insert(PLUGIN_ID.to_string(), 1u64);
    let mut allowed = BTreeSet::new();
    allowed.insert((REPORT_NAMESPACE.to_string(), 1u64));
    let mut ingestor = ReportIngestor::open(
        Box::new(Mutex::new(ledger.clone())),
        clock,
        active,
        allowed,
    )
    .expect("report ingestor");

    let binding = BindingPin {
        binding_id: PLUGIN_ID.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "0.1.0".to_string(),
        generation: 1,
        config_digest: format!("sha256:{}", "0".repeat(64)),
    };
    let publication = seal_publication(
        PLUGIN_ID,
        binding.clone(),
        REPORT_NAMESPACE,
        1,
        1,
        t0,
        Durability::Durable,
        json!({"items": 1, "ready": true}),
    )
    .expect("sealed publication");
    ingestor
        .publish(PLUGIN_ID, &binding, &publication)
        .expect("ingest report");
    assert_eq!(ingestor.freshness(PLUGIN_ID), Freshness::Fresh);

    // Advance the injected clock past the stale threshold (default 15s) but
    // before the failed threshold (30s).
    now.store(t0 + 20_000, Ordering::SeqCst);
    assert_eq!(ingestor.freshness(PLUGIN_ID), Freshness::Stale);

    let sources = vec![PluginSource {
        plugin_id: PLUGIN_ID.to_string(),
        origin: ComponentOrigin::Discovered,
        desired: DesiredState::Running,
        observed: LifecycleState::Ready,
        generation: Some(1),
        last_error: None,
        manifest: None,
    }];
    let components = derive_plugin_components_from_sources(&sources, Some(&ingestor));
    let component = components
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("plugin component");
    assert!(component.stale, "a stale report marks the plugin stale");
    assert_eq!(component.freshness, "stale");
}

// ---------------------------------------------------------------------------
// Case 7: deterministic property over generated action/restart sequences
// ---------------------------------------------------------------------------

/// Recorded deterministic property-test seed (SplitMix64).
const PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_00a8;
/// Recorded property-test case budget.
const PROPERTY_CASES: u64 = 16;
/// Recorded deterministic seed for the delayed-action reconcile property.
const DELAY_PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_00a9;
/// Recorded case budget for the delayed-action reconcile property.
const DELAY_PROPERTY_CASES: u64 = 12;

/// SplitMix64 generator (recorded seed; deterministic replay).
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

#[test]
fn property_effect_oracle_over_generated_action_and_restart_sequences() {
    let _serial = lock_child_env();
    let digest_before = hub_digest();
    let scratch = scratch("hp8-property");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);
    let journal = scratch.child("effects").join("plugin-effects.jsonl");
    std::env::set_var(EFFECT_JOURNAL_ENV, &journal);

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (mut host, mut probe, _ledger) = start_production_host(&rt, &state, &root, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));
    let mut driver = host.action_driver().unwrap().unwrap();

    // Independent oracle: exactly one effect per allowed dispatch, zero for a
    // denied dispatch, and a distinct operation identity per dispatch.
    let mut allowed = 0usize;
    let mut operation_ids: BTreeSet<String> = BTreeSet::new();
    let mut rng = SplitMix64::new(PROPERTY_SEED);

    for case in 0..PROPERTY_CASES {
        let len = 1 + rng.below(4);
        for step in 0..len {
            match rng.below(4) {
                // Permitted action (`flush`, grants `ledger.read`).
                0 | 1 => {
                    let before = read_effects(&journal).len();
                    let (status, refreshed) = dispatch_action(&state, &mut *driver, FLUSH_ACTION);
                    let after = read_effects(&journal);
                    assert_eq!(
                        after.len(),
                        before + 1,
                        "case {case} step {step}: exactly one new effect per allowed dispatch"
                    );
                    let (op_id, generation) = after.last().unwrap();
                    assert_eq!(*generation, 1, "the effect is pinned to generation 1");
                    assert!(
                        operation_ids.insert(op_id.clone()),
                        "case {case} step {step}: distinct operation identity ({op_id})"
                    );
                    allowed += 1;
                    assert!(refreshed);
                    assert!(
                        !status.unwrap_or_default().contains("error"),
                        "case {case} step {step}: the permitted action completes"
                    );
                }
                // Denied action (`danger`, requires the ungranted `ledger.write`).
                2 => {
                    let before = read_effects(&journal).len();
                    let (status, refreshed) = dispatch_action(&state, &mut *driver, DANGER_ACTION);
                    assert_eq!(
                        read_effects(&journal).len(),
                        before,
                        "case {case} step {step}: a denied action has zero effects"
                    );
                    assert!(!refreshed);
                    assert!(status.unwrap_or_default().contains("error"));
                }
                // Crash and restart under the production supervisor.
                _ => crash_and_restart(&mut host, &mut probe, &clock),
            }
        }
    }

    assert!(allowed > 0, "the property exercised real dispatches");
    assert_eq!(
        read_effects(&journal).len(),
        allowed,
        "exactly one durable effect per allowed dispatch across all cases"
    );
    assert_eq!(
        operation_ids.len(),
        allowed,
        "every dispatch carries a distinct operation identity"
    );

    // The hub binary is byte-identical to the digest pinned before any plugin
    // generation was built (no hub rebuild across the generated sequences).
    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 8: a real hub reassembly must not reuse a generation for a replacement
// ---------------------------------------------------------------------------
//
// Drives the shipped `agalma plugins --once` binary twice over one selected
// ledger with a replaced package. After the HP.5 registry-restore repair
// (`3eabeed`) the previous generation is preserved and the replacement is
// allocated a strictly higher never-reused generation; before that repair this
// case reproduced the reuse/redirection defect (generation 1 overwritten by the
// replacement).

/// Run the shipped `agalma plugins --once` host over one configured root.
fn run_cli_host(root: &Path, state: &Path) -> std::process::Output {
    Command::new(hub_bin())
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .args([
            "plugins",
            "--once",
            "--state-dir",
            state.to_str().unwrap(),
            "--plugin-root",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("run agalma plugins --once")
}

/// The installed manifest currently stored for one generation.
fn installed_manifest(state: &Path, generation: u64) -> Option<PluginManifest> {
    let store = HubInstallStore::new(state);
    let path = store
        .generation_dir(PLUGIN_ID, generation)
        .join("package")
        .join(PLUGIN_MANIFEST_FILE);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The sealed package digest currently stored for one installed generation.
fn installed_digest(state: &Path, generation: u64) -> Option<String> {
    installed_manifest(state, generation).map(|manifest| manifest.package_digest)
}

/// The exact package version currently stored for one installed generation.
fn installed_version(state: &Path, generation: u64) -> Option<String> {
    installed_manifest(state, generation).map(|manifest| manifest.package_version)
}

#[test]
fn hub_reassembly_must_not_reuse_a_generation_for_a_replacement() {
    let digest_before = hub_digest();
    let scratch = scratch("hp8-hub-restart");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    let manifest1 = assemble_package(&root.join(PLUGIN_ID), &built);

    // First hub run installs generation 1 over the selected ledger.
    let out1 = run_cli_host(&root, &state);
    assert!(
        out1.status.success(),
        "first host run failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let mut ledger1 =
        LedgerHandle::from_transport_existing(&state, LedgerTransport::InProcess).unwrap();
    let (_, checkpoint1) = read_registry_checkpoint(&mut ledger1).unwrap();
    assert_eq!(
        checkpoint1.active_generations.get(PLUGIN_ID).copied(),
        Some(1)
    );
    assert_eq!(installed_digest(&state, 1).as_deref(), Some(manifest1.package_digest.as_str()));

    // Replace the package with a new sealed generation and restart the hub over
    // the same selected ledger.
    let manifest2 = assemble_package_with_version(&root.join(PLUGIN_ID), &built, Some("0.2.0"));
    assert_ne!(manifest1.package_digest, manifest2.package_digest);
    let out2 = run_cli_host(&root, &state);
    assert!(
        out2.status.success(),
        "second host run failed: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let mut ledger2 =
        LedgerHandle::from_transport_existing(&state, LedgerTransport::InProcess).unwrap();
    let (_, checkpoint2) = read_registry_checkpoint(&mut ledger2).unwrap();
    let observed_active = checkpoint2.active_generations.get(PLUGIN_ID).copied();
    let observed_max = checkpoint2.max_generations.get(PLUGIN_ID).copied();
    let retained_g1 = installed_digest(&state, 1);
    let allocated_g2 = installed_digest(&state, 2);
    let retained_g1_version = installed_version(&state, 1);
    let allocated_g2_version = installed_version(&state, 2);

    // Frozen contract: generations are monotonic and never reused. The
    // replacement received a strictly higher generation (2) and the previous
    // generation (1) is preserved as the previous package, so an operation
    // pinned to it is never redirected onto the replacement.
    assert_eq!(
        observed_max,
        Some(2),
        "the replacement after a hub restart must allocate a new monotonic \
         generation 2 (never reuse generation 1); observed max={observed_max:?} \
         active={observed_active:?}"
    );
    assert_eq!(
        allocated_g2.as_deref(),
        Some(manifest2.package_digest.as_str()),
        "generation 2 is the replacement package"
    );
    assert_eq!(
        retained_g1.as_deref(),
        Some(manifest1.package_digest.as_str()),
        "generation 1 must remain the previously installed package (an operation \
         pinned to it must not be redirected to the replacement); observed {retained_g1:?}"
    );
    // The stored generations report their exact package versions (read from the
    // installed package manifests, never a default): the retained generation
    // keeps the prior version and the replacement reports its own version.
    assert_eq!(
        retained_g1_version.as_deref(),
        Some(manifest1.package_version.as_str()),
        "generation 1 keeps its exact package version: {retained_g1_version:?}"
    );
    assert_eq!(
        allocated_g2_version.as_deref(),
        Some(manifest2.package_version.as_str()),
        "generation 2 reports the exact replacement package version: {allocated_g2_version:?}"
    );
    assert_eq!(
        allocated_g2_version.as_deref(),
        Some("0.2.0"),
        "the replacement package version is 0.2.0, not a hardcoded default"
    );

    // The replacement is staged, not active: the frozen contract switches new
    // work to a candidate only after it is Ready and activation is receipted
    // (`agalma plugins --once` does not start an auto-installed plugin), so the
    // prior generation stays active until then.
    assert_eq!(
        observed_active,
        Some(1),
        "the prior generation stays active while the replacement is a pending \
         candidate; observed active={observed_active:?}"
    );

    // Running the shipped CLI host twice must not rebuild the hub binary.
    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 9: hub reassembly preserves the action binding and its durable receipt.
// The first host is fully torn down (runtime, child, host, probe, action service,
// action driver and selected ledger all dropped) before a fresh runtime/host/
// ledger is assembled over the same durable state.
// ---------------------------------------------------------------------------

#[test]
fn hub_reassembly_preserves_action_binding_and_receipt_without_a_second_effect() {
    let _serial = lock_child_env();
    let digest_before = hub_digest();
    let scratch = scratch("hp8-hub-action");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);
    let journal = scratch.child("effects").join("plugin-effects.jsonl");
    std::env::set_var(EFFECT_JOURNAL_ENV, &journal);
    std::env::remove_var(DELAY_ACTIONS_ENV);

    let clock = ManualClock::new(1_800_000_000_000);

    // ---- First hub generation: one completed action, then a full teardown. ----
    //
    // Only the expected durable record identity (`pin_a`, `op`, `rec_a`) and the
    // on-disk fixture/state cross the restart boundary. Every in-process resource
    // the first host owns — its runtime (and the hub capability serve task it
    // spawned), owned child, host, health probe, action service, action driver and
    // selected `LedgerHandle` — is shut down and dropped inside this block before
    // the second host is constructed.
    let (pin_a, op, rec_a) = {
        let rt_a = hub_runtime();
        let (mut host_a, _probe_a, ledger_a) =
            start_production_host(&rt_a, &state, &root, clock.clock());
        wait_for_report(&host_a, Duration::from_secs(15));
        let pin_a = host_a.pinned_binding(PLUGIN_ID).expect("host A pin");
        let mut driver = host_a.action_driver().expect("driver").expect("ledger-backed driver");
        let (status, _) = dispatch_action(&state, &mut *driver, FLUSH_ACTION);
        assert!(
            status.as_deref().unwrap_or_default().contains("Completed"),
            "the pre-restart action completes: {status:?}"
        );
        let effects = read_effects(&journal);
        assert_eq!(effects.len(), 1, "one effect before the hub restart");
        let op = effects[0].0.clone();
        let service_a = host_a.action_service_with_ledger(&ledger_a).expect("action service A");
        let rec_a = service_a
            .record(PLUGIN_ID, pin_a.generation, &op)
            .expect("read op")
            .expect("op is journaled");
        assert!(rec_a.receipt.is_some(), "the action carries a durable receipt");

        drop(driver);
        drop(service_a);
        let _ = host_a.shutdown();
        drop(host_a);
        drop(ledger_a);
        // Dropping the runtime tears down the hub capability serve task and its
        // socket, so the second host binds fresh.
        drop(rt_a);
        (pin_a, op, rec_a)
    };

    // ---- Second hub generation: a fresh runtime, host and selected ledger over
    // the same durable on-disk state. ----
    let rt_b = hub_runtime();
    let (mut host_b, _probe_b, ledger_b) =
        start_production_host(&rt_b, &state, &root, clock.clock());

    // The exact pinned BindingPin survives the hub restart.
    let pin_b = host_b.pinned_binding(PLUGIN_ID).expect("host B pin");
    assert_eq!(pin_b, pin_a, "the exact binding pin survives hub reassembly");
    assert_eq!(pin_b.generation, pin_a.generation);

    // Query the post-restart service over the **reassembled** host's own selected
    // ledger handle (`ledger_b`), not the first host's handle. The full durable
    // record is byte-for-byte equal to the pre-restart record: same generation,
    // deadline, intent identity, terminal state and receipt content.
    let mut service_b = host_b
        .action_service_with_ledger(&ledger_b)
        .expect("action service B");
    let rec_b = service_b
        .record(PLUGIN_ID, pin_b.generation, &op)
        .expect("read surviving op")
        .expect("the receipt survives hub reassembly");
    assert_eq!(
        rec_b, rec_a,
        "the exact pre-restart durable record survives hub reassembly"
    );
    assert_eq!(
        rec_b.receipt.as_ref().map(|receipt| receipt.binding.clone()),
        Some(pin_a.clone()),
        "the surviving receipt is pinned to the exact binding"
    );
    let pending = service_b.recover().expect("reconcile after hub reassembly");
    assert!(
        pending.is_empty(),
        "a completed action is not re-run after a hub restart"
    );
    assert_eq!(
        read_effects(&journal).len(),
        1,
        "no second effect after hub reassembly"
    );

    drop(service_b);
    let _ = host_b.shutdown();

    assert_hub_unchanged(&digest_before);
}

// ---------------------------------------------------------------------------
// Case 10: deterministic property over delayed dispatch + reconcile
// ---------------------------------------------------------------------------

/// Every delayed dispatch applies exactly one effect, stays unresolved until
/// reconcile, and settles exactly once on its pinned generation — with no
/// repeated effect.
#[test]
fn property_delayed_actions_settle_once_on_their_pinned_generation() {
    let _serial = lock_child_env();
    let digest_before = hub_digest();
    let scratch = scratch("hp8-delay-prop");
    let root = scratch.child("root");
    let state = scratch.child("state");
    let built = build_fixture();
    assemble_package(&root.join(PLUGIN_ID), &built);
    let journal = scratch.child("effects").join("plugin-effects.jsonl");
    std::env::set_var(EFFECT_JOURNAL_ENV, &journal);
    std::env::set_var(DELAY_ACTIONS_ENV, FLUSH_ACTION);

    let rt = hub_runtime();
    let clock = ManualClock::new(1_800_000_000_000);
    let (host, _probe, ledger) = start_production_host(&rt, &state, &root, clock.clock());
    wait_for_report(&host, Duration::from_secs(15));
    let pin = host.pinned_binding(PLUGIN_ID).expect("pin");
    let mut driver = host.action_driver().expect("driver").expect("ledger-backed driver");

    let mut rng = SplitMix64::new(DELAY_PROPERTY_SEED);
    let mut dispatched = 0usize;
    let mut operation_ids: BTreeSet<String> = BTreeSet::new();
    for case in 0..DELAY_PROPERTY_CASES {
        let len = 1 + rng.below(3);
        for step in 0..len {
            let before = read_effects(&journal).len();
            let (status, _) = dispatch_action(&state, &mut *driver, FLUSH_ACTION);
            assert!(
                !status.unwrap_or_default().contains("error"),
                "case {case} step {step}: the delayed action is accepted"
            );
            let after = read_effects(&journal);
            assert_eq!(
                after.len(),
                before + 1,
                "case {case} step {step}: exactly one effect per dispatch"
            );
            let (op, generation) = after.last().unwrap().clone();
            assert_eq!(generation, pin.generation, "the effect is pinned to the live generation");
            assert!(
                operation_ids.insert(op.clone()),
                "case {case} step {step}: distinct operation identity ({op})"
            );
            dispatched += 1;

            let mut service = host.action_service_with_ledger(&ledger).expect("action service");
            let unresolved = service
                .record(PLUGIN_ID, pin.generation, &op)
                .expect("read op")
                .expect("the delayed op is journaled");
            assert!(
                unresolved.receipt.is_none(),
                "case {case} step {step}: the delayed op is unresolved before reconcile"
            );

            let settled = service.recover().expect("reconcile the pinned op");
            let mine: Vec<_> = settled
                .into_iter()
                .filter(|outcome| outcome.intent.operation_id == op)
                .collect();
            assert_eq!(
                mine.len(),
                1,
                "case {case} step {step}: the op reconciles exactly once"
            );
            assert!(mine[0].receipt.is_some(), "the reconcile produces a receipt");
            assert_eq!(
                mine[0].intent.binding, pin,
                "case {case} step {step}: settled on the pinned generation"
            );
            assert_eq!(
                read_effects(&journal).len(),
                dispatched,
                "case {case} step {step}: reconcile repeats no effect"
            );
        }
    }

    assert!(dispatched > 0, "the property exercised real dispatches");
    assert_eq!(
        read_effects(&journal).len(),
        dispatched,
        "exactly one durable effect per delayed dispatch"
    );
    assert_eq!(operation_ids.len(), dispatched, "distinct operation identity per dispatch");
    assert_hub_unchanged(&digest_before);
}
