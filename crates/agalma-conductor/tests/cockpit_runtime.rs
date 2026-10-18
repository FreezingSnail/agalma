//! WEB.13 (`agalma-rwn.13`) production-join regressions for the automatic-queue
//! cockpit.
//!
//! Source-level/process integration coverage for the composition-owned pieces
//! that join the reviewed WEB.9/10/11/12 work:
//!
//! * the production host-registration hook hands `HostSupervision`'s configured
//!   state directory to the registrar **unchanged** and installs the WEB.11
//!   factory read/control adapter on the served hub router;
//! * the launcher issues a generic capability-keyed private descriptor
//!   (`PluginCapabilityBindings/v1`) with distinct per-capability grants for
//!   `control-plane.read/v1`, `factory.read/v1` and `factory.control/v1`, while
//!   the legacy first-grant env keeps its documented behavior;
//! * the configured-root auto-install path stays read-only (no factory grant,
//!   no control binding, no descriptor);
//! * the launcher publishes/rotates the exact pinned control binding and the
//!   served router turns a real control grant into a durable receipt (with the
//!   trusted permission composer observable through `factory.read/v1`);
//! * an exclusive state-scoped control owner refuses a second owner;
//! * the factory stop exception is scoped to the declared factory capabilities.
//!
//! A deterministic seeded property test (seed `0x57454213`) drives generated
//! control/observation sequences and asserts no read effect, one claimant,
//! no unauthorized action, exact replay/unknown state, frozen recovery across a
//! restart, and that an unavailable source is never reported healthy.
//!
//! Ordinary fixture RPC does not establish the production browser acceptance;
//! that is `cockpit_queue` (WEB.13).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agalma_conductor::cli::PluginRootArgs;
use agalma_conductor::composition::{
    control_binding_for, register_work_plugin_host, FactoryControlOwner, InMemoryInstallations,
    PluginHost, HUB_SOCKET_ENV, PLUGIN_GRANT_CAPABILITY_ENV, PLUGIN_GRANT_ENV,
    SAFE_CAPABILITY_ALLOWLIST,
};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::factory_control::{
    ApprovedControlBinding, ControlBindingResolver, ControlOwnership, FactoryControlService,
};
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_router::{
    CapabilityCallRequest, CapabilityRouter, InMemoryOperationJournal, LaunchGrant, RunningStop,
};
use agalma_conductor::stop::{StopControl, StopDecision};
use agalma_conductor::work::factory_runtime::{
    FactoryRuntimeSource, HostRegistration, HostRegistrar, RuntimeControl, RuntimeShared,
    SupervisionHandle, WatchComposition,
};
use agalma_contracts::{
    digest::failure_code, ContractError, FactoryControlResponse, FactoryRuntime,
    FactoryRuntimeStatus, FailureCode, LedgerApi, PackageArtifactRef, PluginInstallation,
    PluginManifest, SourceState, CONTROL_PLANE_READ_CAPABILITY, FACTORY_CONTROL_CAPABILITY,
    FACTORY_READ_CAPABILITY,
};
use agalma_ipc::plugin_server::METHOD_INVOKE_CAPABILITY;
use agalma_ipc::{parse_capability_bindings, PLUGIN_CAPABILITY_BINDINGS_ENV};
use agalma_testkit::ScratchDir;

const PLUGIN_ID: &str = "example.cockpit";
const REPOSITORY_ID: &str = "repo-opaque";
const STATE_ID: &str = "state-opaque";
const FAR_FUTURE_MS: u64 = 10_000_000_000_000;

const PROD_CONFIG_SCHEMA: &[u8] = br#"{"$id":"cfg/v1","type":"object"}"#;
const PROD_DEFAULT_CONFIG: &[u8] = br#"{"n":1}"#;
const PROD_REPORT_SCHEMA: &[u8] = br#"{"$id":"rep/v1","type":"object"}"#;

// ---------------------------------------------------------------------------
// A sealed production plugin package
// ---------------------------------------------------------------------------

fn artifact_ref(path: &str, bytes: &[u8]) -> PackageArtifactRef {
    PackageArtifactRef {
        path: path.to_string(),
        digest: agalma_contracts::sha256_digest(bytes),
        bytes: bytes.len() as u64,
        media_type: "application/json".to_string(),
    }
}

/// Seal one package requesting exactly `requested` capabilities.
fn seal_package(
    root: &Path,
    plugin_id: &str,
    requested: &[&str],
) -> agalma_conductor::plugin_manifest::DiscoveredPackage {
    seal_package_version(root, plugin_id, requested, "1.2.3")
}

/// Seal one package with an explicit `package_version` so two same-id packages
/// seal distinct bytes (and therefore distinct digests).
fn seal_package_version(
    root: &Path,
    plugin_id: &str,
    requested: &[&str],
    package_version: &str,
) -> agalma_conductor::plugin_manifest::DiscoveredPackage {
    let package_dir = root.join(plugin_id);
    let write = |relative: &str, bytes: &[u8]| {
        let path = package_dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };
    write("objects/config-schema.json", PROD_CONFIG_SCHEMA);
    write("objects/default-config.json", PROD_DEFAULT_CONFIG);
    write("objects/report-schema.json", PROD_REPORT_SCHEMA);
    let child = "#!/bin/sh\ncat >/dev/null\n";
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
        "requested_capabilities": requested,
        "required_services": [],
        "optional_services": [],
        "config_schema": artifact_ref("objects/config-schema.json", PROD_CONFIG_SCHEMA),
        "default_configuration": artifact_ref("objects/default-config.json", PROD_DEFAULT_CONFIG),
        "report_schemas": [{
            "namespace": "example.cockpit.status",
            "version": 1,
            "schema": artifact_ref("objects/report-schema.json", PROD_REPORT_SCHEMA)
        }],
        "views": [{
            "view_id": "status",
            "schema_namespace": "example.cockpit.status",
            "kind": "Status",
            "title": "Cockpit status",
            "schema_version": 1,
            "fields": ["ready"]
        }],
        "actions": [],
        "state_compatibility": {"reads": ["cockpit-state/v1"], "writes": "cockpit-state/v1"}
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

fn installation_for(
    package: &agalma_conductor::plugin_manifest::DiscoveredPackage,
    approved: &[&str],
) -> PluginInstallation {
    installation_for_plugin(package, PLUGIN_ID, approved)
}

fn installation_for_plugin(
    package: &agalma_conductor::plugin_manifest::DiscoveredPackage,
    plugin_id: &str,
    approved: &[&str],
) -> PluginInstallation {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: plugin_id.to_string(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: approved.iter().map(|c| (*c).to_string()).collect(),
        installation_digest: String::new(),
    };
    installation.installation_digest = installation.recomputed_installation_digest().unwrap();
    installation
}

/// The exact pinned installation grants control only to the selected plugin.
fn assemble_host(scratch: &ScratchDir, approved: &[&str]) -> PluginHost {
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let package = seal_package(
        &root,
        PLUGIN_ID,
        &[
            CONTROL_PLANE_READ_CAPABILITY,
            FACTORY_READ_CAPABILITY,
            FACTORY_CONTROL_CAPABILITY,
        ],
    );
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).expect("establish ledger");
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_for(&package, approved));
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_000_000);
    let mut host =
        PluginHost::assemble_with_ledger(&config, &installations, ledger, clock)
            .expect("assemble host with the explicit installation");
    // These fixtures exercise the selected control surface.
    host.set_factory_control_plugin(Some(PLUGIN_ID));
    host
}

