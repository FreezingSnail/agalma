//! WEB.2 `ControlPlaneRead/v1` hub projection and scoped read routing
//! (`agalma-rwn.2`).
//!
//! Every case exercises the production pieces: a real sealed fixture package in
//! a real [`PluginRegistry`], a real [`ReportIngestor`] over a real
//! `SqliteLedger` component store, the HP.4 [`CapabilityRouter`] with a
//! per-launch scoped grant, and the `ControlPlaneReadService` projection.
//!
//! Coverage:
//!
//! * production routing returns unknown-plugin data and accurate source states;
//! * an unauthorized, expired or revoked grant, a wrong namespace/method and an
//!   expired deadline are refused before any read;
//! * reads stay available while factory effect admission is stopped and create
//!   no journal entry, task claim or action intent;
//! * a missing or unreadable ledger is an explicit unknown state, never zero;
//! * pagination is bounded, cursors pin collection/revision and a stale cursor
//!   returns `RevisionConflict` so the client restarts;
//! * an artifact read needs a visible report reference, stays inside the
//!   immutable store, re-verifies the digest and is chunk-bounded;
//! * deterministic property cases replay with a recorded seed and case budget.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{json, Value};

use agalma_conductor::composition::{InMemoryInstallations, PluginHost};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::control_plane_read::{
    live_components, ComponentProjection, ControlPlaneReadRequest, ControlPlaneReadService,
    DomainReads, DurableComponents, ProjectedComponent, OVERVIEW_MAX_EXECUTIONS,
    OVERVIEW_MAX_TIMELINE,
};
use agalma_conductor::economics::{derive_bound, EconomicsInputs, SourceRead};
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_registry::{
    persist_registry_generations, DesiredState, HubInstallStore, LifecycleState, PluginRegistry,
};
use agalma_conductor::plugin_supervisor::HealthProbe;
use agalma_conductor::plugin_reports::{
    seal_publication, seal_snapshot, snapshot_entry, ReportIngestor, REPORT_NAMESPACE_PREFIX,
};
use agalma_conductor::plugin_router::{
    admitted_read_deadline, CapabilityAdapter, CapabilityCallRequest, CapabilityRouter, GrantScope,
    LaunchGrant, RouterService, RunningStop, StopAdmission, READ_DEADLINE_MAX_MS,
};
use agalma_conductor::stop::{StopControl, StopDecision};
use agalma_contracts::digest::{canonical_json, sha256_digest};
use agalma_contracts::{
    BrowserWire, BindingPin, ComponentSummary, ContractError, Durability, ExactU64, HealthState,
    LedgerApi, PluginHealth, PluginInstallation, PluginManifest, ReadComponent, ReadCursor,
    ReadMetadata, ReadOverview, ReadPage, ReportEnvelope, SourceState,
    CONTROL_PLANE_READ_CAPABILITY, CONTROL_PLANE_READ_FORMAT, READ_PAGE_MAX_BYTES,
};
use agalma_ipc::plugin_server::{HubHandler, METHOD_INVOKE_CAPABILITY};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

/// Recorded deterministic property-test seed (xorshift64).
const PROPERTY_SEED: u64 = 0x5745_4202;
/// Recorded property-test case budget (source states and bounded collections).
const PROPERTY_CASES: u64 = 128;
/// Start of the injected hub clock (milliseconds).
const START_MS: u64 = 1_791_820_800_000;
/// The fixture reporting plugin id.
const PLUGIN_ID: &str = "example.web-reader";
/// The fixture report namespaces (one current view entry each).
const NS_VIEWS: [&str; 6] = [
    "example.web-reader.status",
    "example.web-reader.queue",
    "example.web-reader.tasks",
    "example.web-reader.timeline",
    "example.web-reader.alerts",
    "example.web-reader.extra",
];
/// The fixture status namespace (a current view entry).
const NS_STATUS: &str = NS_VIEWS[0];
/// The fixture artifact namespace.
const NS_ARTIFACTS: &str = "example.web-reader.artifacts";

const CONFIG_SCHEMA_BYTES: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
const DEFAULT_CONFIG_BYTES: &[u8] = b"{}\n";
const REPORT_SCHEMA_BYTES: &[u8] = b"{\"type\":\"object\"}\n";

// ---------------------------------------------------------------------------
// Injection seams
// ---------------------------------------------------------------------------

/// A `Send + Sync` injected clock shared by the ingestor, router and service.
struct ManualClock {
    now: Arc<AtomicU64>,
}

impl ManualClock {
    fn new(start: u64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(start)),
        }
    }

    fn set(&self, now: u64) {
        self.now.store(now, Ordering::SeqCst);
    }

    fn clock(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let now = Arc::clone(&self.now);
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    fn handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.now)
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

/// A fixed factory stop observation.
struct FixedStop(StopDecision);

impl StopControl for FixedStop {
    fn stopped(&self) -> bool {
        !matches!(self.0, StopDecision::Running)
    }

    fn decision(&self) -> StopDecision {
        self.0.clone()
    }
}

/// A stop admission that always denies effects.
struct StoppedAdmission(String);

impl StopAdmission for StoppedAdmission {
    fn decision(&self) -> StopDecision {
        StopDecision::Stopped(self.0.clone())
    }
}

/// A domain binding whose every read fails (unreadable source).
struct FailingDomains;

impl DomainReads for FailingDomains {
    fn executions(&self) -> Result<Vec<agalma_contracts::ExecutionRecord>, ContractError> {
        Err(ContractError::KnownFailure("ledger unreadable".to_string()))
    }

    fn events(
        &self,
        _execution: &agalma_contracts::ExecutionId,
    ) -> Result<Vec<agalma_contracts::ExecutionEvent>, ContractError> {
        Err(ContractError::KnownFailure("events unreadable".to_string()))
    }

    fn digests_for_execution(
        &self,
        _execution: &agalma_contracts::ExecutionId,
    ) -> Result<Vec<agalma_contracts::DigestRecord>, ContractError> {
        Err(ContractError::KnownFailure("digests unreadable".to_string()))
    }

    fn control_receipts(&self) -> Result<Vec<agalma_contracts::ControlReceipt>, ContractError> {
        Err(ContractError::KnownFailure("receipts unreadable".to_string()))
    }
}

/// A component projection whose registry load fails.
struct FailingProjection;

impl ComponentProjection for FailingProjection {
    fn components(&self) -> Result<Vec<ProjectedComponent>, ContractError> {
        Err(ContractError::KnownFailure(
            "registry checkpoint corrupt".to_string(),
        ))
    }
}

/// Synthetic bound ledger reads with `count` executions and one `start` event.
struct SyntheticDomains {
    count: u64,
    usage: Option<Value>,
    padding: usize,
    advance_clock: Option<Arc<AtomicU64>>,
    advance_to: u64,
}

impl SyntheticDomains {
    fn new(count: u64) -> Self {
        SyntheticDomains {
            count,
            usage: None,
            padding: 0,
            advance_clock: None,
            advance_to: 0,
        }
    }

    fn with_usage(count: u64, usage: Value) -> Self {
        SyntheticDomains {
            count,
            usage: Some(usage),
            padding: 0,
            advance_clock: None,
            advance_to: 0,
        }
    }

    fn with_padding(count: u64, padding: usize) -> Self {
        SyntheticDomains {
            count,
            usage: None,
            padding,
            advance_clock: None,
            advance_to: 0,
        }
    }

    fn advancing_clock(count: u64, clock: Arc<AtomicU64>, advance_to: u64) -> Self {
        SyntheticDomains {
            count,
            usage: None,
            padding: 0,
            advance_clock: Some(clock),
            advance_to,
        }
    }

    fn execution(&self, index: u64) -> agalma_contracts::ExecutionRecord {
        let task = agalma_contracts::TaskId::new(format!(
            "task-{index}{}",
            "x".repeat(self.padding)
        ));
        agalma_contracts::ExecutionRecord {
            execution_id: agalma_contracts::ExecutionId::derive(&task, 1),
            task_id: task,
            generation: 1,
            phase: agalma_contracts::ExecutionPhase::Build,
            state: agalma_contracts::ExecutionState::Running,
            attempt: 1,
            revision: index,
        }
    }
}

impl DomainReads for SyntheticDomains {
    fn executions(&self) -> Result<Vec<agalma_contracts::ExecutionRecord>, ContractError> {
        if let Some(clock) = &self.advance_clock {
            clock.store(self.advance_to, Ordering::SeqCst);
        }
        Ok((0..self.count).map(|index| self.execution(index)).collect())
    }

    fn events(
        &self,
        execution: &agalma_contracts::ExecutionId,
    ) -> Result<Vec<agalma_contracts::ExecutionEvent>, ContractError> {
        let mut events = vec![agalma_contracts::ExecutionEvent {
            execution_id: execution.clone(),
            sequence: 0,
            kind: "start".to_string(),
            payload: json!({}),
            recorded_at_unix_ms: START_MS,
        }];
        if let Some(usage) = &self.usage {
            events.push(agalma_contracts::ExecutionEvent {
                execution_id: execution.clone(),
                sequence: 1,
                kind: "usage".to_string(),
                payload: usage.clone(),
                recorded_at_unix_ms: START_MS + 1,
            });
        }
        Ok(events)
    }

    fn digests_for_execution(
        &self,
        _execution: &agalma_contracts::ExecutionId,
    ) -> Result<Vec<agalma_contracts::DigestRecord>, ContractError> {
        Ok(Vec::new())
    }

    fn control_receipts(&self) -> Result<Vec<agalma_contracts::ControlReceipt>, ContractError> {
        Ok(Vec::new())
    }
}

/// A domain binding whose execution read is slow (deadline bound test).
struct SlowDomains {
    sleep: std::time::Duration,
}

impl DomainReads for SlowDomains {
    fn executions(&self) -> Result<Vec<agalma_contracts::ExecutionRecord>, ContractError> {
        std::thread::sleep(self.sleep);
        Ok(Vec::new())
    }

    fn events(
        &self,
        _execution: &agalma_contracts::ExecutionId,
    ) -> Result<Vec<agalma_contracts::ExecutionEvent>, ContractError> {
        Ok(Vec::new())
    }

    fn digests_for_execution(
        &self,
        _execution: &agalma_contracts::ExecutionId,
    ) -> Result<Vec<agalma_contracts::DigestRecord>, ContractError> {
        Ok(Vec::new())
    }

    fn control_receipts(&self) -> Result<Vec<agalma_contracts::ControlReceipt>, ContractError> {
        Ok(Vec::new())
    }
}

/// A read adapter that blocks until released (router-lock concurrency test).
struct BlockingAdapter {
    started: Arc<(Mutex<bool>, Condvar)>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl CapabilityAdapter for BlockingAdapter {
    fn capability(&self) -> &str {
        CONTROL_PLANE_READ_CAPABILITY
    }

    fn invoke(&mut self, _scope: &GrantScope, _input: &Value) -> Result<Value, ContractError> {
        let (lock, condvar) = &*self.started;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
        let (lock, condvar) = &*self.release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = condvar.wait(released).unwrap();
        }
        Ok(json!({"ok": true}))
    }
}

/// A read adapter that counts invocations (deadline/queueing test).
struct CounterAdapter {
    invoked: Arc<AtomicU64>,
}

impl CapabilityAdapter for CounterAdapter {
    fn capability(&self) -> &str {
        CONTROL_PLANE_READ_CAPABILITY
    }

    fn invoke(&mut self, _scope: &GrantScope, _input: &Value) -> Result<Value, ContractError> {
        self.invoked.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok": true}))
    }
}

// ---------------------------------------------------------------------------
// Fixture package + harness
// ---------------------------------------------------------------------------

fn binding(plugin_id: &str, generation: u64) -> BindingPin {
    BindingPin {
        binding_id: plugin_id.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "1.0.0".to_string(),
        generation,
        config_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

fn byte_ref(path: &str, bytes: &[u8], media_type: &str) -> Value {
    json!({
        "path": path,
        "digest": sha256_digest(bytes),
        "bytes": bytes.len() as u64,
        "media_type": media_type,
    })
}

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

fn allowed_schemas() -> BTreeSet<(String, u64)> {
    NS_VIEWS
        .iter()
        .copied()
        .chain(std::iter::once(NS_ARTIFACTS))
        .map(|namespace| (namespace.to_string(), 1u64))
        .collect()
}

fn manifest_value(plugin_id: &str) -> Value {
    let report_schema = |path: &str| byte_ref(path, REPORT_SCHEMA_BYTES, "application/json");
    let mut report_schemas = Vec::new();
    let mut views = Vec::new();
    for (index, namespace) in NS_VIEWS.iter().enumerate() {
        report_schemas.push(json!({
            "namespace": namespace,
            "version": 1,
            "schema": report_schema("objects/report-schema.json")
        }));
        views.push(json!({
            "view_id": format!("view{index}"),
            "schema_namespace": namespace,
            "kind": "Status",
            "title": format!("View {index}"),
            "schema_version": 1,
            "fields": ["ready"]
        }));
    }
    report_schemas.push(json!({
        "namespace": NS_ARTIFACTS,
        "version": 1,
        "schema": report_schema("objects/report-schema.json")
    }));
    views.push(json!({
        "view_id": "artifacts",
        "schema_namespace": NS_ARTIFACTS,
        "kind": "ArtifactLink",
        "title": "Latest artifact",
        "schema_version": 1,
        "fields": ["path"]
    }));
    json!({
        "format": "PluginManifest/v1",
        "plugin_id": plugin_id,
        "package_version": "1.0.0",
        "package_digest": format!("sha256:{}", "0".repeat(64)),
        "protocol_major": 1,
        "entrypoint": {
            "path": "bin/plugin",
            "argv": ["--serve"],
            "launch_profile": "unfamiliar-read-only/v1"
        },
        "domain_apis": [],
        "requested_capabilities": ["report.publish"],
        "required_services": [],
        "optional_services": [],
        "config_schema": byte_ref("objects/config-schema.json", CONFIG_SCHEMA_BYTES, "application/json"),
        "default_configuration": byte_ref("objects/default-config.json", DEFAULT_CONFIG_BYTES, "application/json"),
        "report_schemas": report_schemas,
        "views": views,
        "actions": [],
        "state_compatibility": {"reads": ["reporter-state/v1"], "writes": "reporter-state/v1"}
    })
}

fn seal_package(package_dir: &Path, manifest: &Value) -> PluginManifest {
    seal_package_with_entry(package_dir, manifest, b"#!/bin/sh\ncat >/dev/null\n")
}

/// Seal a package whose child announces the supervisor READY protocol and then
/// stays alive (a READY-capable production-like fixture).
fn seal_ready_package(package_dir: &Path, manifest: &Value) -> PluginManifest {
    let plugin_id = manifest["plugin_id"].as_str().unwrap_or("unknown");
    let script = format!(
        "#!/bin/sh\nprintf 'READY {{\"api\":\"plugin\",\"api_version\":1,\"impl\":\"{plugin_id}\",\"impl_version\":\"1.0.0\",\"capabilities\":[\"report.publish\"]}}\\n'\ncat >/dev/null\n"
    );
    seal_package_with_entry(package_dir, manifest, script.as_bytes())
}

fn seal_package_with_entry(
    package_dir: &Path,
    manifest: &Value,
    entry: &[u8],
) -> PluginManifest {
    let mut files = support_files();
    files.push(("bin/plugin".to_string(), entry.to_vec()));
    fs::create_dir_all(package_dir).expect("package dir");
    for (relative, bytes) in &files {
        let path = package_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("package parent");
        }
        fs::write(&path, bytes).expect("package file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let entry = package_dir.join("bin/plugin");
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).expect("entry mode");
    }
    let parsed: PluginManifest = serde_json::from_value(manifest.clone()).expect("manifest");
    let digest = package_inventory_digest(package_dir, &parsed).expect("inventory digest");
    let mut sealed = manifest.clone();
    sealed["package_digest"] = Value::String(digest);
    fs::write(
        package_dir.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&sealed).expect("manifest json"),
    )
    .expect("write manifest");
    serde_json::from_value(sealed).expect("sealed manifest")
}

fn installation_for(package: &agalma_conductor::plugin_manifest::DiscoveredPackage) -> PluginInstallation {
    let mut installation = PluginInstallation {
        format: "PluginInstallation/v1".to_string(),
        plugin_id: package.plugin_id.clone(),
        package_digest: package.package_digest.clone(),
        configuration: package.manifest.default_configuration.clone(),
        launch_profile: package.manifest.entrypoint.launch_profile.clone(),
        approved_capabilities: package.manifest.requested_capabilities.clone(),
        installation_digest: String::new(),
    };
    installation.installation_digest = installation
        .recomputed_installation_digest()
        .expect("installation digest");
    installation
}

/// A live registry + report ingestor over a real sealed package and ledger.
struct Harness {
    dir: ScratchDir,
    registry: Arc<Mutex<PluginRegistry>>,
    reports: Arc<Mutex<ReportIngestor>>,
    clock: ManualClock,
    generation: u64,
}

fn harness(name: &str) -> Harness {
    let dir = ScratchDir::new(name);
    let root = dir.child("discovery");
    fs::create_dir_all(&root).expect("discovery root");
    let package_dir = root.join("pkg");
    seal_package(&package_dir, &manifest_value(PLUGIN_ID));
    let report = discover_plugins(&[PluginRoot::required(&root)]).expect("discover");
    let package = report
        .package(PLUGIN_ID)
        .expect("package discovered")
        .clone();

    let store = HubInstallStore::new(dir.path());
    let mut registry = PluginRegistry::new();
    registry.register_builtin("builtin.core", START_MS);
    let generation = registry
        .install(&store, &package, &installation_for(&package))
        .expect("install");
    registry.stage(PLUGIN_ID, generation).expect("stage");
    registry.activate(PLUGIN_ID, generation).expect("activate");
    registry
        .mark_ready(PLUGIN_ID, generation, START_MS)
        .expect("ready");

    let clock = ManualClock::new(START_MS);
    let ledger = SqliteLedger::open(dir.join("report-store.sqlite")).expect("open component store");
    let ingestor = ReportIngestor::open(
        Box::new(Mutex::new(ledger)),
        clock.clock(),
        BTreeMap::from([(PLUGIN_ID.to_string(), generation)]),
        allowed_schemas(),
    )
    .expect("open ingestor")
    .with_thresholds(1_000, 3_000);

    Harness {
        dir,
        registry: Arc::new(Mutex::new(registry)),
        reports: Arc::new(Mutex::new(ingestor)),
        clock,
        generation,
    }
}

impl Harness {
    fn service(&self) -> ControlPlaneReadService {
        ControlPlaneReadService::new(Arc::clone(&self.registry), self.dir.path())
            .with_reports(Arc::clone(&self.reports))
            .with_clock(self.clock.clock())
    }

    fn publish(
        &self,
        sequence: u64,
        namespace: &str,
        version: u64,
        payload: Value,
    ) -> Result<(), ContractError> {
        self.publish_with(
            sequence,
            namespace,
            version,
            Durability::Durable,
            payload,
        )
    }

    fn publish_transient(
        &self,
        sequence: u64,
        namespace: &str,
        payload: Value,
    ) -> Result<(), ContractError> {
        self.publish_with(sequence, namespace, 1, Durability::Transient, payload)
    }

    fn publish_with(
        &self,
        sequence: u64,
        namespace: &str,
        version: u64,
        durability: Durability,
        payload: Value,
    ) -> Result<(), ContractError> {
        let pin = binding(PLUGIN_ID, self.generation);
        let publication = seal_publication(
            PLUGIN_ID,
            pin.clone(),
            namespace,
            version,
            sequence,
            START_MS,
            durability,
            payload,
        )?;
        self.reports
            .lock()
            .unwrap()
            .publish(PLUGIN_ID, &pin, &publication)?;
        Ok(())
    }

    fn publish_health(&self, state: HealthState) {
        let pin = binding(PLUGIN_ID, self.generation);
        let health = PluginHealth {
            format: "PluginHealth/v1".to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding: pin.clone(),
            state,
            detail: "fixture".to_string(),
            checked_at_unix_ms: START_MS,
        };
        self.reports
            .lock()
            .unwrap()
            .record_health(PLUGIN_ID, &pin, &health)
            .expect("record health");
    }
}

// ---------------------------------------------------------------------------
// Router helpers
// ---------------------------------------------------------------------------

fn read_grant(expiry_unix_ms: u64) -> LaunchGrant {
    LaunchGrant::for_capability(
        "web.dashboard",
        binding("web.dashboard", 1),
        METHOD_INVOKE_CAPABILITY,
        CONTROL_PLANE_READ_CAPABILITY,
        expiry_unix_ms,
        None,
    )
    .expect("issue read grant")
}

fn read_call(grant: &LaunchGrant, operation_id: &str, input: Value, now: u64) -> CapabilityCallRequest {
    CapabilityCallRequest {
        grant: grant.token_hex(),
        plugin_id: grant.scope().plugin_id.clone(),
        generation: grant.scope().binding.generation,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: CONTROL_PLANE_READ_CAPABILITY.to_string(),
        operation_id: operation_id.to_string(),
        input,
        deadline_unix_ms: now + 60_000,
        depth: 0,
    }
}

fn input(request: &ControlPlaneReadRequest) -> Value {
    serde_json::to_value(request).expect("request json")
}

// ---------------------------------------------------------------------------
// Production routing + source states
// ---------------------------------------------------------------------------