fn all_query_caps() -> [&'static str; 3] {
    [
        CONTROL_PLANE_READ_CAPABILITY,
        FACTORY_READ_CAPABILITY,
        FACTORY_CONTROL_CAPABILITY,
    ]
}

// ---------------------------------------------------------------------------
// 1. The registrar receives the supervision state dir unchanged
// ---------------------------------------------------------------------------

struct CaptureDir {
    inner: Arc<dyn HostRegistrar>,
    state_dir: Arc<Mutex<Option<PathBuf>>>,
    router: Arc<Mutex<Option<Arc<agalma_conductor::plugin_router::RouterService>>>>,
}

impl HostRegistrar for CaptureDir {
    fn register(&self, registration: &HostRegistration<'_>) -> Result<(), ContractError> {
        self.inner.register(registration)?;
        *self.state_dir.lock().unwrap() = Some(registration.state_dir.to_path_buf());
        *self.router.lock().unwrap() = Some(Arc::clone(registration.router));
        Ok(())
    }
}

fn wait_for(mut cond: impl FnMut() -> bool, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn registrar_receives_supervision_state_dir_unchanged() {
    let dir = ScratchDir::new("cockpit-runtime-registrar");
    let state = dir.child("state");
    fs::create_dir_all(&state).unwrap();
    let ledger = LedgerHandle::in_process(&state).expect("ledger");
    let observation = Arc::new(RuntimeShared::new());
    let control = Arc::new(RuntimeControl::new());

    let composition = WatchComposition::new(
        REPOSITORY_ID.to_string(),
        STATE_ID.to_string(),
        Arc::clone(&observation),
        Arc::clone(&control),
    );
    register_work_plugin_host(&composition);
    let production = composition.take_registrar().expect("production registrar");

    let captured_dir = Arc::new(Mutex::new(None));
    let captured_router = Arc::new(Mutex::new(None));
    let context = agalma_conductor::work::factory_runtime::HostRegistrationContext {
        repository_id: REPOSITORY_ID.to_string(),
        state_id: STATE_ID.to_string(),
        shared: Arc::clone(&observation),
        control: Arc::clone(&control),
        registrar: Arc::new(CaptureDir {
            inner: production,
            state_dir: Arc::clone(&captured_dir),
            router: Arc::clone(&captured_router),
        }),
    };
    let mut supervision = SupervisionHandle::spawn_host(
        state.clone(),
        PluginRootArgs::default(),
        ledger,
        Arc::clone(&observation),
        Some(context),
        Duration::from_millis(20),
    );

    wait_for(
        || captured_dir.lock().unwrap().is_some(),
        "host registration",
    );
    let observed = captured_dir.lock().unwrap().clone().expect("state dir");
    assert_eq!(
        observed, state,
        "the registrar must receive HostSupervision's configured state_dir unchanged"
    );
    assert_ne!(
        observed,
        PathBuf::from(STATE_ID),
        "the state dir is never derived from the opaque state id"
    );
    let router = captured_router.lock().unwrap().clone().expect("router");
    assert!(
        router.router().has_adapter(FACTORY_READ_CAPABILITY),
        "the factory read adapter is registered on the served router"
    );
    assert!(
        router.router().has_adapter(FACTORY_CONTROL_CAPABILITY),
        "the factory control adapter is registered on the served router"
    );

    supervision.request_shutdown();
    supervision.join().expect("supervision owner joins");
}

// ---------------------------------------------------------------------------
// 2. Private multigrant descriptor
// ---------------------------------------------------------------------------

#[test]
fn launch_request_issues_generic_multigrant_descriptor() {
    let scratch = ScratchDir::new("cockpit-runtime-multigrant");
    let mut host = assemble_host(&scratch, &all_query_caps());
    let generation = *host.installed.get(PLUGIN_ID).expect("installed generation");

    let request = host.launch_request(PLUGIN_ID).expect("launch request");
    let raw = request
        .env
        .get(PLUGIN_CAPABILITY_BINDINGS_ENV)
        .expect("generic capability descriptor is issued");
    let bindings = parse_capability_bindings(raw).expect("descriptor is well-formed");

    for capability in all_query_caps() {
        let binding = bindings
            .get(capability)
            .unwrap_or_else(|| panic!("binding for {capability}"));
        assert_eq!(binding.capability, capability);
        assert_eq!(binding.plugin_id, PLUGIN_ID);
        assert_eq!(binding.generation.as_str(), generation.to_string());
        assert_eq!(
            binding.hub_socket,
            host.hub_socket.to_string_lossy().into_owned()
        );
        assert_eq!(binding.grant.len(), agalma_ipc::BINDING_GRANT_HEX_CHARS);
    }

    // Legacy first-grant env keeps its documented behavior (control-plane.read
    // is the first approved capability).
    assert_eq!(
        request.env.get(PLUGIN_GRANT_CAPABILITY_ENV).map(String::as_str),
        Some(CONTROL_PLANE_READ_CAPABILITY)
    );
    let legacy = host.launch_grant(PLUGIN_ID).expect("legacy first grant");
    assert_eq!(legacy.scope().resource_namespace, CONTROL_PLANE_READ_CAPABILITY);
    assert_eq!(
        request.env.get(PLUGIN_GRANT_ENV),
        Some(&legacy.token_hex())
    );
    assert!(request.env.contains_key(HUB_SOCKET_ENV));

    // The explicit control approval publishes the exact pinned binding.
    let binding = control_binding_for(&host.state_dir, PLUGIN_ID).expect("control binding published");
    assert_eq!(binding.plugin_id, PLUGIN_ID);
    assert_eq!(binding.capability, FACTORY_CONTROL_CAPABILITY);
    assert_eq!(binding.generation, generation);
    assert!(binding.expires_at_unix_ms > 0);
}

// ---------------------------------------------------------------------------
// 3. Configured-root auto-install stays read-only
// ---------------------------------------------------------------------------

#[test]
fn configured_root_auto_install_is_read_only() {
    let scratch = ScratchDir::new("cockpit-runtime-autoinstall");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    // The package requests the factory capabilities, but the automatic grant is
    // the intersection with SAFE_CAPABILITY_ALLOWLIST (empty for these).
    seal_package(
        &root,
        PLUGIN_ID,
        &[
            CONTROL_PLANE_READ_CAPABILITY,
            FACTORY_READ_CAPABILITY,
            FACTORY_CONTROL_CAPABILITY,
        ],
    );
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).expect("ledger");
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_000_000);
    let mut host = PluginHost::assemble_with_ledger(&config, &InMemoryInstallations::new(), ledger, clock)
        .expect("auto-install host");
    host.set_factory_control_plugin(None);

    assert!(
        SAFE_CAPABILITY_ALLOWLIST
            .iter()
            .all(|c| *c != FACTORY_CONTROL_CAPABILITY),
        "factory.control/v1 must never be in the automatic safe allowlist"
    );

    host.launch_request(PLUGIN_ID).expect("launch request");
    // The configured-root auto-install grants only the read-safe capabilities; it
    // never grants control.
    assert!(
        host.capability_grant(PLUGIN_ID, CONTROL_PLANE_READ_CAPABILITY)
            .is_some(),
        "the auto-install keeps the safe read capability"
    );
    assert!(
        host.capability_grant(PLUGIN_ID, FACTORY_READ_CAPABILITY)
            .is_some(),
        "the auto-install keeps the safe factory read capability"
    );
    assert!(
        host.capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY).is_none(),
        "no control grant"
    );
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_none(),
        "no control binding is published for a read-only installation"
    );
}