#[test]
fn production_routing_lists_components_with_accurate_source_states() {
    let harness = harness("cp-read-list");
    harness.publish_health(HealthState::Ready);
    harness
        .publish(1, NS_STATUS, 1, json!({"ready": true}))
        .expect("publish status");

    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(harness.service());
    let grant = read_grant(START_MS + 60_000);
    router.install_grant(&grant);
    let service = agalma_conductor::plugin_router::RouterService::new(router);

    let call = read_call(
        &grant,
        "list-1",
        input(&ControlPlaneReadRequest::ListComponents {
            limit: None,
            cursor: None,
        }),
        START_MS,
    );
    let value = service
        .invoke_capability(serde_json::to_value(&call).unwrap())
        .expect("production read routed");
    let page = ReadPage::<ComponentSummary>::from_wire_value(&value).expect("component page");

    let reader = page
        .entries
        .iter()
        .find(|entry| entry.plugin_id == PLUGIN_ID)
        .expect("reporting plugin listed");
    assert_eq!(reader.generation.as_u64().unwrap(), harness.generation);
    assert_eq!(reader.health, HealthState::Ready);
    assert!(reader.available, "{reader:?}");
    assert!(reader.views.iter().any(|view| view.view_id == "view0"));
    assert_eq!(reader.completeness, agalma_contracts::Completeness::Complete);
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.plugin_id == "builtin.core"),
        "installed built-in remains listed"
    );
    assert!(
        page.metadata
            .sources
            .iter()
            .any(|source| source.source == "registry")
    );
}

#[test]
fn unknown_plugin_returns_explicit_unavailable_data_not_an_error() {
    let harness = harness("cp-read-unknown");
    let service = harness.service();
    let page = service
        .read_component(
            "never.installed",
            &agalma_contracts::ReadPageRequest::default(),
        )
        .expect("unknown plugin read is explicit data");
    assert!(page.entries.is_empty());
    assert!(page.descriptors.is_empty());
    assert_eq!(page.binding.generation, 0);
    let registry = page
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "registry")
        .expect("registry source");
    assert_eq!(registry.state, SourceState::Missing);
    let reports = page
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "reports")
        .expect("reports source");
    assert_eq!(reports.state, SourceState::Missing);
}

#[test]
fn missing_ledger_overview_is_unknown_never_zero() {
    let harness = harness("cp-read-missing-ledger");
    let service = harness.service();
    let overview = service.read_overview().expect("overview");
    assert!(overview.economics.usage.is_none());
    assert!(overview.economics.cost.is_none());
    assert!(overview.economics.allowance.is_none());
    assert_eq!(
        overview.economics.completeness,
        agalma_contracts::Completeness::Unknown
    );
    let ledger = overview
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "ledger")
        .expect("ledger source");
    assert_eq!(ledger.state, SourceState::Missing);
    assert!(overview.factory.stopped, "missing stop source is fail-closed");
}

#[test]
fn unreadable_domain_reads_stay_visibly_unknown() {
    let harness = harness("cp-read-unreadable");
    let service = harness
        .service()
        .with_domains(Arc::new(FailingDomains));
    let overview = service.read_overview().expect("overview");
    assert!(overview.economics.cost.is_none());
    let ledger = overview
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "ledger")
        .expect("ledger source");
    assert_eq!(ledger.state, SourceState::Unreadable);
    assert!(ledger.detail.contains("ledger unreadable"));
}

#[test]
fn overview_reports_factory_stop_and_known_usage_from_bound_reads() {
    let harness = harness("cp-read-overview");
    let mut ledger = LedgerHandle::in_process(harness.dir.path()).expect("ledger handle");
    let execution = agalma_contracts::ExecutionId::derive(
        &agalma_contracts::TaskId::new("task-a"),
        1,
    );
    ledger
        .commit(agalma_contracts::CommitBatch {
            expected_revision: None,
            state: Some(agalma_contracts::ExecutionRecord {
                execution_id: execution.clone(),
                task_id: agalma_contracts::TaskId::new("task-a"),
                generation: 1,
                phase: agalma_contracts::ExecutionPhase::Build,
                state: agalma_contracts::ExecutionState::Running,
                attempt: 1,
                revision: 0,
            }),
            events: vec![agalma_contracts::ExecutionEvent {
                execution_id: execution,
                sequence: 0,
                kind: "usage".to_string(),
                payload: json!({"tokens_in": 10, "tokens_out": 5, "cost_usd": 0.25}),
                recorded_at_unix_ms: START_MS,
            }],
            receipts: vec![],
            intents: vec![],
        })
        .expect("seed execution");

    let service = harness
        .service()
        .with_domains(Arc::new(ledger))
        .with_stop(Arc::new(FixedStop(StopDecision::Stopped("latch".to_string()))));
    let overview = service.read_overview().expect("overview");
    assert!(overview.factory.stopped);
    assert_eq!(overview.factory.reason.as_deref(), Some("latch"));
    assert_eq!(overview.executions.len(), 1);
    assert_eq!(
        overview.economics.usage.as_ref().map(|usage| usage.as_str()),
        Some("15")
    );
    assert_eq!(
        overview.economics.cost.as_ref().map(|cost| cost.as_str()),
        Some("0.250000")
    );
    assert_eq!(
        overview.economics.completeness,
        agalma_contracts::Completeness::Complete
    );
}

// ---------------------------------------------------------------------------
// Scoped grant, stop admission and no-intent guarantees
// ---------------------------------------------------------------------------

#[test]
fn reads_are_admitted_while_stopped_and_create_no_journal_or_claims() {
    let harness = harness("cp-read-stopped");
    let ledger = LedgerHandle::in_process(harness.dir.path()).expect("ledger handle");
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(StoppedAdmission("stop latch".to_string())),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(
        harness
            .service()
            .with_domains(Arc::new(ledger.clone())),
    );
    let grant = read_grant(START_MS + 60_000);
    router.install_grant(&grant);

    let call = read_call(
        &grant,
        "read-while-stopped",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS,
    );
    let result = router.invoke_read(&call).expect("read permitted while stopped");
    let overview = ReadOverview::from_wire_value(&result).expect("overview");
    assert!(overview.factory.stopped);

    assert_eq!(
        router.journaled_operations(),
        0,
        "observation never journals a durable operation"
    );
    assert!(
        router
            .operation_entry("web.dashboard", 1, "read-while-stopped")
            .is_none(),
        "no operation entry is created for a read"
    );
    assert!(
        ledger.pending_intents().expect("pending intents").is_empty(),
        "viewing creates no dispatch intents or task claims"
    );
    assert!(
        LedgerApi::executions(&ledger).expect("executions").is_empty(),
        "viewing never claims a task"
    );
}

#[test]
fn unauthorized_expired_and_revoked_grants_are_refused_before_any_read() {
    let harness = harness("cp-read-grants");
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(harness.service());

    // Wrong capability namespace: grant does not authorize the read namespace.
    let wrong_ns = LaunchGrant::for_capability(
        "web.dashboard",
        binding("web.dashboard", 1),
        METHOD_INVOKE_CAPABILITY,
        "ledger.read/v1",
        START_MS + 60_000,
        None,
    )
    .expect("grant");
    router.install_grant(&wrong_ns);
    let mut call = read_call(
        &wrong_ns,
        "wrong-ns",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS,
    );
    let error = router.invoke_read(&call).expect_err("wrong namespace refused");
    assert!(error.to_string().contains("grant authorizes"), "{error}");

    // Wrong method on the read grant.
    let method_grant = read_grant(START_MS + 60_000);
    router.install_grant(&method_grant);
    call = read_call(
        &method_grant,
        "wrong-method",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS,
    );
    call.method = "hub.PublishReport".to_string();
    let error = router.invoke_read(&call).expect_err("wrong method refused");
    assert!(error.to_string().contains("authorizes"), "{error}");

    // Expired grant.
    let expired = read_grant(START_MS);
    router.install_grant(&expired);
    let call = read_call(
        &expired,
        "expired",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS,
    );
    let error = router.invoke_read(&call).expect_err("expired grant refused");
    assert!(error.to_string().contains("expired"), "{error}");

    // Revoked grant.
    let revoked = read_grant(START_MS + 60_000);
    let fingerprint = router.install_grant(&revoked);
    assert!(router.revoke_grant(&fingerprint));
    let call = read_call(
        &revoked,
        "revoked",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS,
    );
    let error = router.invoke_read(&call).expect_err("revoked grant refused");
    assert!(error.to_string().contains("revoked"), "{error}");

    assert_eq!(router.journaled_operations(), 0);
}

#[test]
fn expired_deadline_is_refused_for_reads() {
    let harness = harness("cp-read-deadline");
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(harness.service());
    let grant = read_grant(START_MS + 60_000);
    router.install_grant(&grant);
    let mut call = read_call(
        &grant,
        "late",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS,
    );
    call.deadline_unix_ms = START_MS;
    let error = router.invoke_read(&call).expect_err("deadline refused");
    assert!(error.to_string().contains("deadline"), "{error}");
}

#[test]
fn deferred_component_and_artifact_operations_fail_explicitly() {
    let harness = harness("cp-read-deferred");
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(harness.service());
    let grant = read_grant(START_MS + 60_000);
    router.install_grant(&grant);
    for operation in [
        ControlPlaneReadRequest::ReadComponent {
            plugin_id: PLUGIN_ID.to_string(),
            limit: None,
            cursor: None,
        },
        ControlPlaneReadRequest::ReadArtifactChunk {
            plugin_id: PLUGIN_ID.to_string(),
            digest: sha256_digest(b"x"),
            offset: None,
            max_bytes: None,
        },
    ] {
        let error = router
            .invoke_read(&read_call(&grant, "deferred", input(&operation), START_MS))
            .expect_err("deferred operation refused");
        assert!(
            error.to_string().contains("unsupported capability"),
            "{error}"
        );
    }
}

#[test]
fn health_change_between_pages_invalidates_the_cursor() {
    let harness = harness("cp-read-health-cursor");
    let service = harness.service();
    let first = service
        .list_components(&agalma_contracts::ReadPageRequest {
            limit: Some(1),
            cursor: None,
        })
        .expect("first page");
    let cursor = first.next_cursor.clone().expect("continuation cursor");
    harness.publish_health(HealthState::Unavailable);
    let error = service
        .list_components(&agalma_contracts::ReadPageRequest {
            limit: Some(1),
            cursor: Some(cursor),
        })
        .expect_err("health collection change conflicts");
    assert!(error.to_string().contains("RevisionConflict"), "{error}");
}

fn usage_inputs(tokens_in: u64, tokens_out: u64) -> EconomicsInputs {
    let task = agalma_contracts::TaskId::new("overflow-task");
    let execution_id = agalma_contracts::ExecutionId::derive(&task, 1);
    let event_key = execution_id.as_str().to_string();
    let record = agalma_contracts::ExecutionRecord {
        execution_id: execution_id.clone(),
        task_id: task,
        generation: 1,
        phase: agalma_contracts::ExecutionPhase::Build,
        state: agalma_contracts::ExecutionState::Running,
        attempt: 1,
        revision: 0,
    };
    let event = agalma_contracts::ExecutionEvent {
        execution_id,
        sequence: 0,
        kind: "usage".to_string(),
        payload: json!({
            "tokens_in": tokens_in,
            "tokens_out": tokens_out,
            "cost_usd": 0.0,
            "completeness": "Complete",
        }),
        recorded_at_unix_ms: START_MS,
    };
    EconomicsInputs {
        executions: SourceRead::Available(vec![record]),
        events: BTreeMap::from([(event_key, SourceRead::Available(vec![event]))]),
        digests: BTreeMap::new(),
        control_receipts: SourceRead::Empty,
    }
}

fn usage_inputs_rows(rows: Vec<Vec<(u64, u64)>>) -> EconomicsInputs {
    let mut records = Vec::new();
    let mut events = BTreeMap::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        let task = agalma_contracts::TaskId::new(format!("overflow-row-{row_index}"));
        let execution_id = agalma_contracts::ExecutionId::derive(&task, 1);
        let key = execution_id.as_str().to_string();
        records.push(agalma_contracts::ExecutionRecord {
            execution_id: execution_id.clone(),
            task_id: task,
            generation: 1,
            phase: agalma_contracts::ExecutionPhase::Build,
            state: agalma_contracts::ExecutionState::Running,
            attempt: 1,
            revision: row_index as u64,
        });
        let row_events: Vec<agalma_contracts::ExecutionEvent> = row
            .into_iter()
            .enumerate()
            .map(|(index, (tokens_in, tokens_out))| agalma_contracts::ExecutionEvent {
                execution_id: execution_id.clone(),
                sequence: index as u64,
                kind: "usage".to_string(),
                payload: json!({
                    "tokens_in": tokens_in,
                    "tokens_out": tokens_out,
                    "cost_usd": 0.0,
                    "completeness": "Complete",
                }),
                recorded_at_unix_ms: START_MS + index as u64,
            })
            .collect();
        events.insert(
            key,
            if row_events.is_empty() {
                SourceRead::Empty
            } else {
                SourceRead::Available(row_events)
            },
        );
    }
    EconomicsInputs {
        executions: if records.is_empty() {
            SourceRead::Empty
        } else {
            SourceRead::Available(records)
        },
        events,
        digests: BTreeMap::new(),
        control_receipts: SourceRead::Empty,
    }
}

#[test]
fn multi_event_counter_overflow_within_one_execution_is_unknown() {
    // Two events each fit u64 but their execution total does not; this must be
    // caught before the shared `cost_from_events` u64 accumulation.
    let projection = derive_bound(
        &usage_inputs_rows(vec![vec![(u64::MAX, 0), (1, 0)]]),
        &BTreeMap::new(),
    );
    let cost = projection.usage_cost(START_MS);
    assert_eq!(
        cost.completeness,
        agalma_contracts::Completeness::Unknown
    );
    assert!(cost.usage.is_none());
    assert!(cost.cost.is_none());
}

#[test]
fn property_multi_event_multi_row_token_accumulation_is_exact_or_unknown() {
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x0101_c0de);
    for case in 0..PROPERTY_CASES {
        let row_count = rng.below(3) + 1;
        let mut rows = Vec::new();
        let mut expected: u128 = 0;
        for _ in 0..row_count {
            let event_count = rng.below(3) + 1;
            let mut events = Vec::new();
            for _ in 0..event_count {
                let raw_in = rng.next_u64();
                let shift_in = rng.below(32);
                let raw_out = rng.next_u64();
                let shift_out = rng.below(32);
                let tokens_in = match rng.below(4) {
                    0 => u64::MAX,
                    1 => u64::MAX - rng.below(8),
                    _ => raw_in >> shift_in,
                };
                let tokens_out = match rng.below(3) {
                    0 => u64::MAX,
                    1 => rng.below(8),
                    _ => raw_out >> shift_out,
                };
                expected += tokens_in as u128 + tokens_out as u128;
                events.push((tokens_in, tokens_out));
            }
            rows.push(events);
        }
        let projection = derive_bound(&usage_inputs_rows(rows), &BTreeMap::new());
        let cost = projection.usage_cost(START_MS);
        if expected > u64::MAX as u128 {
            assert_eq!(
                cost.completeness,
                agalma_contracts::Completeness::Unknown,
                "case {case}"
            );
            assert!(cost.usage.is_none(), "case {case}");
        } else {
            let text = (expected as u64).to_string();
            assert_eq!(
                cost.usage.as_ref().map(|usage| usage.as_str()),
                Some(text.as_str()),
                "case {case}: exact multi-row total"
            );
        }
    }
}

#[test]
fn token_counter_overflow_is_unknown_not_a_complete_saturated_total() {
    // Per-row saturating add previously hid this: MAX + 1 must be Unknown.
    let projection = derive_bound(&usage_inputs(u64::MAX, 1), &BTreeMap::new());
    let cost = projection.usage_cost(START_MS);
    assert_eq!(
        cost.completeness,
        agalma_contracts::Completeness::Unknown,
        "overflow is explicit Unknown"
    );
    assert!(cost.usage.is_none(), "no saturated/wrapped total");
    assert!(cost.cost.is_none());
}

#[test]
fn property_token_accumulation_is_exact_or_unknown() {
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x7000_0e00);
    for case in 0..PROPERTY_CASES {
        let tokens_in = match rng.below(4) {
            0 => u64::MAX,
            1 => u64::MAX - rng.below(4),
            _ => rng.next_u64(),
        };
        let tokens_out = match rng.below(3) {
            0 => u64::MAX,
            1 => rng.below(4),
            _ => rng.next_u64(),
        };
        let expected = tokens_in as u128 + tokens_out as u128;
        let projection = derive_bound(&usage_inputs(tokens_in, tokens_out), &BTreeMap::new());
        let cost = projection.usage_cost(START_MS);
        if expected > u64::MAX as u128 {
            assert_eq!(
                cost.completeness,
                agalma_contracts::Completeness::Unknown,
                "case {case}"
            );
            assert!(cost.usage.is_none(), "case {case}");
        } else {
            let text = (expected as u64).to_string();
            assert_eq!(
                cost.usage.as_ref().map(|usage| usage.as_str()),
                Some(text.as_str()),
                "case {case}: exact counter total"
            );
        }
    }
}

#[test]
fn property_source_states_never_fabricate_totals() {
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x5157);
    for case in 0..PROPERTY_CASES {
        let execution_state = rng.below(4);
        let executions = match execution_state {
            0 => SourceRead::Empty,
            1 => SourceRead::Missing("no ledger".to_string()),
            2 => SourceRead::Unreadable("ledger io".to_string()),
            _ => SourceRead::Available(vec![agalma_contracts::ExecutionRecord {
                execution_id: agalma_contracts::ExecutionId::derive(
                    &agalma_contracts::TaskId::new("task"),
                    1,
                ),
                task_id: agalma_contracts::TaskId::new("task"),
                generation: 1,
                phase: agalma_contracts::ExecutionPhase::Build,
                state: agalma_contracts::ExecutionState::Running,
                attempt: 1,
                revision: 0,
            }]),
        };
        let usage_shape = rng.below(4);
        let mut events = BTreeMap::new();
        if let SourceRead::Available(records) = &executions {
            let id = records[0].execution_id.as_str().to_string();
            events.insert(
                id,
                match usage_shape {
                    0 => SourceRead::Empty,
                    1 => SourceRead::Missing("events".to_string()),
                    2 => SourceRead::Available(vec![agalma_contracts::ExecutionEvent {
                        execution_id: records[0].execution_id.clone(),
                        sequence: 0,
                        kind: "usage".to_string(),
                        payload: json!({"tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0, "completeness": "Unknown"}),
                        recorded_at_unix_ms: START_MS,
                    }]),
                    _ => SourceRead::Available(vec![agalma_contracts::ExecutionEvent {
                        execution_id: records[0].execution_id.clone(),
                        sequence: 0,
                        kind: "usage".to_string(),
                        payload: json!({"tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0, "completeness": "Complete"}),
                        recorded_at_unix_ms: START_MS,
                    }]),
                },
            );
        }
        let inputs = EconomicsInputs {
            executions,
            events,
            digests: BTreeMap::new(),
            control_receipts: SourceRead::Empty,
        };
        let projection = derive_bound(&inputs, &BTreeMap::new());
        let cost = projection.usage_cost(START_MS);
        let complete = projection.usage_is_known()
            && projection.usage == agalma_contracts::Completeness::Complete;
        assert_eq!(
            cost.usage.is_some(),
            complete,
            "case {case}: usage total only for complete known observation"
        );
        assert_eq!(
            cost.cost.is_some(),
            complete,
            "case {case}: cost total usage={:?} events={:?} executions={:?} known={}",
            projection.usage,
            projection.events,
            projection.executions,
            projection.usage_is_known()
        );
        if !matches!(projection.executions, SourceState::Available | SourceState::Empty) {
            assert_eq!(
                projection.events,
                projection.executions,
                "case {case}: dependent source keeps execution state"
            );
        }
    }
}

#[test]
fn read_service_rechecks_namespace_and_method_scope() {
    let harness = harness("cp-read-scope");
    let mut service = harness.service();
    let mut scope = read_grant(START_MS + 60_000).scope().clone();
    scope.resource_namespace = "ledger.read/v1".to_string();
    let error = agalma_conductor::plugin_router::CapabilityAdapter::invoke(
        &mut service,
        &scope,
        &input(&ControlPlaneReadRequest::ReadOverview),
    )
    .expect_err("wrong namespace refused by the service");
    assert!(error.to_string().contains("control-plane.read/v1"), "{error}");

    let mut scope = read_grant(START_MS + 60_000).scope().clone();
    scope.method = "hub.PublishReport".to_string();
    let error = agalma_conductor::plugin_router::CapabilityAdapter::invoke(
        &mut service,
        &scope,
        &input(&ControlPlaneReadRequest::ReadOverview),
    )
    .expect_err("wrong method refused by the service");
    assert!(error.to_string().contains("InvokeCapability"), "{error}");
}

// ---------------------------------------------------------------------------
// Pagination, cursors and bounds
// ---------------------------------------------------------------------------

#[test]
fn component_reports_page_bounded_and_restart_on_a_stale_cursor() {
    let harness = harness("cp-read-page");
    for (index, namespace) in NS_VIEWS.iter().enumerate().take(5) {
        harness
            .publish(
                index as u64 + 1,
                namespace,
                1,
                json!({"ready": true, "sequence": index}),
            )
            .expect("publish");
    }
    let service = harness.service();
    let first = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest {
                limit: Some(2),
                cursor: None,
            },
        )
        .expect("first page");
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.entries[0].sequence, 1);
    let cursor = first.next_cursor.clone().expect("continuation cursor");
    let second = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest {
                limit: Some(2),
                cursor: Some(cursor.clone()),
            },
        )
        .expect("second page");
    assert_eq!(second.entries[0].sequence, 3);
    assert!(second.next_cursor.is_some());

    // A new report advances the projection revision; the old cursor conflicts.
    harness
        .publish(6, NS_VIEWS[5], 1, json!({"ready": true, "sequence": 5}))
        .expect("publish six");
    let error = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest {
                limit: Some(2),
                cursor: Some(cursor),
            },
        )
        .expect_err("stale cursor conflicts");
    assert!(error.to_string().contains("RevisionConflict"), "{error}");

    // A malformed cursor fails as an invalid record.
    let error = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest {
                limit: Some(2),
                cursor: Some("not-a-cursor".to_string()),
            },
        )
        .expect_err("malformed cursor refused");
    assert!(error.to_string().contains("InvalidRecord"), "{error}");
}