// ---------------------------------------------------------------------------
// 4. Rotation republishes the pinned control binding
// ---------------------------------------------------------------------------

#[test]
fn control_binding_rotates_with_the_launch_epoch() {
    let scratch = ScratchDir::new("cockpit-runtime-rotate");
    let mut host = assemble_host(&scratch, &all_query_caps());
    let generation = *host.installed.get(PLUGIN_ID).unwrap();

    let first = host.launch_request(PLUGIN_ID).unwrap();
    let first_token = host
        .capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY)
        .unwrap()
        .token_hex();
    let first_binding = control_binding_for(&host.state_dir, PLUGIN_ID).unwrap();
    assert_eq!(first_binding.generation, generation);
    assert!(first.env.contains_key(PLUGIN_CAPABILITY_BINDINGS_ENV));

    let second = host.launch_request(PLUGIN_ID).unwrap();
    let second_token = host
        .capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY)
        .unwrap()
        .token_hex();
    assert_ne!(first_token, second_token, "the launch epoch rotates the token");
    assert_eq!(
        control_binding_for(&host.state_dir, PLUGIN_ID).unwrap().generation,
        generation,
        "rotation preserves the pinned generation"
    );
    assert!(second.env.contains_key(PLUGIN_CAPABILITY_BINDINGS_ENV));
}

// ---------------------------------------------------------------------------
// 5. Served router control receipt + revocation + trusted permissions
// ---------------------------------------------------------------------------

fn capture_grant(
    host: &PluginHost,
    capability: &str,
    operation_id: &str,
    input: Value,
) -> CapabilityCallRequest {
    let grant = host
        .capability_grant(PLUGIN_ID, capability)
        .expect("capability grant");
    CapabilityCallRequest {
        grant: grant.token_hex(),
        plugin_id: PLUGIN_ID.to_string(),
        generation: grant.scope().binding.generation,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: capability.to_string(),
        operation_id: operation_id.to_string(),
        input,
        deadline_unix_ms: FAR_FUTURE_MS,
        depth: 0,
    }
}

fn invoke_control(
    router: &mut CapabilityRouter,
    call: &CapabilityCallRequest,
) -> Result<Value, ContractError> {
    router.invoke_direct(call)
}

fn snapshot(
    router: &mut CapabilityRouter,
    call: &CapabilityCallRequest,
) -> Result<Value, ContractError> {
    // `factory.read/v1` is a direct (stop-exempt, journal-free) capability; only
    // `control-plane.read/v1` is the observation-only read path.
    router.invoke_direct(call)
}

#[test]
fn served_factory_control_applies_replays_and_refuses_after_revocation() {
    let scratch = ScratchDir::new("cockpit-runtime-control");
    let mut host = assemble_host(&scratch, &all_query_caps());
    let state_dir = host.state_dir.clone();
    let ledger = host.ledger().expect("ledger attached").clone();

    // Register the factory service over the real ledger, the live observation
    // cell and the composition resolver/owner (exactly the production registrar
    // path, applied to the host's served router).
    let shared = Arc::new(RuntimeShared::new());
    shared.initialize(
        Some("run-1".to_string()),
        vec![
            (SourceState::Available, String::new()),
            (SourceState::Available, String::new()),
            (SourceState::Empty, String::new()),
            (SourceState::Empty, String::new()),
            (SourceState::Available, String::new()),
        ],
    );
    shared.publish_idle(None);
    let control = Arc::new(RuntimeControl::new());
    let composition = WatchComposition::new(
        REPOSITORY_ID.to_string(),
        STATE_ID.to_string(),
        Arc::clone(&shared),
        Arc::clone(&control),
    );
    register_work_plugin_host(&composition);
    let registrar = composition.take_registrar().unwrap();
    registrar
        .register(&HostRegistration {
            repository_id: REPOSITORY_ID,
            state_id: STATE_ID,
            state_dir: &state_dir,
            ledger: &ledger,
            router: &host.router,
            shared: &shared,
            control: &control,
        })
        .expect("production registrar binds the factory service");

    let _ = host.launch_request(PLUGIN_ID).expect("launch request");

    // Trusted permission composer: with a live control binding, healthy sources
    // and a running stop, a read snapshot advertises pause (never resume).
    let read_router = host.router.router();
    {
        let mut router = read_router;
        let read_call = capture_grant(
            &host,
            FACTORY_READ_CAPABILITY,
            "op-read-1",
            json!({"format": "FactoryRuntime/v1", "operation": "snapshot"}),
        );
        let value = snapshot(&mut router, &read_call).expect("snapshot routes");
        let runtime: FactoryRuntime = serde_json::from_value(value).unwrap();
        runtime.validate().unwrap();
        assert_eq!(runtime.status, FactoryRuntimeStatus::Idle);
        assert!(runtime.permissions.pause, "pause is enabled by the composer");
        assert!(!runtime.permissions.resume, "resume is not enabled while running");
    }

    // A real control grant yields a durable applied receipt.
    let control_call = capture_grant(
        &host,
        FACTORY_CONTROL_CAPABILITY,
        "op-pause-1",
        json!({"format": "FactoryControl/v1", "operation": "pause", "action_id": "pause-1", "confirmation": null}),
    );
    let control_fingerprint = host
        .capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY)
        .unwrap()
        .fingerprint();
    {
        let mut router = host.router.router();
        let first = invoke_control(&mut router, &control_call).expect("control routes");
        let response = FactoryControlResponse::from_value(first).expect("typed control response");
        assert!(matches!(response, FactoryControlResponse::Receipt(_)));
    }
    // The durable latch is set and the intent/result pair is recorded.
    assert!(LedgerApi::kill_latch(&ledger).unwrap(), "pause sets the latch");
    let receipts = LedgerApi::control_receipts(&ledger).unwrap();
    assert_eq!(receipts.len(), 2, "one intent + one result");

    // Exact replay (same action identity/input) returns the identical receipt.
    {
        let mut router = host.router.router();
        let replay = invoke_control(&mut router, &control_call).expect("replay routes");
        let response = FactoryControlResponse::from_value(replay).expect("typed replay");
        assert!(matches!(response, FactoryControlResponse::Receipt(_)));
    }
    assert_eq!(
        LedgerApi::control_receipts(&ledger).unwrap().len(),
        2,
        "an identical replay adds no new effect"
    );

    // Conflicting reuse of the same action id fails before any effect.
    {
        let mut conflict = control_call.clone();
        conflict.input = json!({"format": "FactoryControl/v1", "operation": "pause", "action_id": "pause-1", "confirmation": "resume"});
        let mut router = host.router.router();
        let error = invoke_control(&mut router, &conflict).expect_err("conflict refused");
        assert_eq!(failure_code(&error), Some(FailureCode::IdentityConflict));
    }
    assert_eq!(LedgerApi::control_receipts(&ledger).unwrap().len(), 2);

    // Revoking the control grant makes every further control call fail closed
    // with zero new effects.
    host.router.router().revoke_grant(&control_fingerprint);
    {
        let mut router = host.router.router();
        let error = invoke_control(&mut router, &control_call).expect_err("revoked control refused");
        matches!(
            failure_code(&error),
            Some(FailureCode::AuthorityDenied | FailureCode::MissingCapability)
        );
    }
    assert_eq!(
        LedgerApi::control_receipts(&ledger).unwrap().len(),
        2,
        "a revoked control call has zero effects"
    );
}