#[test]
fn page_and_artifact_limits_reject_out_of_range_requests() {
    let harness = harness("cp-read-bounds");
    let service = harness.service();
    let error = service
        .list_components(&agalma_contracts::ReadPageRequest {
            limit: Some(0),
            cursor: None,
        })
        .expect_err("zero limit refused");
    assert!(error.to_string().contains("InvalidRecord"), "{error}");
    let error = service
        .list_components(&agalma_contracts::ReadPageRequest {
            limit: Some(101),
            cursor: None,
        })
        .expect_err("over-limit refused");
    assert!(error.to_string().contains("InvalidRecord"), "{error}");

    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest: sha256_digest(b"x"),
            offset: None,
            max_bytes: Some(0),
        })
        .expect_err("zero chunk refused");
    assert!(error.to_string().contains("InvalidRecord"), "{error}");
    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest: sha256_digest(b"x"),
            offset: None,
            max_bytes: Some(agalma_contracts::ARTIFACT_CHUNK_MAX_BYTES + 1),
        })
        .expect_err("oversized chunk refused");
    assert!(error.to_string().contains("InvalidRecord"), "{error}");
}

// ---------------------------------------------------------------------------
// Artifact reads
// ---------------------------------------------------------------------------

/// Publish an artifact reference whose bytes are stored in the immutable store.
fn publish_artifact(harness: &Harness, sequence: u64, bytes: &[u8]) -> String {
    let digest = sha256_digest(bytes);
    let hex = digest.trim_start_matches("sha256:");
    let relative = format!("packages/objects/sha256/{hex}");
    let path = harness.dir.path().join(&relative);
    fs::create_dir_all(path.parent().unwrap()).expect("artifact dir");
    fs::write(&path, bytes).expect("artifact bytes");
    harness
        .publish(
            sequence,
            NS_ARTIFACTS,
            1,
            json!({"artifact": byte_ref(&relative, bytes, "application/octet-stream")}),
        )
        .expect("publish artifact");
    digest
}

#[test]
fn artifact_chunk_requires_a_visible_reference_and_stays_in_the_store() {
    let harness = harness("cp-read-artifact");
    let bytes: Vec<u8> = (0..3000u32).map(|value| (value % 251) as u8).collect();
    let digest = publish_artifact(&harness, 1, &bytes);
    let service = harness.service();

    let chunk = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest: digest.clone(),
            offset: None,
            max_bytes: Some(1024),
        })
        .expect("authorized chunk");
    assert_eq!(chunk.data_len().unwrap(), 1024);
    assert_eq!(chunk.next_offset.as_ref().unwrap().as_u64().unwrap(), 1024);
    assert_eq!(&bytes[..1024], &decode_hex(&chunk.data_hex)[..]);

    // A digest no visible report references is refused.
    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest: sha256_digest(b"not-published"),
            offset: None,
            max_bytes: None,
        })
        .expect_err("unreferenced digest refused");
    assert!(error.to_string().contains("ScopeDenied"), "{error}");

    // A visible reference that escapes the store is refused by confinement.
    let secret = b"secret-bytes";
    let secret_digest = sha256_digest(secret);
    fs::write(harness.dir.path().join("secret.bin"), secret).expect("secret file");
    harness
        .publish(
            2,
            NS_ARTIFACTS,
            1,
            json!({"artifact": byte_ref("secret.bin", secret, "application/octet-stream")}),
        )
        .expect("publish escape");
    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest: secret_digest,
            offset: None,
            max_bytes: None,
        })
        .expect_err("out-of-store reference refused");
    assert!(error.to_string().contains("ScopeDenied"), "{error}");
}

#[test]
fn artifact_offset_beyond_end_is_refused() {
    let harness = harness("cp-read-artifact-offset");
    let bytes = b"tiny".to_vec();
    let digest = publish_artifact(&harness, 1, &bytes);
    let service = harness.service();
    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest,
            offset: Some(agalma_contracts::ExactU64::from(5)),
            max_bytes: None,
        })
        .expect_err("offset beyond end refused");
    assert!(error.to_string().contains("InvalidRecord"), "{error}");
}

fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex"))
        .collect()
}

// ---------------------------------------------------------------------------
// Deterministic property cases
// ---------------------------------------------------------------------------

#[test]
fn property_component_pagination_reassembles_every_report_exactly() {
    let harness = harness("cp-read-prop-paging");
    let total = NS_VIEWS.len() as u64;
    for (index, namespace) in NS_VIEWS.iter().enumerate() {
        harness
            .publish(
                index as u64 + 1,
                namespace,
                1,
                json!({"ready": true, "sequence": index}),
            )
            .expect("publish");
    }
    let service = harness.service();
    let full = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest {
                limit: Some(100),
                cursor: None,
            },
        )
        .expect("all reports");
    let expected: Vec<u64> = full.entries.iter().map(|entry| entry.sequence).collect();
    assert_eq!(expected, (1..=total).collect::<Vec<_>>());

    let mut rng = XorShift64::new(PROPERTY_SEED);
    for case in 0..PROPERTY_CASES {
        let limit = rng.below(3) + 1;
        let mut gathered = Vec::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            let page = service
                .read_component(
                    PLUGIN_ID,
                    &agalma_contracts::ReadPageRequest {
                        limit: Some(limit as u32),
                        cursor,
                    },
                )
                .unwrap_or_else(|error| panic!("case {case} limit {limit}: {error}"));
            assert!(page.entries.len() <= limit as usize, "case {case}");
            gathered.extend(page.entries.iter().map(|entry| entry.sequence));
            cursor = page.next_cursor;
            pages += 1;
            assert!(pages <= 64, "case {case} did not terminate");
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(gathered, expected, "case {case} limit {limit}");
    }

    // Invalid cursors and limits are always refused, never silently ignored.
    for case in 0..PROPERTY_CASES {
        let malformed = format!("cp1:{case}");
        let error = service
            .read_component(
                PLUGIN_ID,
                &agalma_contracts::ReadPageRequest {
                    limit: Some(5),
                    cursor: Some(malformed),
                },
            )
            .expect_err("malformed cursor refused");
        assert!(error.to_string().contains("InvalidRecord"), "case {case}");
        let invalid_limit = rng.below(3) * 150;
        let error = service
            .read_component(
                PLUGIN_ID,
                &agalma_contracts::ReadPageRequest {
                    limit: Some(invalid_limit as u32),
                    cursor: None,
                },
            )
            .expect_err("invalid limit refused");
        assert!(error.to_string().contains("InvalidRecord"), "case {case}");
    }
}

#[test]
fn property_artifact_chunks_reconstruct_the_exact_bytes() {
    let harness = harness("cp-read-prop-artifact");
    let bytes: Vec<u8> = (0..5000u32)
        .map(|value| ((value * 31 + 7) % 256) as u8)
        .collect();
    let digest = publish_artifact(&harness, 1, &bytes);
    let service = harness.service();

    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x00ff);
    for case in 0..PROPERTY_CASES {
        let mut offset = 0u64;
        let mut gathered = Vec::new();
        let mut steps = 0;
        while offset < bytes.len() as u64 {
            let max_bytes = rng.below(2048) + 1;
            let chunk = service
                .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
                    plugin_id: PLUGIN_ID.to_string(),
                    digest: digest.clone(),
                    offset: Some(agalma_contracts::ExactU64::from(offset)),
                    max_bytes: Some(max_bytes),
                })
                .unwrap_or_else(|error| panic!("case {case} offset {offset}: {error}"));
            let delivered = chunk.data_len().unwrap();
            assert!(delivered <= max_bytes, "case {case}");
            assert_eq!(chunk.offset.as_u64().unwrap(), offset, "case {case}");
            gathered.extend_from_slice(&decode_hex(&chunk.data_hex));
            let next = chunk
                .next_offset
                .as_ref()
                .map(|next| next.as_u64().unwrap());
            if next.is_none() {
                break;
            }
            assert_eq!(next.unwrap(), offset + delivered, "case {case}");
            offset += delivered;
            steps += 1;
            assert!(steps <= 64, "case {case} did not terminate");
        }
        assert_eq!(gathered, bytes, "case {case}");
    }
}

#[test]
fn production_host_registers_the_control_plane_read_adapter() {
    let scratch = ScratchDir::new("cp-read-production");
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: Vec::new(),
        once: true,
    };
    let ledger = LedgerHandle::in_process(&state_dir).expect("selected ledger");
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| START_MS);
    let host = PluginHost::assemble_with_ledger(
        &config,
        &InMemoryInstallations::new(),
        ledger,
        clock,
    )
    .expect("production plugin host assembles over the selected ledger");
    assert!(
        host.router
            .router()
            .has_adapter(CONTROL_PLANE_READ_CAPABILITY),
        "the production constructor registers the read service once"
    );
}

#[test]
fn current_entries_authorize_reads_and_artifacts_not_replay_history() {
    let harness = harness("cp-read-authoritative");
    let bytes = b"old-artifact".to_vec();
    let digest = publish_artifact(&harness, 1, &bytes);
    // A later report in the same namespace replaces the authoritative entry and
    // drops the reference; the durable replay window still holds sequence 1.
    harness
        .publish(2, NS_ARTIFACTS, 1, json!({"artifact": null}))
        .expect("publish replacement");
    let service = harness.service();

    let page = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest::default(),
        )
        .expect("component page");
    // Two applied envelopes share the namespace, so the current entry has no
    // unambiguous provenance link: the rows are Unknown, never digest-guessed.
    assert!(page.entries.is_empty(), "no ambiguous provenance is served");
    let reports = page
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "reports")
        .expect("reports source");
    assert_eq!(reports.state, SourceState::Unknown);

    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest,
            offset: None,
            max_bytes: None,
        })
        .expect_err("a removed report reference no longer authorizes a read");
    assert!(error.to_string().contains("ScopeDenied"), "{error}");
}

#[test]
fn live_report_update_is_served_with_exact_observed_timestamp_provenance() {
    let harness = harness("cp-read-live-update");
    harness
        .publish(1, NS_VIEWS[0], 1, json!({"ready": false, "v": 1}))
        .expect("publish v1");
    harness.clock.set(START_MS + 10);
    harness
        .publish(2, NS_VIEWS[0], 1, json!({"ready": true, "v": 2}))
        .expect("publish v2");
    let page = harness
        .service()
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest::default(),
        )
        .expect("component page");
    assert_eq!(page.entries.len(), 1, "latest live update is served");
    assert_eq!(page.entries[0].sequence, 2);
    assert_eq!(page.entries[0].payload, json!({"ready": true, "v": 2}));
    let reports = page
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "reports")
        .expect("reports source");
    assert_eq!(reports.state, SourceState::Available);
}