// ---------------------------------------------------------------------------
// 6. Exclusive control owner
// ---------------------------------------------------------------------------

#[test]
fn exclusive_control_owner_refuses_a_second_owner() {
    let scratch = ScratchDir::new("cockpit-runtime-owner");
    let state = scratch.child("state");
    fs::create_dir_all(&state).unwrap();

    let first = FactoryControlOwner::acquire(&state);
    assert!(first.owned(), "the first owner holds the guard");
    let second = FactoryControlOwner::acquire(&state);
    assert!(!second.owned(), "a competing owner is refused");
    drop(first);
    let third = FactoryControlOwner::acquire(&state);
    assert!(third.owned(), "a released guard is reclaimed");
}

/// A concurrent-owner race must yield **at most one** owner, even when many
/// acquirers race the create/publish window (the original defect could unlink a
/// lock before its owner pid was written, admitting two owners).
#[test]
fn concurrent_control_owner_acquisition_admits_at_most_one_owner() {
    for round in 0..64 {
        let scratch = ScratchDir::new(&format!("cockpit-runtime-owner-race-{round}"));
        let state = scratch.child("state");
        fs::create_dir_all(&state).unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let held = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let state = state.clone();
            let barrier = Arc::clone(&barrier);
            let held = Arc::clone(&held);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let owner = FactoryControlOwner::acquire(&state);
                if owner.owned() {
                    held.lock().unwrap().push(owner);
                }
            }));
        }
        for worker in workers {
            worker.join().expect("owner thread joins");
        }
        let owners = held.lock().unwrap();
        assert_eq!(
            owners.len(),
            1,
            "round {round}: exactly one owner may hold the lock"
        );
        drop(owners);
    }
}

/// A well-formed but dead-pid lock must be reclaimed by exactly one contender,
/// and a stale reclaimer must never unlink the live replacement lock.
///
/// Starts every round from a dead-PID lock (unlike the create-window race) and
/// races the reclamation path directly.
#[test]
fn stale_lock_reclaim_admits_exactly_one_owner_and_keeps_its_lock() {
    let mut dead_pids = Vec::new();
    for _ in 0..4 {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .spawn()
            .expect("spawn helper");
        let pid = child.id();
        child.wait().expect("reap helper");
        dead_pids.push(pid);
    }

    for round in 0..64 {
        let scratch = ScratchDir::new(&format!("cockpit-runtime-stale-{round}"));
        let state = scratch.child("state");
        fs::create_dir_all(&state).unwrap();
        let lock = state.join("factory-control.lock");
        // A well-formed lock whose recorded pid is provably dead.
        fs::write(&lock, format!("{}\n", dead_pids[round % dead_pids.len()])).unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let held = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let state = state.clone();
            let barrier = Arc::clone(&barrier);
            let held = Arc::clone(&held);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let owner = FactoryControlOwner::acquire(&state);
                if owner.owned() {
                    held.lock().unwrap().push(owner);
                }
            }));
        }
        for worker in workers {
            worker.join().expect("reclaimer thread joins");
        }

        let owners = held.lock().unwrap();
        assert_eq!(owners.len(), 1, "round {round}: exactly one live owner");
        // The live owner's lock remains present and records a live pid.
        let recorded: u32 = fs::read_to_string(&lock)
            .expect("the winner's lock remains present")
            .trim()
            .parse()
            .expect("a well-formed owner pid");
        assert!(
            std::process::Command::new("/bin/kill")
                .arg("-0")
                .arg(recorded.to_string())
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false),
            "the surviving lock must belong to a live owner"
        );
        drop(owners);
    }
}

/// An empty or malformed lock is never reclaimed (fail closed, never unlinked).
#[test]
fn empty_or_malformed_control_lock_is_never_unlinked() {
    let scratch = ScratchDir::new("cockpit-runtime-malformed-lock");
    let state = scratch.child("state");
    fs::create_dir_all(&state).unwrap();
    let lock = state.join("factory-control.lock");

    for malformed in ["", "   \n", "not-a-pid\n"] {
        fs::write(&lock, malformed).unwrap();
        let owner = FactoryControlOwner::acquire(&state);
        assert!(!owner.owned(), "a malformed lock is never claimed");
        assert!(
            lock.exists(),
            "a malformed lock must never be unlinked ({malformed:?})"
        );
    }
}

/// Launching an unrelated plugin must not clear or disable the dashboard's live
/// control binding (Sol finding: the old rotation cleared the state-wide cell on
/// every launch).
#[test]
fn unrelated_launch_preserves_the_dashboard_control_binding() {
    let scratch = ScratchDir::new("cockpit-runtime-binding-scope");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let dashboard = seal_package(&root, PLUGIN_ID, &all_query_caps());
    let other = seal_package(&root, "example.other", &[FACTORY_CONTROL_CAPABILITY]);

    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).expect("ledger");
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: true,
    };
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_for(&dashboard, &all_query_caps()));
    installations.insert(installation_for_plugin(
        &other,
        "example.other",
        &[FACTORY_CONTROL_CAPABILITY],
    ));
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_000_000);
    let mut host = PluginHost::assemble_with_ledger(&config, &installations, ledger, clock)
        .expect("assemble host with both packages");
    host.set_factory_control_plugin(Some(PLUGIN_ID));

    // The dashboard launch publishes its own control binding.
    host.launch_request(PLUGIN_ID).expect("dashboard launch");
    let binding = control_binding_for(&host.state_dir, PLUGIN_ID).expect("dashboard binding published");
    assert_eq!(binding.plugin_id, PLUGIN_ID);
    let generation = binding.generation;

    // An unrelated plugin launch must not clear it, and receives no control of
    // its own (it is not the selected control surface).
    host.launch_request("example.other")
        .expect("unrelated plugin launch");
    let after = control_binding_for(&host.state_dir, PLUGIN_ID)
        .expect("unrelated launch must preserve the dashboard control binding");
    assert_eq!(after.plugin_id, PLUGIN_ID);
    assert_eq!(after.generation, generation);
    assert!(
        control_binding_for(&host.state_dir, "example.other").is_none(),
        "an unselected plugin receives no control binding"
    );

    // Relaunching the dashboard rotates only its own binding.
    host.launch_request(PLUGIN_ID).expect("dashboard relaunch");
    let rotated = control_binding_for(&host.state_dir, PLUGIN_ID).expect("binding republished");
    assert_eq!(rotated.plugin_id, PLUGIN_ID);
    assert_eq!(rotated.generation, generation);
}