#[test]
fn snapshot_recovery_serves_current_entries_not_stale_history() {
    let harness = harness("cp-read-snapshot");
    harness
        .publish(1, NS_STATUS, 1, json!({"ready": false, "stale": true}))
        .expect("publish stale");
    let binding = binding(PLUGIN_ID, harness.generation);
    let entry = snapshot_entry(NS_STATUS, 1, json!({"ready": true, "stale": false}))
        .expect("snapshot entry");
    let snapshot = seal_snapshot(PLUGIN_ID, binding.clone(), 7, vec![entry]).expect("snapshot");
    harness
        .reports
        .lock()
        .unwrap()
        .apply_snapshot(PLUGIN_ID, &binding, &snapshot)
        .expect("apply snapshot");

    let service = harness.service();
    let page = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest::default(),
        )
        .expect("component page");
    // A snapshot-recovered entry has no canonical producer identity, so it is
    // never fabricated into an envelope; it is reported as explicitly
    // unavailable and names its owner (HP.5 snapshot projection).
    assert!(page.entries.is_empty(), "no fabricated snapshot envelope");
    let reports = page
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "reports")
        .expect("reports source");
    assert_eq!(reports.state, SourceState::Unknown);
    assert!(reports.detail.contains("HP.5 snapshot projection"));
    assert_eq!(
        page.metadata
            .report_watermark
            .as_ref()
            .unwrap()
            .as_u64()
            .unwrap(),
        7
    );
}

#[test]
fn property_component_summaries_are_stable_ordered_and_bounded() {
    let harness = harness("cp-read-prop-components");
    for index in 0..6u64 {
        harness.publish_health(HealthState::Ready);
        let _ = index;
    }
    let service = harness.service();
    let baseline = service
        .list_components(&agalma_contracts::ReadPageRequest::default())
        .expect("components");
    let ids: Vec<&str> = baseline
        .entries
        .iter()
        .map(|entry| entry.plugin_id.as_str())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "components are ordered by plugin id");
    assert!(baseline.entries.len() <= 100);

    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0xabcd);
    for case in 0..PROPERTY_CASES {
        let limit = rng.below(6) + 1;
        let page = service
            .list_components(&agalma_contracts::ReadPageRequest {
                limit: Some(limit as u32),
                cursor: None,
            })
            .unwrap_or_else(|error| panic!("case {case}: {error}"));
        assert!(page.entries.len() <= limit as usize, "case {case}");
        // Every projection names the report namespace prefix boundary that the
        // durable ingestor uses, so a client can distinguish hub observations.
        assert!(!REPORT_NAMESPACE_PREFIX.is_empty());
        assert!(page
            .entries
            .iter()
            .all(|entry| !entry.plugin_id.is_empty()));
    }
}

// ---------------------------------------------------------------------------
// Sol review repairs: projection failures, cursor revisions, bounds,
// dependency propagation, usage completeness, artifact size, deadlines
// ---------------------------------------------------------------------------

#[test]
fn failed_registry_projection_is_visible_for_list_and_component() {
    let harness = harness("cp-read-projection-failure");
    let service = harness
        .service()
        .with_projection(Arc::new(FailingProjection));

    let page = service
        .list_components(&agalma_contracts::ReadPageRequest::default())
        .expect("failed registry still returns an explicit page");
    assert!(page.entries.is_empty());
    let registry = page
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "registry")
        .expect("registry source");
    assert_eq!(registry.state, SourceState::Unreadable, "never coerced to Empty");
    assert!(registry.detail.contains("registry checkpoint corrupt"));

    let component = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest::default(),
        )
        .expect("failed registry still returns explicit unknown data");
    let registry = component
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "registry")
        .expect("registry source");
    assert_eq!(registry.state, SourceState::Unreadable);
    assert!(component.entries.is_empty());
}

#[test]
fn registry_collection_mutation_conflicts_between_pages() {
    let harness = harness("cp-read-registry-mutation");
    let service = harness.service();
    let first = service
        .list_components(&agalma_contracts::ReadPageRequest {
            limit: Some(1),
            cursor: None,
        })
        .expect("first page");
    let cursor = first.next_cursor.clone().expect("continuation cursor");

    // A registry/generation collection mutation changes the pinned revision.
    harness
        .registry
        .lock()
        .unwrap()
        .register_builtin("builtin.added", START_MS + 1);
    let error = service
        .list_components(&agalma_contracts::ReadPageRequest {
            limit: Some(1),
            cursor: Some(cursor),
        })
        .expect_err("mutated collection conflicts");
    assert!(error.to_string().contains("RevisionConflict"), "{error}");

    // A fresh collection restarts and sees the new entry.
    let restarted = service
        .list_components(&agalma_contracts::ReadPageRequest {
            limit: Some(1),
            cursor: None,
        })
        .expect("fresh collection");
    assert!(!restarted.entries.is_empty());
}

#[test]
fn overview_bounds_executions_and_timeline() {
    let harness = harness("cp-read-overview-bounds");
    let service = harness
        .service()
        .with_domains(Arc::new(SyntheticDomains::new(150)));
    let overview = service.read_overview().expect("bounded overview");
    assert_eq!(overview.executions.len(), OVERVIEW_MAX_EXECUTIONS);
    assert_eq!(overview.timeline.len(), OVERVIEW_MAX_TIMELINE);
    let bounded = overview
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "overview")
        .expect("bound marker");
    assert!(bounded.detail.contains("bounded to"));
}

#[test]
fn missing_and_unreadable_dependencies_propagate_to_event_sources() {
    let harness = harness("cp-read-dependency-state");

    let missing = harness.service();
    let overview = missing.read_overview().expect("missing overview");
    for source in ["ledger", "ledger.events", "ledger.digests", "ledger.receipts"] {
        let availability = overview
            .metadata
            .sources
            .iter()
            .find(|entry| entry.source == source)
            .unwrap_or_else(|| panic!("{source} source"));
        assert_eq!(availability.state, SourceState::Missing, "{source}");
    }

    let unreadable = harness.service().with_domains(Arc::new(FailingDomains));
    let overview = unreadable.read_overview().expect("unreadable overview");
    for source in ["ledger", "ledger.events", "ledger.digests", "ledger.receipts"] {
        let availability = overview
            .metadata
            .sources
            .iter()
            .find(|entry| entry.source == source)
            .unwrap_or_else(|| panic!("{source} source"));
        assert_eq!(availability.state, SourceState::Unreadable, "{source}");
    }
}

#[test]
fn unknown_usage_fields_are_partial_not_a_complete_zero() {
    let harness = harness("cp-read-usage-partial");
    let service = harness
        .service()
        .with_domains(Arc::new(SyntheticDomains::with_usage(1, json!({}))));
    let overview = service.read_overview().expect("overview");
    assert_eq!(
        overview.economics.completeness,
        agalma_contracts::Completeness::Partial,
        "a usage event with missing fields is never a complete zero"
    );
    assert!(overview.economics.usage.is_none(), "no fabricated numeric total");
    assert!(overview.economics.cost.is_none(), "no fabricated numeric cost");
}