// ---------------------------------------------------------------------------
// 7. Stop exception scope
// ---------------------------------------------------------------------------

#[test]
fn factory_stop_exception_is_scoped_to_the_factory_capabilities() {
    assert!(CapabilityRouter::is_factory_capability(FACTORY_READ_CAPABILITY));
    assert!(CapabilityRouter::is_factory_capability(FACTORY_CONTROL_CAPABILITY));
    assert!(CapabilityRouter::is_direct_capability(FACTORY_CONTROL_CAPABILITY));
    assert!(CapabilityRouter::is_read_only_capability(CONTROL_PLANE_READ_CAPABILITY));
    // Ordinary effects keep the stop/journal path.
    assert!(!CapabilityRouter::is_factory_capability("report.publish"));
    assert!(!CapabilityRouter::is_direct_capability("report.publish"));
    assert!(!CapabilityRouter::is_factory_capability("ledger.write"));
}

// ---------------------------------------------------------------------------
// 8. Deterministic property sequences (seed 0x57454213)
// ---------------------------------------------------------------------------

const PROP_SEED: u64 = 0x5745_4213;
const PROP_CASES: usize = 256;
const PROP_MAX_EVENTS: usize = 32;

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

#[derive(Clone)]
struct ToggleAuthority {
    enabled: Arc<AtomicBool>,
    generation: u64,
    expiry: u64,
}

impl ControlBindingResolver for ToggleAuthority {
    fn resolve(&self, plugin_id: &str) -> Option<ApprovedControlBinding> {
        if !self.enabled.load(Ordering::SeqCst) {
            return None;
        }
        Some(ApprovedControlBinding {
            plugin_id: plugin_id.to_string(),
            capability: FACTORY_CONTROL_CAPABILITY.to_string(),
            generation: self.generation,
            expires_at_unix_ms: self.expiry,
        })
    }
}

#[derive(Clone)]
struct ToggleOwner(Arc<AtomicBool>);

impl ControlOwnership for ToggleOwner {
    fn owned(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A scripted stop control: 0 = running, 1 = stopped, 2 = unavailable.
#[derive(Clone)]
struct ScriptedStop(Arc<AtomicU8>);

impl StopControl for ScriptedStop {
    fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst) != 0
    }

    fn decision(&self) -> StopDecision {
        match self.0.load(Ordering::SeqCst) {
            0 => StopDecision::Running,
            1 => StopDecision::Stopped("operator pause".to_string()),
            _ => StopDecision::Unavailable("stop authority unavailable".to_string()),
        }
    }
}

fn grant_for(router_grant: &LaunchGrant, capability: &str, operation_id: &str, input: Value) -> CapabilityCallRequest {
    CapabilityCallRequest {
        grant: router_grant.token_hex(),
        plugin_id: PLUGIN_ID.to_string(),
        generation: router_grant.scope().binding.generation,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: capability.to_string(),
        operation_id: operation_id.to_string(),
        input,
        deadline_unix_ms: FAR_FUTURE_MS,
        depth: 0,
    }
}

fn fixed_grant(capability: &str) -> LaunchGrant {
    let binding = agalma_contracts::BindingPin {
        binding_id: PLUGIN_ID.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "1.2.3".to_string(),
        generation: 1,
        config_digest: format!("sha256:{}", "a".repeat(64)),
    };
    LaunchGrant::for_capability(
        PLUGIN_ID,
        binding,
        METHOD_INVOKE_CAPABILITY,
        capability,
        FAR_FUTURE_MS,
        None,
    )
    .expect("OS randomness")
}