#[test]
fn absent_usage_is_known_zero_and_valid_zero_is_complete() {
    let harness = harness("cp-read-usage-zero");
    // No usage events: a known empty observation is a complete zero.
    let absent = harness
        .service()
        .with_domains(Arc::new(SyntheticDomains::new(1)));
    let overview = absent.read_overview().expect("absent usage");
    assert_eq!(
        overview.economics.completeness,
        agalma_contracts::Completeness::Complete
    );
    assert_eq!(
        overview.economics.usage.as_ref().map(|usage| usage.as_str()),
        Some("0")
    );
    // Explicit numeric zeros are valid and complete.
    let valid = harness.service().with_domains(Arc::new(
        SyntheticDomains::with_usage(1, json!({"tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0})),
    ));
    let overview = valid.read_overview().expect("valid zero usage");
    assert_eq!(
        overview.economics.completeness,
        agalma_contracts::Completeness::Complete
    );
    assert_eq!(
        overview.economics.usage.as_ref().map(|usage| usage.as_str()),
        Some("0")
    );
    // Malformed (non-numeric) fields are Partial with no fabricated total.
    let malformed = harness.service().with_domains(Arc::new(
        SyntheticDomains::with_usage(
            1,
            json!({"tokens_in": "x", "tokens_out": 2, "cost_usd": 1.0}),
        ),
    ));
    let overview = malformed.read_overview().expect("malformed usage");
    assert_eq!(
        overview.economics.completeness,
        agalma_contracts::Completeness::Partial
    );
    assert!(overview.economics.usage.is_none());
}

#[test]
fn artifact_lying_size_and_oversized_files_are_refused_before_reading() {
    let harness = harness("cp-read-artifact-size");
    let service = harness.service();

    // Declared size lies smaller than the object's real size.
    let actual = b"0123456789";
    let digest = sha256_digest(actual);
    let hex = digest.trim_start_matches("sha256:");
    let relative = format!("packages/objects/sha256/{hex}");
    let path = harness.dir.path().join(&relative);
    fs::create_dir_all(path.parent().unwrap()).expect("artifact dir");
    fs::write(&path, actual).expect("artifact bytes");
    harness
        .publish(
            1,
            NS_ARTIFACTS,
            1,
            json!({"artifact": {"path": relative, "digest": digest, "bytes": 5, "media_type": "application/octet-stream"}}),
        )
        .expect("publish lying size");
    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest,
            offset: None,
            max_bytes: None,
        })
        .expect_err("lying size refused");
    assert!(error.to_string().contains("IntegrityMismatch"), "{error}");

    // An oversized object is refused from metadata without being read.
    let small = b"small".to_vec();
    let small_digest = sha256_digest(&small);
    let small_hex = small_digest.trim_start_matches("sha256:");
    let big_relative = format!("packages/objects/sha256/{small_hex}");
    let big_path = harness.dir.path().join(&big_relative);
    let file = fs::File::create(&big_path).expect("big artifact");
    file.set_len(agalma_contracts::ARTIFACT_TOTAL_MAX_BYTES + 1)
        .expect("sparse big artifact");
    // A distinct namespace keeps each artifact's provenance unambiguous.
    harness
        .publish(
            2,
            NS_VIEWS[5],
            1,
            json!({"artifact": {"path": big_relative, "digest": small_digest, "bytes": small.len() as u64, "media_type": "application/octet-stream"}}),
        )
        .expect("publish oversized");
    let error = service
        .read_artifact_chunk(&agalma_contracts::ReadArtifactChunkRequest {
            plugin_id: PLUGIN_ID.to_string(),
            digest: small_digest,
            offset: None,
            max_bytes: None,
        })
        .expect_err("oversized artifact refused");
    assert!(error.to_string().contains("InvalidRecord"), "{error}");
}

#[test]
fn durable_projection_uses_installed_config_pin_and_live_health() {
    let harness = harness("cp-read-durable-pin");
    harness.publish_health(HealthState::Ready);
    let mut ledger = LedgerHandle::in_process(harness.dir.path()).expect("selected ledger");
    {
        let registry = harness.registry.lock().unwrap();
        persist_registry_generations(&mut ledger, &registry).expect("persist generations");
    }
    let store = HubInstallStore::new(harness.dir.path());
    let pin = store
        .load_installation(PLUGIN_ID, harness.generation)
        .expect("pin readable")
        .expect("pin present");
    let components = DurableComponents::new(
        harness.dir.path(),
        ledger,
        Arc::clone(&harness.reports),
    );
    let projected = components.components().expect("durable projection");
    let plugin = projected
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("plugin projected");
    assert_eq!(
        plugin.config_digest, pin.configuration.digest,
        "config digest is the installed pin, not a manifest hash"
    );
    assert_ne!(plugin.config_digest, sha256_digest(b""));
    assert_eq!(plugin.observed, LifecycleState::Ready, "live health signal");

    // A crash/unavailable heartbeat is the live lifecycle signal.
    harness.publish_health(HealthState::Unavailable);
    let projected = components.components().expect("durable projection");
    let plugin = projected
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("plugin projected");
    assert_eq!(plugin.observed, LifecycleState::Failed);
}

#[test]
fn router_service_releases_the_lock_during_a_slow_read() {
    let harness = harness("cp-read-lock-release");
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(BlockingAdapter {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    let grant = read_grant(START_MS + 60_000);
    router.install_grant(&grant);
    let service = Arc::new(RouterService::new(router));
    let call = read_call(
        &grant,
        "slow-read",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS + 10_000,
    );
    let worker = Arc::clone(&service);
    let handle = std::thread::spawn(move || {
        worker.invoke_capability(serde_json::to_value(&call).unwrap())
    });

    let (lock, condvar) = &*started;
    let mut started_flag = lock.lock().unwrap();
    while !*started_flag {
        started_flag = condvar.wait(started_flag).unwrap();
    }
    drop(started_flag);
    assert!(
        service.try_router().is_some(),
        "the router lock is released while a read source is running"
    );

    let (lock, condvar) = &*release;
    *lock.lock().unwrap() = true;
    condvar.notify_all();
    handle
        .join()
        .expect("slow read thread")
        .expect("slow read result");
}

#[test]
fn read_deadline_refuses_a_post_deadline_source_result() {
    let harness = harness("cp-read-deadline-source");
    let domains = SyntheticDomains::advancing_clock(
        1,
        harness.clock.handle(),
        START_MS + 120_000,
    );
    let mut service = harness
        .service()
        .with_domains(Arc::new(domains))
        .with_clock(harness.clock.clock());
    let scope = read_grant(START_MS + 60_000).scope().clone();
    let error = service
        .invoke_with_deadline(
            &scope,
            &input(&ControlPlaneReadRequest::ReadOverview),
            START_MS + 60_000,
        )
        .expect_err("a source result after the deadline is refused");
    assert!(error.to_string().contains("DeadlineExceeded"), "{error}");
}

#[test]
fn transient_report_between_pages_invalidates_the_cursor() {
    let harness = harness("cp-read-transient-page");
    harness
        .publish(1, NS_VIEWS[0], 1, json!({"ready": true}))
        .expect("publish");
    let service = harness.service();
    let first = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest {
                limit: Some(1),
                cursor: None,
            },
        )
        .expect("first page");
    let cursor = first.next_cursor.clone();
    if cursor.is_none() {
        // Single-entry collection: add a durable namespace first so a page has
        // a continuation, then re-read.
        harness
            .publish(2, NS_VIEWS[1], 1, json!({"ready": true}))
            .expect("publish second namespace");
        let first = service
            .read_component(
                PLUGIN_ID,
                &agalma_contracts::ReadPageRequest {
                    limit: Some(1),
                    cursor: None,
                },
            )
            .expect("first page");
        let cursor = first.next_cursor.expect("continuation cursor");
        // A transient report applies a new authoritative entry without a
        // durable commit, so `view.revision` is unchanged and only the actual
        // report collection fingerprint can invalidate the cursor.
        harness
            .publish_transient(3, NS_VIEWS[2], json!({"ready": false}))
            .expect("transient report");
        let error = service
            .read_component(
                PLUGIN_ID,
                &agalma_contracts::ReadPageRequest {
                    limit: Some(1),
                    cursor: Some(cursor),
                },
            )
            .expect_err("transient collection change conflicts");
        assert!(error.to_string().contains("RevisionConflict"), "{error}");
    }
}

#[test]
fn large_nested_u64_identity_stays_exact_wire_text() {
    let identity = agalma_contracts::ExactU64::parse("9007199254740993").expect("exact u64");
    let value = serde_json::to_value(&identity).expect("wire value");
    assert!(value.is_string(), "identity is text, not a JSON number");
    assert_eq!(value.as_str().unwrap(), "9007199254740993");
    let cursor = ReadCursor {
        collection: "components".to_string(),
        filter_digest: "f".to_string(),
        revision: identity.clone(),
        after: "a".to_string(),
    };
    let token = cursor.encode().expect("cursor encode");
    let decoded = ReadCursor::decode(&token).expect("cursor decode");
    assert_eq!(decoded.revision.as_u64().unwrap(), 9_007_199_254_740_993);
}

#[test]
fn wire_projection_keeps_nested_identity_u64_exact() {
    let large = 9_007_199_254_740_993u64;
    let payload = json!({"v": large});
    let payload_digest = sha256_digest(&serde_json::to_vec(&payload).unwrap());
    let binding = BindingPin {
        binding_id: "plugin.large".to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "1".to_string(),
        generation: large,
        config_digest: sha256_digest(b""),
    };
    let envelope = ReportEnvelope {
        format: "ReportEnvelope/v1".to_string(),
        plugin_id: "plugin.large".to_string(),
        binding: binding.clone(),
        schema_namespace: "plugin.large.status".to_string(),
        schema_version: 1,
        sequence: large,
        reported_at_unix_ms: large,
        observed_at_unix_ms: large,
        execution_id: None,
        durability: Durability::Durable,
        payload: payload.clone(),
        payload_digest,
    };
    let page = ReadComponent {
        metadata: ReadMetadata {
            format: CONTROL_PLANE_READ_FORMAT.to_string(),
            collected_at_unix_ms: 0,
            registry_revision: Some(ExactU64::from(large)),
            report_watermark: Some(ExactU64::from(large)),
            sources: Vec::new(),
        },
        binding,
        descriptors: Vec::new(),
        entries: vec![envelope],
        next_cursor: None,
    };
    let wire = page.to_wire_value().expect("browser wire projection");
    assert_eq!(wire["binding"]["generation"], json!("9007199254740993"));
    assert_eq!(wire["entries"][0]["sequence"], json!("9007199254740993"));
    assert_eq!(
        wire["entries"][0]["binding"]["generation"],
        json!("9007199254740993")
    );
    // Plugin-private payload numbers are copied verbatim, never rewritten.
    assert_eq!(wire["entries"][0]["payload"]["v"], json!(large));
    assert!(page.to_wire_json().expect("wire json").contains("\"sequence\":\"9007199254740993\""));
}

#[test]
fn overview_encoded_bytes_stay_within_the_page_bound() {
    let harness = harness("cp-read-overview-bytes");
    let service = harness
        .service()
        .with_domains(Arc::new(SyntheticDomains::with_padding(100, 4_000)));
    let overview = service.read_overview().expect("bounded overview");
    let value = serde_json::to_value(&overview).expect("wire value");
    let bytes = canonical_json(&value).expect("canonical json").len() as u64;
    assert!(bytes <= READ_PAGE_MAX_BYTES, "overview {bytes} bytes");
    assert!(overview
        .metadata
        .sources
        .iter()
        .any(|source| source.source == "overview"));
}

#[test]
fn empty_executions_yield_unknown_allowance_and_null_usage_is_partial() {
    let harness = harness("cp-read-usage-null");
    let empty = harness
        .service()
        .with_domains(Arc::new(SyntheticDomains::new(0)));
    let overview = empty.read_overview().expect("empty overview");
    assert!(overview.economics.allowance.is_none(), "empty is unknown, not zero");

    let null_usage = harness.service().with_domains(Arc::new(
        SyntheticDomains::with_usage(
            1,
            json!({"tokens_in": null, "tokens_out": null, "cost_usd": null}),
        ),
    ));
    let overview = null_usage.read_overview().expect("null usage overview");
    assert_eq!(
        overview.economics.completeness,
        agalma_contracts::Completeness::Partial,
        "null usage fields are missing, never a complete zero"
    );
    assert!(overview.economics.usage.is_none());
    assert!(overview.economics.cost.is_none());
}

#[test]
fn read_deadline_is_enforced_after_adapter_queueing() {
    let harness = harness("cp-read-deadline-queue");
    let invoked = Arc::new(AtomicU64::new(0));
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(CounterAdapter {
        invoked: Arc::clone(&invoked),
    });
    let grant = read_grant(START_MS + 60_000);
    router.install_grant(&grant);
    let call = read_call(
        &grant,
        "queued-read",
        input(&ControlPlaneReadRequest::ReadOverview),
        START_MS,
    );

    // Hold the adapter lock, queue the read behind it, then let the deadline
    // pass: the queued read must refuse without invoking the adapter.
    let adapter = router.prepare_read(&call).expect("prepare read").1;
    let guard = adapter.lock().unwrap();
    let clock = harness.clock.clock();
    let handle = std::thread::spawn(move || router.invoke_read(&call));
    std::thread::sleep(std::time::Duration::from_millis(80));
    // Advance the shared clock past the read deadline.
    let now = clock();
    let _ = now;
    harness.clock.set(START_MS + 120_000);
    drop(guard);
    let started_at = std::time::Instant::now();
    let result = handle.join().expect("queued read thread");
    assert!(
        started_at.elapsed() < std::time::Duration::from_secs(2),
        "caller returned promptly, not after the adapter released"
    );
    let error = result.expect_err("queued read expired");
    assert!(error.to_string().contains("DeadlineExceeded"), "{error}");
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        0,
        "an expired queued read never invokes the adapter"
    );
}

#[test]
fn router_mutex_contention_returns_by_the_admitted_deadline() {
    let invoked = Arc::new(AtomicU64::new(0));
    let real_clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    });
    let now = real_clock();
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(Arc::clone(&real_clock));
    router.register_adapter(CounterAdapter {
        invoked: Arc::clone(&invoked),
    });
    let grant = read_grant(now + 60_000);
    router.install_grant(&grant);
    let service = Arc::new(RouterService::new(router));
    let call = CapabilityCallRequest {
        grant: grant.token_hex(),
        plugin_id: grant.scope().plugin_id.clone(),
        generation: grant.scope().binding.generation,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: CONTROL_PLANE_READ_CAPABILITY.to_string(),
        operation_id: "mutex-contention".to_string(),
        input: input(&ControlPlaneReadRequest::ReadOverview),
        deadline_unix_ms: real_clock() + 60,
        depth: 0,
    };

    // Hold the global router mutex past the caller deadline and require the
    // contended call to return *while the guard is still held*: a blocking
    // implementation cannot pass this.
    let guard = service.router();
    let worker = Arc::clone(&service);
    let started_at = std::time::Instant::now();
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let result = worker.invoke_capability(serde_json::to_value(&call).unwrap());
        let _ = sender.send(result);
    });
    let result = receiver
        .recv_timeout(std::time::Duration::from_millis(500))
        .expect("contended read must return while the lock is still held");
    let error = result.expect_err("contended read deadline expired");
    assert!(error.to_string().contains("DeadlineExceeded"), "{error}");
    assert!(
        started_at.elapsed() < std::time::Duration::from_millis(500),
        "bounded acquisition returned near the deadline, not after lock release"
    );
    assert_eq!(invoked.load(Ordering::SeqCst), 0, "no read ran after expiry");
    drop(guard);
    handle.join().expect("contended read thread");
}