#[test]
fn property_control_and_observation_sequences_seed_57454213() {
    let scratch = ScratchDir::new("cockpit-runtime-prop");
    let state = scratch.child("state");
    fs::create_dir_all(&state).unwrap();
    let ledger = LedgerHandle::in_process(&state).expect("ledger");
    let read_grant = fixed_grant(FACTORY_READ_CAPABILITY);
    let control_grant = fixed_grant(FACTORY_CONTROL_CAPABILITY);

    let mut rng = XorShift::new(PROP_SEED);
    for case in 0..PROP_CASES {
        let events = 1 + rng.below(PROP_MAX_EVENTS as u64) as usize;
        let shared = Arc::new(RuntimeShared::new());
        shared.initialize(
            Some(format!("run-{case}")),
            vec![
                (SourceState::Available, String::new()),
                (SourceState::Available, String::new()),
                (SourceState::Empty, String::new()),
                (SourceState::Empty, String::new()),
                (SourceState::Available, String::new()),
            ],
        );
        shared.publish_idle(None);

        let authority = ToggleAuthority {
            enabled: Arc::new(AtomicBool::new(true)),
            generation: read_grant.scope().binding.generation,
            expiry: FAR_FUTURE_MS,
        };
        let owner = ToggleOwner(Arc::new(AtomicBool::new(true)));
        let stop = ScriptedStop(Arc::new(AtomicU8::new(0)));

        let service = FactoryControlService::new(
            REPOSITORY_ID,
            STATE_ID,
            Box::new(ledger.clone()),
            &state,
        )
        .with_runtime(Arc::new(FactoryRuntimeSource::new(
            Arc::clone(&shared),
            REPOSITORY_ID.to_string(),
            STATE_ID.to_string(),
        )))
        .with_stop(Arc::new(stop.clone()))
        .with_authority(Arc::new(authority.clone()))
        .with_ownership(Arc::new(owner.clone()));

        let mut router = CapabilityRouter::new(
            Box::new(InMemoryOperationJournal::new()),
            Arc::new(RunningStop),
        );
        service.register(&mut router);
        router.install_grant(&read_grant);
        router.install_grant(&control_grant);

        let mut applied_action: Option<String> = None;

        for event in 0..events {
            let op = rng.below(6);
            match op {
                // Observation read: never an effect, never a fabricated status.
                0 => {
                    let before = LedgerApi::control_receipts(&ledger).unwrap().len();
                    let call = grant_for(
                        &read_grant,
                        FACTORY_READ_CAPABILITY,
                        &format!("read-{case}-{event}"),
                        json!({"format": "FactoryRuntime/v1", "operation": "snapshot"}),
                    );
                    let value = router.invoke_direct(&call).expect("read routes");
                    let runtime: FactoryRuntime = serde_json::from_value(value).unwrap();
                    runtime.validate().unwrap();
                    if op == 0 && rng.below(4) == 0 {
                        shared.publish_unavailable("ledger unreadable");
                    }
                    if runtime.status == FactoryRuntimeStatus::Unavailable {
                        assert_ne!(runtime.status, FactoryRuntimeStatus::Idle);
                    }
                    assert_eq!(
                        LedgerApi::control_receipts(&ledger).unwrap().len(),
                        before,
                        "a read creates no effect"
                    );
                }
                // Pause / resume with a fresh action identity.
                1 | 2 => {
                    let action = if op == 1 { "pause" } else { "resume" };
                    let id = format!("a-{case}-{event}");
                    let confirmation = if action == "resume" { Some("resume") } else { None };
                    let call = grant_for(
                        &control_grant,
                        FACTORY_CONTROL_CAPABILITY,
                        &format!("ctl-{case}-{event}"),
                        json!({
                            "format": "FactoryControl/v1",
                            "operation": action,
                            "action_id": id,
                            "confirmation": confirmation,
                        }),
                    );
                    let before = LedgerApi::control_receipts(&ledger).unwrap().len();
                    match router.invoke_direct(&call) {
                        Ok(value) => {
                            let response = FactoryControlResponse::from_value(value).unwrap();
                            match response {
                                FactoryControlResponse::Receipt(_) => {
                                    assert_eq!(
                                        LedgerApi::control_receipts(&ledger).unwrap().len(),
                                        before + 2,
                                        "a new applied/denied action records intent+result"
                                    );
                                    applied_action = Some(id);
                                }
                                FactoryControlResponse::Unconfirmed(_) => {
                                    assert_eq!(
                                        LedgerApi::control_receipts(&ledger).unwrap().len(),
                                        before + 1,
                                        "unconfirmed leaves only the intent"
                                    );
                                }
                                FactoryControlResponse::NotFound(_) => {
                                    panic!("an action is never not_found")
                                }
                            }
                        }
                        Err(error) => {
                            // Authority/stop refusals happen before any effect.
                            let code = failure_code(&error);
                            assert!(
                                matches!(
                                    code,
                                    Some(FailureCode::AuthorityDenied | FailureCode::StateUnavailable)
                                ),
                                "unexpected control error {code:?}"
                            );
                            assert_eq!(
                                LedgerApi::control_receipts(&ledger).unwrap().len(),
                                before,
                                "a refused control creates no effect"
                            );
                        }
                    }
                }
                // Authority/ownership loss.
                3 => {
                    let on = rng.below(2) == 0;
                    authority.enabled.store(on, Ordering::SeqCst);
                    owner.0.store(rng.below(4) != 0, Ordering::SeqCst);
                }
                // Stop state changes.
                4 => {
                    stop.0.store(rng.below(3) as u8, Ordering::SeqCst);
                    shared.publish_stop(stop.decision());
                }
                // Conflicting reuse of a previously completed action id.
                _ => {
                    if let Some(id) = applied_action.clone() {
                        let call = grant_for(
                            &control_grant,
                            FACTORY_CONTROL_CAPABILITY,
                            &format!("conflict-{case}-{event}"),
                            json!({
                                "format": "FactoryControl/v1",
                                "operation": "pause",
                                "action_id": id,
                                "confirmation": "resume",
                            }),
                        );
                        let before = LedgerApi::control_receipts(&ledger).unwrap().len();
                        if let Err(error) = router.invoke_direct(&call) {
                            assert!(
                                matches!(
                                    failure_code(&error),
                                    Some(
                                        FailureCode::IdentityConflict
                                            | FailureCode::AuthorityDenied
                                            | FailureCode::StateUnavailable
                                    )
                                ),
                                "conflict error {error:?}"
                            );
                        }
                        assert_eq!(
                            LedgerApi::control_receipts(&ledger).unwrap().len(),
                            before,
                            "a conflicting reuse creates no new effect"
                        );
                    }
                }
            }
        }
    }
}

/// A restart over the same durable ledger replays the frozen receipt exactly.
#[test]
fn frozen_recovery_replays_the_durable_receipt_after_restart() {
    let scratch = ScratchDir::new("cockpit-runtime-frozen");
    let state = scratch.child("state");
    fs::create_dir_all(&state).unwrap();
    let ledger = LedgerHandle::in_process(&state).expect("ledger");
    let control_grant = fixed_grant(FACTORY_CONTROL_CAPABILITY);
    let authority = ToggleAuthority {
        enabled: Arc::new(AtomicBool::new(true)),
        generation: control_grant.scope().binding.generation,
        expiry: FAR_FUTURE_MS,
    };
    let owner = ToggleOwner(Arc::new(AtomicBool::new(true)));
    let stop = ScriptedStop(Arc::new(AtomicU8::new(0)));

    let service = || {
        let shared = Arc::new(RuntimeShared::new());
        shared.initialize(
            Some("run-frozen".to_string()),
            vec![
                (SourceState::Available, String::new()),
                (SourceState::Available, String::new()),
                (SourceState::Empty, String::new()),
                (SourceState::Empty, String::new()),
                (SourceState::Available, String::new()),
            ],
        );
        shared.publish_idle(None);
        let mut router = CapabilityRouter::new(
            Box::new(InMemoryOperationJournal::new()),
            Arc::new(RunningStop),
        );
        FactoryControlService::new(REPOSITORY_ID, STATE_ID, Box::new(ledger.clone()), &state)
            .with_runtime(Arc::new(FactoryRuntimeSource::new(
                shared,
                REPOSITORY_ID.to_string(),
                STATE_ID.to_string(),
            )))
            .with_stop(Arc::new(stop.clone()))
            .with_authority(Arc::new(authority.clone()))
            .with_ownership(Arc::new(owner.clone()))
            .register(&mut router);
        router.install_grant(&control_grant);
        router
    };

    let call = grant_for(
        &control_grant,
        FACTORY_CONTROL_CAPABILITY,
        "frozen-op",
        json!({"format": "FactoryControl/v1", "operation": "pause", "action_id": "frozen-1", "confirmation": null}),
    );

    let mut router = service();
    let first = router.invoke_direct(&call).expect("first applied");
    let first_receipt = FactoryControlResponse::from_value(first)
        .expect("receipt")
        .clone();
    let (first_recorded, first_outcome) = match &first_receipt {
        FactoryControlResponse::Receipt(r) => (
            r.receipt.recorded_at_unix_ms.clone(),
            r.receipt.outcome,
        ),
        other => panic!("expected receipt, got {other:?}"),
    };
    assert_eq!(first_outcome, agalma_contracts::FactoryControlOutcome::Applied);

    // Restart: a fresh service over the same durable ledger and a fresh router.
    let mut restarted = service();
    let replay = restarted.invoke_direct(&call).expect("replay routes");
    match FactoryControlResponse::from_value(replay).unwrap() {
        FactoryControlResponse::Receipt(r) => {
            assert_eq!(
                r.receipt.recorded_at_unix_ms, first_recorded,
                "the frozen historical timestamp is replayed exactly"
            );
        }
        other => panic!("expected replayed receipt, got {other:?}"),
    }
    assert_eq!(
        LedgerApi::control_receipts(&ledger).unwrap().len(),
        2,
        "a restart adds no duplicate effect"
    );
}

// ---------------------------------------------------------------------------
// 9. Durable exact-package operator pin (Sol finding 1)
// ---------------------------------------------------------------------------

/// Persist an operator installation pin into the **existing** durable hub
/// installation store (`HubInstallStore`), binding the exact frozen package
/// digest, configuration artifact, launch profile and capability set.
fn install_operator_pin(
    state_dir: &Path,
    package: &agalma_conductor::plugin_manifest::DiscoveredPackage,
    caps: &[&str],
) {
    install_operator_pin_at(state_dir, package, caps, 1);
}

/// Persist an operator installation pin at an explicit generation.
fn install_operator_pin_at(
    state_dir: &Path,
    package: &agalma_conductor::plugin_manifest::DiscoveredPackage,
    caps: &[&str],
    generation: u64,
) {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: package.plugin_id.clone(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
        installation_digest: String::new(),
    };
    installation.installation_digest = installation.recomputed_installation_digest().unwrap();
    let store = agalma_conductor::plugin_registry::HubInstallStore::new(state_dir);
    store
        .install(package, generation, &installation)
        .expect("persist the operator installation pin");
}

fn work_host(
    state_dir: &Path,
    root: &Path,
    ledger: &LedgerHandle,
    selector: Option<&str>,
) -> Option<PluginHost> {
    let roots = PluginRootArgs {
        plugin_root: vec![root.to_path_buf()],
        plugin_root_optional: Vec::new(),
    };
    agalma_conductor::composition::assemble_work_plugin_host_with_control(
        state_dir, &roots, ledger, selector,
    )
}

#[test]
fn durable_operator_pin_binds_control_to_the_exact_package() {
    let scratch = ScratchDir::new("cockpit-runtime-durable-pin");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let package = seal_package(&root, PLUGIN_ID, &all_query_caps());
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    // The exact frozen pin (digest + configuration + profile + capabilities)
    // grants the dashboard's factory read/control capabilities and requests it
    // Running.
    install_operator_pin(&state_dir, &package, &all_query_caps());
    let mut host = work_host(&state_dir, &root, &ledger, Some(PLUGIN_ID)).expect("pinned package assembles");
    assert_eq!(
        host.registry.desired(PLUGIN_ID),
        agalma_conductor::plugin_registry::DesiredState::Running,
        "an approved pin requests the package Running"
    );
    host.launch_request(PLUGIN_ID).expect("launch request");
    let binding = control_binding_for(&host.state_dir, PLUGIN_ID)
        .expect("the exact pinned approval grants control authority");
    assert_eq!(binding.plugin_id, PLUGIN_ID);
    assert_eq!(binding.capability, FACTORY_CONTROL_CAPABILITY);
}

#[test]
fn changed_package_digest_cannot_obtain_control_authority() {
    let scratch = ScratchDir::new("cockpit-runtime-changed-digest");
    let pinned_root = scratch.child("plugins-pinned");
    let changed_root = scratch.child("plugins-changed");
    fs::create_dir_all(&pinned_root).unwrap();
    fs::create_dir_all(&changed_root).unwrap();
    // The approved package and the discovered package share a plugin id but the
    // latter's bytes changed (a version bump changes the sealed digest).
    let approved = seal_package(&pinned_root, PLUGIN_ID, &all_query_caps());
    let changed = seal_package_version(&changed_root, PLUGIN_ID, &all_query_caps(), "2.0.0");
    assert_ne!(approved.package_digest, changed.package_digest);

    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    install_operator_pin(&state_dir, &approved, &all_query_caps());

    // Discovery over the changed package only: the stored pin names the old
    // digest, so no approval is granted for the changed bytes.
    let roots = PluginRootArgs {
        plugin_root: vec![changed_root.clone()],
        plugin_root_optional: Vec::new(),
    };
    let mut host = agalma_conductor::composition::assemble_work_plugin_host_with_control(
        &state_dir,
        &roots,
        &ledger,
        Some(PLUGIN_ID),
    )
    .expect("the changed package assembles read-only, never with control");
    host.launch_request(PLUGIN_ID).expect("launch request");
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_none(),
        "a changed digest must never inherit the pinned control authority"
    );
    assert!(
        host.capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY).is_none(),
        "no control grant for the changed package"
    );
}

#[test]
fn unrelated_same_id_package_cannot_obtain_control_authority() {
    let scratch = ScratchDir::new("cockpit-runtime-same-id");
    let pinned_root = scratch.child("plugins-a");
    let other_root = scratch.child("plugins-b");
    fs::create_dir_all(&pinned_root).unwrap();
    fs::create_dir_all(&other_root).unwrap();
    // Two packages with the same plugin id but different bytes.
    let pinned = seal_package(&pinned_root, PLUGIN_ID, &all_query_caps());
    let unrelated = seal_package_version(&other_root, PLUGIN_ID, &all_query_caps(), "2.0.0");
    assert_ne!(pinned.package_digest, unrelated.package_digest);

    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    // The durable pin approves the pinned package's exact digest; the discovered
    // unrelated package (same id, different digest) must not borrow it.
    install_operator_pin(&state_dir, &pinned, &all_query_caps());
    let roots = PluginRootArgs {
        plugin_root: vec![other_root.clone()],
        plugin_root_optional: Vec::new(),
    };
    let mut host = agalma_conductor::composition::assemble_work_plugin_host_with_control(
        &state_dir,
        &roots,
        &ledger,
        Some(PLUGIN_ID),
    )
    .expect("the unrelated same-id package assembles read-only");
    host.launch_request(PLUGIN_ID).expect("launch request");
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_none(),
        "an unrelated same-id package must never obtain control authority"
    );
}

/// A newer same-plugin generation whose pin drops `factory.control/v1` revokes
/// the older grant: authority resolves from the **current** pin only.
#[test]
fn newer_generation_revokes_older_factory_control_grant() {
    let scratch = ScratchDir::new("cockpit-runtime-revocation");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let package = seal_package(&root, PLUGIN_ID, &all_query_caps());
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    // Generation 1 approval grants the factory control capability; generation 2
    // (newer) keeps only the read capabilities.
    install_operator_pin_at(&state_dir, &package, &all_query_caps(), 1);
    install_operator_pin_at(
        &state_dir,
        &package,
        &[CONTROL_PLANE_READ_CAPABILITY, FACTORY_READ_CAPABILITY],
        2,
    );

    let mut host = work_host(&state_dir, &root, &ledger, Some(PLUGIN_ID)).expect("pinned package assembles");
    host.launch_request(PLUGIN_ID).expect("launch request");
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_none(),
        "the newer generation revokes the older factory.control grant"
    );
    assert!(
        host.capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY)
            .is_none(),
        "no factory.control grant is issued from the newer pin"
    );

    // Sanity: with only the granting generation present, control is available.
    let scratch = ScratchDir::new("cockpit-runtime-revocation-single");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let package = seal_package(&root, PLUGIN_ID, &all_query_caps());
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    install_operator_pin_at(&state_dir, &package, &all_query_caps(), 1);
    let mut host = work_host(&state_dir, &root, &ledger, Some(PLUGIN_ID)).expect("pinned package assembles");
    host.launch_request(PLUGIN_ID).expect("launch request");
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_some(),
        "the granting generation alone still yields control authority"
    );
}