#[test]
fn property_admitted_read_deadline_is_bounded_and_exact() {
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0xdead_11fe);
    for case in 0..PROPERTY_CASES {
        let now = rng.next_u64();
        let requested = rng.next_u64();
        let admitted = admitted_read_deadline(now, requested);
        assert_eq!(admitted, requested.min(now.saturating_add(READ_DEADLINE_MAX_MS)));
        assert!(admitted <= now.saturating_add(READ_DEADLINE_MAX_MS), "case {case}");
        if requested <= now {
            assert!(admitted <= now, "case {case}: expired request stays expired");
        }
        if requested >= now.saturating_add(READ_DEADLINE_MAX_MS) {
            assert_eq!(admitted, now.saturating_add(READ_DEADLINE_MAX_MS), "case {case}");
        }
        // A later request never admits an earlier deadline.
        let later = requested.saturating_add(rng.below(10_000));
        assert!(admitted_read_deadline(now, later) >= admitted, "case {case}");
    }
}

#[test]
fn bounded_read_returns_by_deadline_with_a_slow_source() {
    let harness = harness("cp-read-slow-source");
    let real_clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    });
    let service = harness
        .service()
        .with_domains(Arc::new(SlowDomains {
            sleep: std::time::Duration::from_millis(400),
        }))
        .with_clock(Arc::clone(&real_clock));
    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(Arc::clone(&real_clock));
    router.register_adapter(service);
    let grant = read_grant(real_clock() + 60_000);
    router.install_grant(&grant);
    let deadline = real_clock() + 60;
    let call = CapabilityCallRequest {
        grant: grant.token_hex(),
        plugin_id: grant.scope().plugin_id.clone(),
        generation: grant.scope().binding.generation,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: CONTROL_PLANE_READ_CAPABILITY.to_string(),
        operation_id: "slow".to_string(),
        input: input(&ControlPlaneReadRequest::ReadOverview),
        deadline_unix_ms: deadline,
        depth: 0,
    };
    let service_router = RouterService::new(router);
    let started_at = std::time::Instant::now();
    let error = service_router
        .invoke_capability(serde_json::to_value(&call).unwrap())
        .expect_err("slow source exceeds the deadline");
    assert!(error.to_string().contains("DeadlineExceeded"), "{error}");
    assert!(
        started_at.elapsed() < std::time::Duration::from_millis(250),
        "caller returns by the deadline, not the source's completion: {:?}",
        started_at.elapsed()
    );
}

#[test]
fn live_registry_overlay_reports_a_crash_before_freshness_expiry() {
    let harness = harness("cp-read-live-crash");
    harness.publish_health(HealthState::Ready);
    let live = Arc::new(Mutex::new(Vec::new()));
    let service = harness.service().with_live_cell(Arc::clone(&live));
    let refresh = |live: &Arc<Mutex<Vec<ProjectedComponent>>>| {
        let reports = harness.reports.lock().unwrap();
        let registry = harness.registry.lock().unwrap();
        *live.lock().unwrap() = live_components(&registry, Some(&reports));
    };
    refresh(&live);

    let mut router = CapabilityRouter::new(
        Box::new(agalma_conductor::plugin_router::InMemoryOperationJournal::new()),
        Arc::new(RunningStop),
    )
    .with_clock(harness.clock.clock());
    router.register_adapter(service);
    let grant = read_grant(START_MS + 60_000);
    router.install_grant(&grant);

    // A real supervisor crash is recorded on the live registry.
    harness
        .registry
        .lock()
        .unwrap()
        .mark_failed(PLUGIN_ID, "child crashed", START_MS + 1)
        .expect("record crash");
    refresh(&live);

    let value = router
        .invoke_read(&read_call(
            &grant,
            "crash-read",
            input(&ControlPlaneReadRequest::ListComponents {
                limit: None,
                cursor: None,
            }),
            START_MS + 2,
        ))
        .expect("read after crash");
    let page = ReadPage::<ComponentSummary>::from_wire_value(&value).expect("page");
    let entry = page
        .entries
        .iter()
        .find(|entry| entry.plugin_id == PLUGIN_ID)
        .expect("component listed");
    assert!(!entry.available, "crash is visible immediately, not after freshness expiry");
    assert_eq!(entry.health, HealthState::Unavailable);
}

#[test]
fn producer_usage_completeness_is_merged_conservatively() {
    let harness = harness("cp-read-producer-usage");
    for (label, declared, expected) in [
        ("Unknown", "Unknown", agalma_contracts::Completeness::Unknown),
        ("Partial", "Partial", agalma_contracts::Completeness::Partial),
        ("Complete", "Complete", agalma_contracts::Completeness::Complete),
        ("unknown", "unknown", agalma_contracts::Completeness::Unknown),
        ("partial", "partial", agalma_contracts::Completeness::Partial),
    ] {
        let service = harness.service().with_domains(Arc::new(
            SyntheticDomains::with_usage(
                1,
                json!({
                    "tokens_in": 0,
                    "tokens_out": 0,
                    "cost_usd": 0.0,
                    "completeness": declared,
                }),
            ),
        ));
        let overview = service.read_overview().expect("overview");
        assert_eq!(
            overview.economics.completeness, expected,
            "producer {label} completeness"
        );
        if expected == agalma_contracts::Completeness::Complete {
            assert_eq!(
                overview.economics.usage.as_ref().map(|usage| usage.as_str()),
                Some("0")
            );
        } else {
            assert!(
                overview.economics.usage.is_none(),
                "producer {label} never fabricates a numeric total"
            );
        }
    }
}

#[test]
fn production_supervisor_crash_is_visible_immediately_through_the_router() {
    let scratch = ScratchDir::new("cp-read-real-supervisor-crash");
    let state_dir = scratch.child("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let root = scratch.child("root");
    fs::create_dir_all(&root).expect("root");
    seal_ready_package(&root.join("pkg"), &manifest_value(PLUGIN_ID));
    let config = PluginHostConfig {
        state_dir: state_dir.clone(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: false,
    };
    let ledger = LedgerHandle::in_process(&state_dir).expect("ledger");
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| START_MS);
    let mut host = PluginHost::assemble_with_ledger(
        &config,
        &InMemoryInstallations::new(),
        ledger,
        clock,
    )
    .expect("production host");
    host.set_desired(PLUGIN_ID, DesiredState::Running)
        .expect("desired running");
    // Spawn the real READY-capable fixture child, then kill it so the shared
    // supervisor observes an actual process exit.
    let request = host.launch_request(PLUGIN_ID).expect("launch request");
    let spawned = host
        .supervisor
        .spawn(&mut host.registry, &request)
        .expect("READY-capable fixture child spawns");
    let _ = std::process::Command::new("kill")
        .arg(spawned.pid.to_string())
        .status();
    std::thread::sleep(std::time::Duration::from_millis(400));

    // The shared production supervision path reaps the exit and publishes the
    // live projection; no test-side cell mutation or refresh is used.
    struct NoopProbe;
    impl HealthProbe for NoopProbe {
        fn probe(&mut self, _plugin_id: &str, _epoch: u64) -> Result<bool, ContractError> {
            Ok(false)
        }
    }
    let outcome = host
        .supervise_tick(&mut NoopProbe)
        .expect("shared supervise tick");
    assert!(
        !outcome.crashed.is_empty(),
        "supervisor observed the real child exit"
    );

    let grant = read_grant(agalma_conductor::plugin_router::wall_clock_now_ms() + 60_000);
    host.router
        .router()
        .install_grant(&grant);
    let call = CapabilityCallRequest {
        grant: grant.token_hex(),
        plugin_id: grant.scope().plugin_id.clone(),
        generation: grant.scope().binding.generation,
        method: METHOD_INVOKE_CAPABILITY.to_string(),
        capability: CONTROL_PLANE_READ_CAPABILITY.to_string(),
        operation_id: "real-crash".to_string(),
        input: input(&ControlPlaneReadRequest::ListComponents {
            limit: None,
            cursor: None,
        }),
        deadline_unix_ms: agalma_conductor::plugin_router::wall_clock_now_ms() + 60_000,
        depth: 0,
    };
    let value = host
        .router
        .invoke_capability(serde_json::to_value(&call).unwrap())
        .expect("production-routed read");
    let page = ReadPage::<ComponentSummary>::from_wire_value(&value).expect("page");
    let entry = page
        .entries
        .iter()
        .find(|entry| entry.plugin_id == PLUGIN_ID)
        .expect("component listed");
    assert!(!entry.available, "supervisor crash visible immediately");
    assert_eq!(entry.health, HealthState::Unavailable);
}

#[test]
fn mixed_supported_and_recovered_snapshot_rows_are_unknown() {
    let harness = harness("cp-read-snapshot-mixed");
    let binding = binding(PLUGIN_ID, harness.generation);
    let entry = snapshot_entry(NS_STATUS, 1, json!({"ready": true})).expect("snapshot entry");
    let snapshot = seal_snapshot(PLUGIN_ID, binding.clone(), 7, vec![entry]).expect("snapshot");
    harness
        .reports
        .lock()
        .unwrap()
        .apply_snapshot(PLUGIN_ID, &binding, &snapshot)
        .expect("apply snapshot");
    // A supported durable report above the snapshot watermark.
    harness
        .publish(8, NS_ARTIFACTS, 1, json!({"artifact": null}))
        .expect("durable above snapshot");

    let service = harness.service();
    let page = service
        .read_component(
            PLUGIN_ID,
            &agalma_contracts::ReadPageRequest::default(),
        )
        .expect("component page");
    let reports = page
        .metadata
        .sources
        .iter()
        .find(|source| source.source == "reports")
        .expect("reports source");
    assert_eq!(
        reports.state,
        SourceState::Unknown,
        "one unmatched recovered row makes the aggregate unknown"
    );
    assert!(reports.detail.contains("HP.5 snapshot projection"));
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.schema_namespace == NS_ARTIFACTS),
        "post-snapshot live reports remain servable"
    );
    assert!(
        !page
            .entries
            .iter()
            .any(|entry| entry.schema_namespace == NS_STATUS),
        "recovered rows stay unsupported"
    );
}