/// A different plugin id whose durable pin is only the automatic safe-allowlist
/// auto-install shape (a "valid matching durable pin" that every configured-root
/// package receives) never obtains control authority.
#[test]
fn unrelated_plugin_safe_pin_never_gets_control() {
    let scratch = ScratchDir::new("cockpit-runtime-unrelated-safe-pin");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let other = seal_package(&root, "example.other", &all_query_caps());
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    // The pin is durable and digest-matching, but approves only the automatic
    // safe-allowlist capabilities (the auto-install shape) — never an explicit
    // control grant.
    install_operator_pin(&state_dir, &other, &["report.publish", "ledger.read"]);
    let roots = PluginRootArgs {
        plugin_root: vec![root.clone()],
        plugin_root_optional: Vec::new(),
    };
    let mut host = agalma_conductor::composition::assemble_work_plugin_host_with_control(
        &state_dir,
        &roots,
        &ledger,
        Some("example.other"),
    )
    .expect("the safe-pinned unrelated package assembles read-only");
    host.launch_request("example.other").expect("launch request");
    assert!(
        control_binding_for(&host.state_dir, "example.other").is_none(),
        "a safe-allowlist pin is never an explicit control approval"
    );
    assert!(
        host.capability_grant("example.other", FACTORY_CONTROL_CAPABILITY)
            .is_none(),
        "no control grant for an auto-install-shaped pin"
    );
}

/// With no `--factory-control-plugin` selector the cockpit is read-only, even
/// when the selected plugin's durable pin explicitly grants `factory.control/v1`.
#[test]
fn absent_selector_is_read_only_even_with_control_pin() {
    let scratch = ScratchDir::new("cockpit-runtime-no-selector");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let package = seal_package(&root, PLUGIN_ID, &all_query_caps());
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    install_operator_pin(&state_dir, &package, &all_query_caps());

    let host = work_host(&state_dir, &root, &ledger, None).expect("read-only assembly");
    let mut host = host;
    host.launch_request(PLUGIN_ID).expect("launch request");
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_none(),
        "no selector means no control authority"
    );
    assert!(
        host.capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY)
            .is_none(),
        "no control grant without a selector"
    );
}

/// Only the selected plugin id may receive control; an unselected plugin with an
/// equally explicit `factory.control/v1` pin receives none.
#[test]
fn unselected_plugin_control_pin_receives_no_control() {
    let scratch = ScratchDir::new("cockpit-runtime-unselected");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let selected = seal_package(&root, PLUGIN_ID, &all_query_caps());
    let unselected = seal_package(&root, "example.other", &all_query_caps());
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    // Both packages carry an explicit control pin; only the selected one counts.
    install_operator_pin(&state_dir, &selected, &all_query_caps());
    install_operator_pin(&state_dir, &unselected, &all_query_caps());

    let mut host = work_host(&state_dir, &root, &ledger, Some(PLUGIN_ID))
        .expect("selected package assembles");
    host.launch_request(PLUGIN_ID).expect("selected launch");
    host.launch_request("example.other")
        .expect("unselected launch");
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_some(),
        "the selected plugin receives control authority"
    );
    assert!(
        control_binding_for(&host.state_dir, "example.other").is_none(),
        "an unselected plugin never receives control authority"
    );
    assert!(
        host.capability_grant("example.other", FACTORY_CONTROL_CAPABILITY)
            .is_none(),
        "no control grant for an unselected plugin"
    );
}

/// An unknown process-status probe must never reclaim the lock (fail closed);
/// only a confirmed-dead pid is reclaimable, and live/dead behavior is preserved.
#[test]
fn unknown_process_status_preserves_the_lock() {
    use agalma_conductor::composition::{classify_process_probe, PidStatus};

    // Classification of the probe outcome.
    assert_eq!(classify_process_probe(true, ""), PidStatus::Alive);
    assert_eq!(
        classify_process_probe(false, "/bin/kill: 123: No such process"),
        PidStatus::Dead
    );
    assert_eq!(
        classify_process_probe(false, "/bin/kill: 1: Operation not permitted"),
        PidStatus::Unknown
    );
    assert_eq!(classify_process_probe(false, ""), PidStatus::Unknown);
    assert_eq!(
        classify_process_probe(false, "unexpected output"),
        PidStatus::Unknown
    );

    let scratch = ScratchDir::new("cockpit-runtime-unknown-status");
    let state = scratch.child("state");
    fs::create_dir_all(&state).unwrap();
    let lock = state.join("factory-control.lock");

    // pid 1 exists but is not signalable by this user: the probe reports
    // permission-denied, an *unknown* status that must be preserved.
    fs::write(&lock, "1\n").unwrap();
    let owner = FactoryControlOwner::acquire(&state);
    assert!(!owner.owned(), "an unknown-status lock is never reclaimed");
    assert!(lock.exists(), "the unknown-status lock is preserved");

    // A live pid is likewise preserved.
    fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
    let owner = FactoryControlOwner::acquire(&state);
    assert!(!owner.owned(), "a live lock is preserved");
    assert!(lock.exists());

    // A confirmed-dead pid is reclaimed (existing behavior preserved).
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("true")
        .spawn()
        .unwrap();
    let dead = child.id();
    child.wait().unwrap();
    fs::write(&lock, format!("{dead}\n")).unwrap();
    let owner = FactoryControlOwner::acquire(&state);
    assert!(owner.owned(), "a confirmed-dead lock is reclaimed");
}

/// A selected plugin with no durable pin fails closed for control while keeping
/// its safe read capabilities.
#[test]
fn selected_plugin_without_pin_keeps_reads_but_no_control() {
    let scratch = ScratchDir::new("cockpit-runtime-selected-no-pin");
    let root = scratch.child("plugins");
    fs::create_dir_all(&root).unwrap();
    let package = seal_package(&root, PLUGIN_ID, &all_query_caps());
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).unwrap();
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();
    // No operator pin is installed for the selected plugin.

    let mut host = work_host(&state_dir, &root, &ledger, Some(PLUGIN_ID))
        .expect("selected package assembles read-only");
    host.launch_request(PLUGIN_ID).expect("launch request");
    assert!(
        host.capability_grant(PLUGIN_ID, CONTROL_PLANE_READ_CAPABILITY)
            .is_some(),
        "the safe read capability is preserved"
    );
    assert!(
        host.capability_grant(PLUGIN_ID, FACTORY_CONTROL_CAPABILITY)
            .is_none(),
        "control fails closed without a durable pin"
    );
    assert!(
        control_binding_for(&host.state_dir, PLUGIN_ID).is_none(),
        "no control binding without a durable pin"
    );
    let _ = package;
}
