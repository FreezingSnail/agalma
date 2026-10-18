//! HP.6 registry-driven component navigation and declarative plugin data views
//! (`agalma-jmr.6`).
//!
//! Every case renders off-screen through the production read model: it installs
//! a sealed, unfamiliar **test fixture** package into a real
//! [`PluginRegistry`] and drives a real [`ReportIngestor`] over a real
//! `SqliteLedger` component store, then renders
//! [`crate::status::Snapshot`]-derived frames through [`watch::render_to_string`]
//! (`ratatui::TestBackend`). No plugin-specific hub UI code is exercised: the
//! fixtures only supply manifest/view declarations, and the hub renders them
//! through the frozen generic building blocks.
//!
//! Coverage:
//!
//! * an unfamiliar plugin shows its Status/Metrics/Table/Timeline/Alert/
//!   ArtifactLink views and its undeclared report namespaces render generically;
//! * component navigation is bounded (markers plus a scroll offset);
//! * a stale heartbeat, a disconnected lifecycle and an unsupported view version
//!   are all visible;
//! * viewing and refresh are strictly read-only;
//! * control characters are escaped and rows/text are bounded;
//! * generated property cases assert the rendering invariants with a recorded
//!   deterministic seed and case budget.
//!
//! Fixtures live under the persistent scratch root (`target/test-runs`), never
//! `/tmp`. No network, no paid provider call.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use agalma_conductor::components::{
    derive_plugin_components, escape_control, render_blocks, ComponentView, HealthView,
    PluginComponent, Transports, ViewBlock, MAX_VIEW_BLOCK_ROWS, MAX_VIEW_TEXT_CHARS,
};
use agalma_conductor::composition::{InMemoryInstallations, PluginHost};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::economics::EconomicsSnapshot;
use agalma_conductor::ledger_process::{LedgerHandle, LedgerTransport};
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveredPackage, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_registry::{
    DesiredState, HubInstallStore, LedgerDesiredStateStore, PluginRegistry,
};
use agalma_conductor::plugin_reports::{seal_publication, ReportIngestor, REGISTRY_NAMESPACE};
use agalma_conductor::status::{snapshot_with_selected_transport, Snapshot};
use agalma_conductor::watch::{once_frame, render_to_string, App};
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::{
    BindingPin, ComponentCommit, ContractError, Durability, HealthState, LedgerApi, PluginHealth,
    PluginInstallation, PluginManifest, ViewKind, COMPONENT_COMMIT_FORMAT,
};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

/// Recorded deterministic property-test seed (xorshift64).
const PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_006a;
/// Recorded property-test case budget.
const PROPERTY_CASES: u64 = 40;
/// Start of the injected hub clock (milliseconds).
const START_MS: u64 = 1_791_820_800_000;

const PLUGIN_ID: &str = "example.unfamiliar";
const NS_STATUS: &str = "example.unfamiliar.status";
const NS_METRICS: &str = "example.unfamiliar.metrics";
const NS_TABLE: &str = "example.unfamiliar.tasks";
const NS_TIMELINE: &str = "example.unfamiliar.timeline";
const NS_ALERT: &str = "example.unfamiliar.alerts";
const NS_ARTIFACT: &str = "example.unfamiliar.artifacts";
/// Declared at schema v2, but reports arrive at v1: a visible unsupported view.
const NS_PENDING: &str = "example.unfamiliar.pending";
/// No declared view: rendered generically.
const NS_EXTRA: &str = "example.unfamiliar.extra";

const CONFIG_SCHEMA_BYTES: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
const DEFAULT_CONFIG_BYTES: &[u8] = b"{}\n";
const REPORT_SCHEMA_BYTES: &[u8] = b"{\"type\":\"object\"}\n";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A `Send + Sync` injected clock shared by the ingestor and the tests.
struct ManualClock {
    now: Arc<AtomicU64>,
}

impl ManualClock {
    fn new(start: u64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(start)),
        }
    }

    fn advance(&self, by: u64) {
        self.now.fetch_add(by, Ordering::SeqCst);
    }

    fn clock(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let now = Arc::clone(&self.now);
        Arc::new(move || now.load(Ordering::SeqCst))
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

/// Every namespace/version the manifest declares as a report schema (the
/// ingestor's allowlist).
fn allowed_schemas() -> BTreeSet<(String, u64)> {
    [
        NS_STATUS,
        NS_METRICS,
        NS_TABLE,
        NS_TIMELINE,
        NS_ALERT,
        NS_ARTIFACT,
        NS_PENDING,
        NS_EXTRA,
    ]
    .into_iter()
    .map(|namespace| (namespace.to_string(), 1u64))
    .collect()
}

/// A `PluginManifest/v1` declaring one view per building block plus an
/// unsupported (v2) view and an undeclared namespace.
fn manifest_value(plugin_id: &str) -> Value {
    let report_schema = |path: &str| byte_ref(path, REPORT_SCHEMA_BYTES, "application/json");
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
        "report_schemas": [
            {"namespace": NS_STATUS, "version": 1, "schema": report_schema("objects/report-schema.json")},
            {"namespace": NS_METRICS, "version": 1, "schema": report_schema("objects/report-schema.json")},
            {"namespace": NS_TABLE, "version": 1, "schema": report_schema("objects/report-schema.json")},
            {"namespace": NS_TIMELINE, "version": 1, "schema": report_schema("objects/report-schema.json")},
            {"namespace": NS_ALERT, "version": 1, "schema": report_schema("objects/report-schema.json")},
            {"namespace": NS_ARTIFACT, "version": 1, "schema": report_schema("objects/report-schema.json")},
            {"namespace": NS_PENDING, "version": 1, "schema": report_schema("objects/report-schema.json")},
            {"namespace": NS_EXTRA, "version": 1, "schema": report_schema("objects/report-schema.json")}
        ],
        "views": [
            {"view_id": "status", "schema_namespace": NS_STATUS, "kind": "Status", "title": "Reporter status", "schema_version": 1, "fields": ["ready"]},
            {"view_id": "metrics", "schema_namespace": NS_METRICS, "kind": "Metrics", "title": "Queue metrics", "schema_version": 1, "fields": ["queued", "in_flight", "failed"]},
            {"view_id": "tasks", "schema_namespace": NS_TABLE, "kind": "Table", "title": "Task table", "schema_version": 1, "fields": ["task", "state", "attempt"]},
            {"view_id": "timeline", "schema_namespace": NS_TIMELINE, "kind": "Timeline", "title": "Reporter events", "schema_version": 1, "fields": ["time", "event"]},
            {"view_id": "alerts", "schema_namespace": NS_ALERT, "kind": "Alert", "title": "Reporter alerts", "schema_version": 1, "fields": ["level", "message"]},
            {"view_id": "artifacts", "schema_namespace": NS_ARTIFACT, "kind": "ArtifactLink", "title": "Latest artifact", "schema_version": 1, "fields": ["path"]},
            {"view_id": "pending", "schema_namespace": NS_PENDING, "kind": "Status", "title": "Pending schema", "schema_version": 2, "fields": ["value"]}
        ],
        "actions": [
            {
                "action_id": "reporter.refresh",
                "input_schema": "reporter.refresh/v1",
                "required_capability": "report.publish",
                "effectful": true,
                "human_confirmation": null,
                "default_deadline_ms": 1000
            }
        ],
        "state_compatibility": {"reads": ["reporter-state/v1"], "writes": "reporter-state/v1"}
    })
}

/// Write the package files and seal the manifest with its inventory digest.
fn seal_package(package_dir: &Path, manifest: &Value) -> PluginManifest {
    let mut files = support_files();
    files.push((
        "bin/plugin".to_string(),
        b"#!/bin/sh\ncat >/dev/null\n".to_vec(),
    ));
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
    installation.installation_digest = installation
        .recomputed_installation_digest()
        .expect("installation digest");
    installation
}

/// A live registry + report ingestor over real sealed-package and ledger
/// fixtures.
struct Harness {
    dir: ScratchDir,
    registry: PluginRegistry,
    ingestor: ReportIngestor,
    clock: ManualClock,
    generation: u64,
}

fn harness(name: &str, builtins: usize) -> Harness {
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
    for index in 0..builtins {
        registry.register_builtin(&format!("builtin.{index:02}"), START_MS);
    }
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
        registry,
        ingestor,
        clock,
        generation,
    }
}

impl Harness {
    fn publish(
        &mut self,
        sequence: u64,
        namespace: &str,
        version: u64,
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
            Durability::Durable,
            payload,
        )?;
        self.ingestor.publish(PLUGIN_ID, &pin, &publication)?;
        Ok(())
    }

    fn publish_health(&mut self) {
        let pin = binding(PLUGIN_ID, self.generation);
        let health = PluginHealth {
            format: "PluginHealth/v1".to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding: pin.clone(),
            state: HealthState::Ready,
            detail: "serving".to_string(),
            checked_at_unix_ms: START_MS,
        };
        self.ingestor
            .record_health(PLUGIN_ID, &pin, &health)
            .expect("record health");
    }

    /// The standard fixture: one report per declared namespace, health, and the
    /// undeclared generic namespace.
    fn publish_all(&mut self) {
        self.publish_health();
        self.publish(1, NS_STATUS, 1, json!({"ready": true}))
            .expect("status");
        self.publish(
            2,
            NS_METRICS,
            1,
            json!({"queued": 3, "in_flight": 1, "failed": 0}),
        )
        .expect("metrics");
        self.publish(
            3,
            NS_TABLE,
            1,
            json!([
                {"task": "t1", "state": "running", "attempt": 1},
                {"task": "t2", "state": "queued", "attempt": 0},
                {"task": "t3", "state": "done", "attempt": 2}
            ]),
        )
        .expect("table");
        self.publish(
            4,
            NS_TIMELINE,
            1,
            json!([
                {"time": "10:00", "event": "started"},
                {"time": "10:05", "event": "reported"}
            ]),
        )
        .expect("timeline");
        self.publish(
            5,
            NS_ALERT,
            1,
            json!({"level": "warning", "message": "queue backing up"}),
        )
        .expect("alert");
        self.publish(6, NS_ARTIFACT, 1, json!({"path": "/artifacts/report.json"}))
            .expect("artifact");
        self.publish(7, NS_PENDING, 1, json!({"value": "v1"}))
            .expect("pending");
        self.publish(8, NS_EXTRA, 1, json!({"mystery": "opaque", "count": 2}))
            .expect("extra");
    }

    fn components(&self) -> Vec<PluginComponent> {
        derive_plugin_components(&self.registry, Some(&self.ingestor))
    }

    fn component(&self) -> PluginComponent {
        self.components()
            .into_iter()
            .find(|component| component.plugin_id == PLUGIN_ID)
            .expect("unfamiliar plugin component")
    }

    fn view(&self, view_id: &str) -> ComponentView {
        self.component()
            .views
            .into_iter()
            .find(|view| view.view_id == view_id)
            .unwrap_or_else(|| panic!("view {view_id} missing"))
    }

    fn snapshot(&self) -> Snapshot {
        render_snapshot(self.dir.path(), self.components())
    }
}

/// Current wall-clock milliseconds (the durable status path computes freshness
/// against real time).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// A read-model document carrying a pre-derived plugin panel (used by the
/// render-level fixtures; the production path builds this through
/// [`snapshot_with_selected_transport`]).
fn render_snapshot(state_dir: &Path, plugins: Vec<PluginComponent>) -> Snapshot {
    Snapshot {
        state_dir: state_dir.display().to_string(),
        exists: false,
        kill_latch: false,
        schema_version: None,
        binding: None,
        executions: Vec::new(),
        components: Vec::new(),
        plugins,
        economics: EconomicsSnapshot::default(),
    }
}

/// A state dir whose registry/report state is durable in the **selected ledger**
/// (`<state_dir>/ledger.sqlite`), so the production `status`/`watch` read path
/// can derive the plugin panel without any injected snapshot.
struct DurableFixture {
    dir: ScratchDir,
    generation: u64,
}

/// Build the durable fixture. `age_ms` is how far in the past the injected hub
/// observed the reports (the production `status` read computes freshness against
/// real time with the contract's 15 s stale / 30 s unavailable thresholds).
fn durable_fixture(name: &str, age_ms: u64) -> DurableFixture {
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

    // Install the hub-owned immutable generation (writes
    // `<state_dir>/plugin-installed/...`).
    let store = HubInstallStore::new(dir.path());
    let mut registry = PluginRegistry::new();
    let generation = registry
        .install(&store, &package, &installation_for(&package))
        .expect("install");

    // Durable registry checkpoint + report observations, both through the
    // selected ledger handle (no direct SQLite helper).
    let observed = now_ms().saturating_sub(age_ms);
    let handle = LedgerHandle::in_process(dir.path()).expect("selected ledger");
    let mut desired = LedgerDesiredStateStore::open(handle.clone()).expect("desired store");
    desired
        .update(|checkpoint| {
            checkpoint
                .desired
                .insert(PLUGIN_ID.to_string(), DesiredState::Running);
            checkpoint
                .active_generations
                .insert(PLUGIN_ID.to_string(), generation);
            checkpoint
                .max_generations
                .insert(PLUGIN_ID.to_string(), generation);
        })
        .expect("write checkpoint");

    let clock = ManualClock::new(observed);
    let mut ingestor = ReportIngestor::open(
        Box::new(Mutex::new(handle.clone())),
        clock.clock(),
        BTreeMap::from([(PLUGIN_ID.to_string(), generation)]),
        allowed_schemas(),
    )
    .expect("open ingestor");
    publish_standard_reports(&mut ingestor, generation, observed);

    DurableFixture { dir, generation }
}

/// Publish the standard fixture reports through any report ingestor.
fn publish_standard_reports(ingestor: &mut ReportIngestor, generation: u64, observed_at: u64) {
    let pin = binding(PLUGIN_ID, generation);
    let health = PluginHealth {
        format: "PluginHealth/v1".to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        binding: pin.clone(),
        state: HealthState::Ready,
        detail: "serving".to_string(),
        checked_at_unix_ms: observed_at,
    };
    ingestor
        .record_health(PLUGIN_ID, &pin, &health)
        .expect("record health");
    let reports: [(u64, &str, Value); 8] = [
        (1, NS_STATUS, json!({"ready": true})),
        (
            2,
            NS_METRICS,
            json!({"queued": 3, "in_flight": 1, "failed": 0}),
        ),
        (
            3,
            NS_TABLE,
            json!([
                {"task": "t1", "state": "running", "attempt": 1},
                {"task": "t2", "state": "queued", "attempt": 0},
                {"task": "t3", "state": "done", "attempt": 2}
            ]),
        ),
        (
            4,
            NS_TIMELINE,
            json!([
                {"time": "10:00", "event": "started"},
                {"time": "10:05", "event": "reported"}
            ]),
        ),
        (
            5,
            NS_ALERT,
            json!({"level": "warning", "message": "queue backing up"}),
        ),
        (6, NS_ARTIFACT, json!({"path": "/artifacts/report.json"})),
        (7, NS_PENDING, json!({"value": "v1"})),
        (8, NS_EXTRA, json!({"mystery": "opaque", "count": 2})),
    ];
    for (sequence, namespace, payload) in reports {
        let publication = seal_publication(
            PLUGIN_ID,
            pin.clone(),
            namespace,
            1,
            sequence,
            observed_at,
            Durability::Durable,
            payload,
        )
        .expect("seal publication");
        ingestor
            .publish(PLUGIN_ID, &pin, &publication)
            .expect("publish");
    }
}

/// Read the read model through the production selected-ledger entry point.
fn durable_snapshot(fixture: &DurableFixture, transport: LedgerTransport) -> Snapshot {
    snapshot_with_selected_transport(fixture.dir.path(), &Transports::all_in_process(), transport)
        .expect("status snapshot")
}

fn frame(snapshot: Snapshot, width: u16, height: u16) -> String {
    let app = App::new(snapshot, Transports::all_in_process(), 1000);
    render_to_string(&app, width, height)
}

/// A content digest of the whole tree (relative path + file digest), used to
/// prove a read path wrote nothing.
fn tree_digest(root: &Path) -> String {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| entry.expect("entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("prefix")
                    .to_string_lossy()
                    .to_string();
                let bytes = fs::read(&path).expect("read file");
                out.push((relative, sha256_digest(&bytes)));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    format!("{out:?}")
}

// ---------------------------------------------------------------------------
// Acceptance cases
// ---------------------------------------------------------------------------

#[test]
fn unfamiliar_plugin_renders_all_declarative_building_blocks() {
    let mut h = harness("hp-ui-views", 0);
    h.publish_all();

    let component = h.component();
    assert_eq!(component.origin, "discovered");
    assert_eq!(component.lifecycle, "ready");
    assert_eq!(component.freshness, "fresh");
    assert_eq!(component.generation, Some(h.generation));
    assert_eq!(component.actions.len(), 1);
    assert_eq!(component.actions[0].action_id, "reporter.refresh");
    assert!(component.actions[0].effectful);

    // Metrics block carries the declared key/value rows.
    let metrics = h.view("metrics");
    assert_eq!(metrics.kind, "metrics");
    assert_eq!(metrics.state, "rendered");
    match &metrics.blocks[0] {
        ViewBlock::Metrics { rows } => {
            assert_eq!(rows[0].name, "queued");
            assert_eq!(rows[0].value, "3");
            assert_eq!(rows[2].value, "0");
        }
        other => panic!("expected metrics block, got {other:?}"),
    }

    // Table block carries headers and bounded rows.
    let table = h.view("tasks");
    assert_eq!(table.kind, "table");
    match &table.blocks[0] {
        ViewBlock::Table {
            headers,
            rows,
            truncated,
        } => {
            assert_eq!(
                headers,
                &vec![
                    "task".to_string(),
                    "state".to_string(),
                    "attempt".to_string()
                ]
            );
            assert_eq!(rows.len(), 3);
            assert_eq!(*truncated, 0);
        }
        other => panic!("expected table block, got {other:?}"),
    }

    // Alert block.
    let alert = h.view("alerts");
    match &alert.blocks[0] {
        ViewBlock::Alert { level, message } => {
            assert_eq!(level, "warning");
            assert_eq!(message, "queue backing up");
        }
        other => panic!("expected alert block, got {other:?}"),
    }

    // The undeclared namespace renders generic data, never a custom UI path.
    let extra = h.view(NS_EXTRA);
    assert_eq!(
        extra.note.as_deref(),
        Some("undeclared namespace; rendered generically")
    );
    assert_eq!(extra.kind, "metrics");

    // The off-screen frame shows the plugin and its data through the panes.
    let plain = frame(h.snapshot(), 160, 140);
    assert!(plain.contains(PLUGIN_ID), "{plain}");
    assert!(plain.contains("Queue metrics"), "{plain}");
    assert!(plain.contains("queued"), "{plain}");
    assert!(plain.contains("Task table"), "{plain}");
    assert!(plain.contains("t1"), "{plain}");
    assert!(plain.contains("Reporter alerts"), "{plain}");
    assert!(plain.contains("queue backing up"), "{plain}");
    assert!(plain.contains("Reporter events"), "{plain}");
    assert!(plain.contains("started"), "{plain}");
    assert!(plain.contains("reporter.refresh"), "{plain}");
}

// ---------------------------------------------------------------------------
// Assembled production path: the normal status/watch entry points must consume
// the selected ledger + durable registry/report read model, not an injected
// snapshot.
// ---------------------------------------------------------------------------

#[test]
fn status_entry_point_reads_registered_plugin_data_from_selected_ledger() {
    let fixture = durable_fixture("hp-ui-production-inprocess", 0);
    let snapshot = durable_snapshot(&fixture, LedgerTransport::InProcess);

    assert!(snapshot.exists, "durable ledger present");
    let component = snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .unwrap_or_else(|| panic!("plugin panel: {:?}", snapshot.plugins));
    assert_eq!(component.origin, "discovered");
    assert_eq!(component.generation, Some(fixture.generation));
    assert_eq!(component.freshness, "fresh", "{component:?}");
    assert!(!component.disconnected);
    let metrics = component
        .views
        .iter()
        .find(|view| view.view_id == "metrics")
        .expect("metrics view");
    assert_eq!(metrics.state, "rendered");
    let pending = component
        .views
        .iter()
        .find(|view| view.view_id == "pending")
        .expect("pending view");
    assert_eq!(pending.state, "unsupported");
    assert!(
        component.views.iter().any(|view| view.view_id == NS_EXTRA),
        "undeclared namespace renders generically"
    );

    // The off-screen frame is built from the production read model.
    let plain = frame(snapshot, 160, 140);
    assert!(plain.contains(PLUGIN_ID), "{plain}");
    assert!(
        plain.contains("Queue metrics") && plain.contains("queued"),
        "{plain}"
    );
    assert!(
        plain.contains("Task table") && plain.contains("t1"),
        "{plain}"
    );
    assert!(
        plain.contains("Reporter alerts") && plain.contains("queue backing up"),
        "{plain}"
    );
    assert!(plain.contains("[unsupported]"), "{plain}");
}

#[test]
fn watch_refresh_keeps_the_production_plugin_panel() {
    let fixture = durable_fixture("hp-ui-production-watch", 0);
    let mut app = App::new(
        durable_snapshot(&fixture, LedgerTransport::InProcess),
        Transports::all_in_process(),
        1000,
    );
    assert!(app
        .snapshot
        .plugins
        .iter()
        .any(|component| component.plugin_id == PLUGIN_ID));

    app.refresh(fixture.dir.path()).expect("refresh");
    assert!(
        app.snapshot
            .plugins
            .iter()
            .any(|component| component.plugin_id == PLUGIN_ID),
        "refresh keeps the plugin panel"
    );
    let text = render_to_string(&app, 160, 140);
    assert!(text.contains("Queue metrics"), "{text}");
}

#[test]
fn durable_stale_and_disconnected_freshness_is_visible() {
    let stale = durable_fixture("hp-ui-durable-stale", 20_000);
    let snapshot = durable_snapshot(&stale, LedgerTransport::InProcess);
    let component = snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("plugin component");
    assert!(component.stale, "stale: {component:?}");
    let plain = frame(snapshot, 160, 140);
    assert!(plain.contains("STALE"), "{plain}");

    let gone = durable_fixture("hp-ui-durable-gone", 600_000);
    let snapshot = durable_snapshot(&gone, LedgerTransport::InProcess);
    let component = snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("plugin component");
    assert_eq!(component.freshness, "unavailable");
    assert!(component.disconnected, "disconnected: {component:?}");
    let plain = frame(snapshot, 160, 140);
    assert!(plain.contains("DISCONNECTED"), "{plain}");
}

#[test]
fn corrupt_registry_checkpoint_fails_status_closed() {
    let dir = ScratchDir::new("hp-ui-corrupt-registry");
    // A reachable selected-ledger checkpoint whose content cannot be decoded.
    let mut handle = LedgerHandle::in_process(dir.path()).expect("selected ledger");
    let mut commit = ComponentCommit {
        format: COMPONENT_COMMIT_FORMAT.to_string(),
        namespace: REGISTRY_NAMESPACE.to_string(),
        commit_id: "corrupt-registry:1".to_string(),
        input_digest: String::new(),
        expected_revision: 0,
        checkpoint: Some(json!({"desired": 5})),
        events: Vec::new(),
        operations: Vec::new(),
        receipts: Vec::new(),
    };
    commit.input_digest = commit.computed_input_digest().expect("digest");
    handle
        .component_commit(&commit)
        .expect("commit corrupt checkpoint");
    drop(handle);

    let error = snapshot_with_selected_transport(
        dir.path(),
        &Transports::all_in_process(),
        LedgerTransport::InProcess,
    )
    .expect_err("a corrupt registry checkpoint must fail closed, never an empty healthy panel");
    assert!(error.contains("plugin registry state"), "{error}");
}

#[test]
fn process_transport_reads_the_same_production_read_model() {
    let fixture = durable_fixture("hp-ui-production-process", 0);
    let snapshot = durable_snapshot(&fixture, LedgerTransport::Process);
    let component = snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .unwrap_or_else(|| {
            panic!(
                "plugin panel over the process transport: {:?}",
                snapshot.plugins
            )
        });
    assert_eq!(component.freshness, "fresh");
    assert!(component
        .views
        .iter()
        .any(|view| view.view_id == "metrics" && view.state == "rendered"));
}

#[test]
fn stale_heartbeat_is_visible() {
    let mut h = harness("hp-ui-stale", 0);
    h.publish_all();
    assert_eq!(h.component().freshness, "fresh");

    // Advance past the stale threshold but before the failed threshold.
    h.clock.advance(1_500);
    let component = h.component();
    assert!(component.stale, "stale flag: {component:?}");
    assert!(!component.disconnected, "stale is not disconnected yet");

    let plain = frame(h.snapshot(), 160, 140);
    assert!(plain.contains("stale"), "{plain}");
    assert!(plain.contains("STALE"), "{plain}");
}

#[test]
fn disconnected_lifecycle_is_visible() {
    let mut h = harness("hp-ui-disconnected", 0);
    h.publish_all();
    h.registry
        .mark_failed(PLUGIN_ID, "process exited unexpectedly", START_MS)
        .expect("mark failed");

    let component = h.component();
    assert!(component.disconnected, "disconnected flag: {component:?}");
    assert_eq!(component.lifecycle, "failed");

    let plain = frame(h.snapshot(), 160, 140);
    assert!(plain.contains("failed"), "{plain}");
    assert!(plain.contains("DISCONNECTED"), "{plain}");
    assert!(plain.contains("process exited unexpectedly"), "{plain}");
}

#[test]
fn unsupported_view_version_is_visible() {
    let mut h = harness("hp-ui-unsupported", 0);
    h.publish_all();

    let pending = h.view("pending");
    assert_eq!(pending.state, "unsupported", "{pending:?}");
    assert_eq!(pending.schema_version, 2);
    let note = pending.note.clone().unwrap_or_default();
    assert!(note.contains("v2"), "{note}");
    assert!(note.contains("v1"), "{note}");

    let plain = frame(h.snapshot(), 160, 140);
    assert!(plain.contains("[unsupported]"), "{plain}");
    assert!(
        plain.contains("view expects schema v2 but the report carries v1"),
        "{plain}"
    );
}

#[test]
fn component_navigation_is_bounded() {
    let mut h = harness("hp-ui-navigation", 30);
    h.publish_all();

    let snapshot = h.snapshot();
    let app = App::new(snapshot.clone(), Transports::all_in_process(), 1000);
    let top = render_to_string(&app, 120, 16);
    assert!(top.contains("more"), "paging marker: {top}");
    assert_eq!(app.components_offset, 0);

    let mut scrolled = App::new(snapshot, Transports::all_in_process(), 1000);
    scrolled.scroll_components(12);
    assert!(scrolled.components_offset > 0);
    let moved = render_to_string(&scrolled, 120, 16);
    assert!(moved.contains("earlier"), "earlier marker: {moved}");
    assert_ne!(top, moved, "scrolling changes the visible window");

    // The offset saturates instead of growing without bound.
    scrolled.scroll_components(10_000);
    let total = agalma_conductor::watch::component_line_count(&scrolled);
    assert!(scrolled.components_offset <= total.saturating_sub(1));
    scrolled.scroll_components(-10_000);
    assert_eq!(scrolled.components_offset, 0);
}

#[test]
fn absent_and_builtin_snapshots_remain_readable() {
    // An absent ledger still renders every pane (built-in TUI snapshot).
    let dir = ScratchDir::new("hp-ui-absent");
    let absent = once_frame(dir.path(), 1000).expect("absent frame");
    assert!(absent.contains("exists:false"), "{absent}");
    assert!(absent.contains("agalma watch"), "{absent}");

    // A built-in-only registry renders without a manifest or views.
    let mut registry = PluginRegistry::new();
    registry.register_builtin("builtin.solo", START_MS);
    let components = derive_plugin_components(&registry, None);
    assert_eq!(components.len(), 1);
    assert_eq!(components[0].origin, "built-in");
    assert!(components[0].views.is_empty());

    let snapshot = render_snapshot(dir.path(), components);
    let plain = frame(snapshot, 150, 40);
    assert!(plain.contains("builtin.solo"), "{plain}");
    assert!(plain.contains("(no declared views)"), "{plain}");
}

#[test]
fn viewing_and_refresh_write_nothing() {
    let mut h = harness("hp-ui-read-only", 2);
    h.publish_all();

    let before = tree_digest(h.dir.path());

    let snapshot = h.snapshot();
    let plain = frame(snapshot.clone(), 150, 50);
    assert!(plain.contains(PLUGIN_ID), "{plain}");

    // Refresh re-reads the same state dir; still nothing written.
    let mut app = App::new(snapshot, Transports::all_in_process(), 1000);
    app.refresh(h.dir.path()).expect("refresh");
    let _ = render_to_string(&app, 150, 50);

    let after = tree_digest(h.dir.path());
    assert_eq!(before, after, "viewing/refresh must not write");
}

#[test]
fn control_characters_are_escaped_and_rows_bounded() {
    let mut h = harness("hp-ui-bounds", 0);
    h.publish_health();
    h.publish(1, NS_STATUS, 1, json!({"ready": "true"}))
        .expect("status");
    // A metrics value with a control character and a newline.
    h.publish(
        2,
        NS_METRICS,
        1,
        json!({"queued": "3\u{7}\nline", "in_flight": 1, "failed": 0}),
    )
    .expect("metrics");
    // A table with more rows and one longer cell than the bound allows.
    let rows: Vec<Value> = (0..200)
        .map(|index| {
            let state = if index == 0 {
                "x".repeat(200)
            } else {
                "ok".to_string()
            };
            json!({
                "task": format!("t{index}"),
                "state": state,
                "attempt": index,
            })
        })
        .collect();
    h.publish(3, NS_TABLE, 1, Value::Array(rows))
        .expect("table");
    h.publish(4, NS_TIMELINE, 1, json!([])).expect("timeline");
    h.publish(5, NS_ALERT, 1, json!({"level": "info", "message": "ok"}))
        .expect("alert");
    h.publish(6, NS_ARTIFACT, 1, json!({"path": "/a"}))
        .expect("artifact");
    h.publish(7, NS_PENDING, 1, json!({"value": "v1"}))
        .expect("pending");
    h.publish(8, NS_EXTRA, 1, json!({"opaque": "data"}))
        .expect("extra");

    // Metrics value is escaped in the model.
    let metrics = h.view("metrics");
    match &metrics.blocks[0] {
        ViewBlock::Metrics { rows } => {
            assert_eq!(rows[0].value, r"3\u{7}\nline");
        }
        other => panic!("expected metrics, got {other:?}"),
    }

    // Table rows are bounded and truncated count is reported.
    let table = h.view("tasks");
    match &table.blocks[0] {
        ViewBlock::Table {
            rows, truncated, ..
        } => {
            assert_eq!(rows.len(), MAX_VIEW_BLOCK_ROWS);
            assert_eq!(*truncated, 200 - MAX_VIEW_BLOCK_ROWS);
        }
        other => panic!("expected table, got {other:?}"),
    }

    let width = 150u16;
    let plain = frame(h.snapshot(), width, 200);
    // Escaped control characters, never raw ones.
    assert!(plain.contains(r"3\u{7}\nline"), "{plain}");
    assert!(
        !plain.chars().any(|ch| ch.is_control() && ch != '\n'),
        "raw control character rendered"
    );
    // Bounded rows and text.
    assert!(plain.contains("more rows"), "{plain}");
    for line in plain.split('\n') {
        assert!(
            line.chars().count() <= width as usize,
            "line wider than the frame: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Live host health overlay (HP.6 reopened addendum)
//
// The shipped interactive `watch` path builds its plugin panel from the
// stateless ledger snapshot. The production plugin host additionally holds the
// live registry and its attached `ReportIngestor`, whose transient
// `PluginHealth` (recorded by the identity-checked `plugin.Health` probe) is
// never persisted. `PluginHost::plugin_components` is the production helper
// that supplies that live panel and `watch::App::overlay_plugins` is the
// production overlay the interactive loop applies each tick. These cases drive
// both and render through `watch::render_to_string` (`ratatui::TestBackend`).
// No plugin-specific UI code is exercised.
// ---------------------------------------------------------------------------

/// A real assembled production [`PluginHost`] over the sealed fixture package.
///
/// Assembly installs the hub-owned generation and opens the host's live report
/// ingestor against the selected ledger; it does not spawn the child, so the
/// transient health below is recorded exactly as the identity-checked probe
/// gate would (`PluginHealth/v1` naming the host pin). HP.8 owns the executable
/// end-to-end child proof.
struct HostFixture {
    dir: ScratchDir,
    host: PluginHost,
    clock: ManualClock,
    generation: u64,
}

fn host_fixture(name: &str) -> HostFixture {
    let dir = ScratchDir::new(name);
    let root = dir.child("discovery");
    fs::create_dir_all(&root).expect("discovery root");
    let package_dir = root.join("pkg");
    seal_package(&package_dir, &manifest_value(PLUGIN_ID));

    let clock = ManualClock::new(START_MS);
    let ledger = LedgerHandle::in_process(dir.path()).expect("selected ledger");
    let config = PluginHostConfig {
        state_dir: dir.path().to_path_buf(),
        plugin_roots: vec![PluginRoot::required(&root)],
        once: false,
    };
    let host = PluginHost::assemble_with_ledger(
        &config,
        &InMemoryInstallations::new(),
        ledger,
        clock.clock(),
    )
    .expect("assemble production host");
    let generation = *host
        .installed
        .get(PLUGIN_ID)
        .unwrap_or_else(|| panic!("the fixture package auto-installs: {:?}", host.diagnostics));
    HostFixture {
        dir,
        host,
        clock,
        generation,
    }
}

impl HostFixture {
    /// The host-pinned binding for the installed generation.
    fn pin(&self) -> BindingPin {
        self.host
            .pinned_binding(PLUGIN_ID)
            .expect("host-pinned binding")
    }

    /// Record a transient hub-observed health observation exactly as the
    /// identity-checked `plugin.Health` probe gate does.
    fn record_health(&self, state: HealthState, detail: &str) {
        let pin = self.pin();
        let health = PluginHealth {
            format: "PluginHealth/v1".to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding: pin.clone(),
            state,
            detail: detail.to_string(),
            checked_at_unix_ms: START_MS,
        };
        let reports = self
            .host
            .reports
            .clone()
            .expect("the production host attaches a report ingestor");
        reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_health(PLUGIN_ID, &pin, &health)
            .expect("record health");
    }

    /// Activate the staged generation and mark it Ready (the pending-to-active
    /// transition the READY/Describe/Health gates perform).
    fn mark_ready(&mut self) {
        self.host
            .registry
            .activate(PLUGIN_ID, self.generation)
            .expect("activate staged generation");
        self.host
            .registry
            .mark_ready(PLUGIN_ID, self.generation, START_MS)
            .expect("mark ready");
    }

    /// The live host read model through the exact production helper.
    fn live_components(&self) -> Vec<PluginComponent> {
        self.host.plugin_components()
    }

    fn live_component(&self) -> PluginComponent {
        self.live_components()
            .into_iter()
            .find(|component| component.plugin_id == PLUGIN_ID)
            .unwrap_or_else(|| panic!("the live host exposes `{PLUGIN_ID}`"))
    }

    /// The live host panel overlaid onto an empty stateless snapshot, rendered
    /// off-screen through the production overlay helper.
    fn overlay_frame(&self) -> (PluginComponent, String) {
        let live = self.live_components();
        let component = live
            .iter()
            .find(|component| component.plugin_id == PLUGIN_ID)
            .cloned()
            .expect("live plugin component");
        let mut app = App::new(
            render_snapshot(self.dir.path(), Vec::new()),
            Transports::all_in_process(),
            1000,
        );
        app.overlay_plugins(live);
        (component, render_to_string(&app, 160, 60))
    }
}

#[test]
fn live_host_ready_health_overlays_into_the_shipped_ui() {
    let mut fx = host_fixture("hp-ui-live-ready");
    fx.mark_ready();
    fx.record_health(HealthState::Ready, "serving");

    let (component, text) = fx.overlay_frame();
    assert_eq!(component.plugin_id, PLUGIN_ID);
    assert_eq!(component.lifecycle, "ready");
    assert_eq!(component.generation, Some(fx.generation));
    assert_eq!(component.freshness, "fresh", "{component:?}");
    assert!(!component.disconnected, "{component:?}");
    let health = component.health.as_ref().expect("health view");
    assert_eq!(health.state, "ready");
    assert_eq!(health.detail, "serving");

    // Rendered through TestBackend with the exact identity, generation, detail.
    assert!(text.contains(PLUGIN_ID), "{text}");
    assert!(text.contains(&format!("gen{}", fx.generation)), "{text}");
    assert!(text.contains("health: ready · serving"), "{text}");
    assert!(!text.contains("DISCONNECTED"), "{text}");
}

#[test]
fn live_host_unavailable_health_overlays_disconnected_into_the_ui() {
    let mut fx = host_fixture("hp-ui-live-unavailable");
    fx.mark_ready();
    fx.record_health(HealthState::Ready, "serving");

    // The child crashes: the supervisor records the failure and the identity
    // checked health probe records an `unavailable` observation (the hub never
    // fabricates a `ready`). The injected clock then ages past the failed
    // threshold so freshness is `unavailable` too.
    fx.host
        .registry
        .mark_failed(PLUGIN_ID, "child exited unexpectedly", START_MS)
        .expect("mark failed");
    fx.record_health(
        HealthState::Unavailable,
        "plugin.Health probe failed: process exited",
    );
    fx.clock.advance(31_000);

    let (component, text) = fx.overlay_frame();
    assert_eq!(component.lifecycle, "failed");
    assert_eq!(component.generation, Some(fx.generation));
    assert_eq!(component.freshness, "unavailable", "{component:?}");
    assert!(component.disconnected, "{component:?}");
    let health = component.health.as_ref().expect("health view");
    assert_eq!(health.state, "unavailable");
    assert!(health.detail.contains("process exited"), "{health:?}");

    assert!(text.contains(PLUGIN_ID), "{text}");
    assert!(text.contains("DISCONNECTED"), "{text}");
    assert!(text.contains("unavailable"), "{text}");
    assert!(text.contains("failed"), "{text}");
    assert!(text.contains("child exited unexpectedly"), "{text}");
}

#[test]
fn live_host_recovery_after_restart_overlays_into_the_ui() {
    let mut fx = host_fixture("hp-ui-live-recovery");
    fx.mark_ready();
    fx.record_health(HealthState::Ready, "serving");

    // Crash, then the host restarts the same pinned generation under a fresh
    // launch epoch and re-runs the identity-checked health gate.
    fx.host
        .registry
        .mark_failed(PLUGIN_ID, "child exited unexpectedly", START_MS)
        .expect("mark failed");
    fx.record_health(HealthState::Unavailable, "probe failed");
    fx.clock.advance(31_000);
    assert!(fx.live_component().disconnected, "crash is visible first");

    fx.host
        .registry
        .mark_starting(PLUGIN_ID, fx.generation, START_MS + 31_000)
        .expect("restart launch epoch");
    fx.mark_ready();
    fx.record_health(HealthState::Ready, "recovered and serving");

    let (component, text) = fx.overlay_frame();
    assert_eq!(component.lifecycle, "ready");
    assert_eq!(component.generation, Some(fx.generation));
    assert_eq!(component.freshness, "fresh", "{component:?}");
    assert!(!component.disconnected, "{component:?}");
    let health = component.health.as_ref().expect("health view");
    assert_eq!(health.state, "ready");
    assert_eq!(health.detail, "recovered and serving");

    assert!(text.contains(&format!("gen{}", fx.generation)), "{text}");
    assert!(text.contains("health: ready · recovered and serving"), "{text}");
    assert!(!text.contains("DISCONNECTED"), "{text}");
}

#[test]
fn live_overlay_reports_fresh_health_over_stale_durable_reports() {
    // Independent semantics: report **freshness** is driven by the age of the
    // durable observations, while **health** is the hub's live observation of the
    // plugin. A stateless snapshot whose reports are 600 s old is disconnected
    // regardless of any health field its checkpoint happens to carry; the
    // attached live host observes a current `ready` health. The overlay must
    // present the live health/freshness, must not mask the stale durable report
    // state it replaced (report age still drives the durable panel it overlays),
    // and must keep durable-only plugins.
    let stale = durable_fixture("hp-ui-overlay-durable-stale", 600_000);
    let durable = durable_snapshot(&stale, LedgerTransport::InProcess);
    let durable_component = durable
        .plugins
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("durable component");
    assert_eq!(
        durable_component.freshness, "unavailable",
        "{durable_component:?}"
    );
    assert!(durable_component.disconnected, "{durable_component:?}");
    let durable_text = render_to_string(
        &App::new(durable.clone(), Transports::all_in_process(), 1000),
        160,
        60,
    );
    assert!(durable_text.contains("DISCONNECTED"), "{durable_text}");

    let mut fx = host_fixture("hp-ui-overlay-live");
    fx.mark_ready();
    fx.record_health(HealthState::Ready, "serving");
    let live = fx.live_components();
    let live_component = live
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("live component");
    assert_eq!(live_component.freshness, "fresh", "{live_component:?}");
    assert!(!live_component.disconnected, "{live_component:?}");
    assert_eq!(
        live_component.health.as_ref().map(|health| health.state.as_str()),
        Some("ready")
    );

    let mut app = App::new(durable, Transports::all_in_process(), 1000);
    app.overlay_plugins(live);
    let merged = app
        .snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == PLUGIN_ID)
        .expect("merged component");
    assert_eq!(merged.freshness, "fresh", "{merged:?}");
    assert!(!merged.disconnected, "{merged:?}");
    assert_eq!(
        merged.health.as_ref().map(|health| health.state.as_str()),
        Some("ready")
    );

    let text = render_to_string(&app, 160, 60);
    assert!(text.contains(PLUGIN_ID), "{text}");
    assert!(text.contains("health: ready · serving"), "{text}");
    assert!(!text.contains("DISCONNECTED"), "{text}");
}

#[test]
fn empty_live_panel_leaves_the_stateless_snapshot_untouched() {
    // No live host: the durable stateless panel is retained verbatim.
    let fixture = durable_fixture("hp-ui-overlay-none", 0);
    let mut app = App::new(
        durable_snapshot(&fixture, LedgerTransport::InProcess),
        Transports::all_in_process(),
        1000,
    );
    let before = app.snapshot.plugins.clone();
    assert!(before.iter().any(|component| component.plugin_id == PLUGIN_ID));
    app.overlay_plugins(Vec::new());
    assert_eq!(app.snapshot.plugins, before, "empty live panel restores baseline");
}

/// The plugin id set of the current panel.
fn panel_ids(app: &App) -> BTreeSet<String> {
    app.snapshot
        .plugins
        .iter()
        .map(|component| component.plugin_id.clone())
        .collect()
}

#[test]
fn live_overlay_removal_drops_omitted_live_only_plugins() {
    // A durable baseline with one durable-only plugin that no live host reports.
    let durable = vec![generated_live_component(
        "durable.only",
        1,
        HealthState::Degraded,
        "durable",
    )];
    let mut app = App::new(
        render_snapshot(Path::new("overlay-removal"), durable.clone()),
        Transports::all_in_process(),
        1000,
    );

    // First authoritative live panel adds two live-only plugins.
    let live_a = generated_live_component("live.a", 2, HealthState::Ready, "a-ready");
    let live_b = generated_live_component("live.b", 3, HealthState::Unavailable, "b-gone");
    app.overlay_plugins(vec![live_a.clone(), live_b]);
    assert_eq!(
        panel_ids(&app),
        BTreeSet::from([
            "durable.only".to_string(),
            "live.a".to_string(),
            "live.b".to_string(),
        ])
    );

    // Second authoritative panel omits `live.b`: it must disappear, not linger.
    app.overlay_plugins(vec![live_a.clone()]);
    assert_eq!(
        panel_ids(&app),
        BTreeSet::from(["durable.only".to_string(), "live.a".to_string()])
    );
    assert!(!app
        .snapshot
        .plugins
        .iter()
        .any(|component| component.plugin_id == "live.b"));

    // The durable-only entry is preserved verbatim; the live entry is current.
    let durable_only = app
        .snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == "durable.only")
        .expect("durable-only preserved");
    assert_eq!(*durable_only, durable[0]);
    let live = app
        .snapshot
        .plugins
        .iter()
        .find(|component| component.plugin_id == "live.a")
        .expect("live.a current");
    assert_eq!(*live, live_a);
}

#[test]
fn empty_live_panel_drops_previous_live_only_plugins() {
    let durable = vec![generated_live_component(
        "durable.only",
        1,
        HealthState::Ready,
        "durable",
    )];
    let mut app = App::new(
        render_snapshot(Path::new("overlay-empty"), durable.clone()),
        Transports::all_in_process(),
        1000,
    );
    app.overlay_plugins(vec![generated_live_component(
        "live.a",
        2,
        HealthState::Ready,
        "a",
    )]);
    assert!(app
        .snapshot
        .plugins
        .iter()
        .any(|component| component.plugin_id == "live.a"));

    // An authoritative empty panel means the host currently reports no plugins:
    // the live layer is replaced with nothing, so live-only entries disappear
    // while durable-only entries remain.
    app.overlay_plugins(Vec::new());
    assert_eq!(app.snapshot.plugins, durable);
    assert!(!app
        .snapshot
        .plugins
        .iter()
        .any(|component| component.plugin_id == "live.a"));
}

#[test]
fn clear_live_plugins_restores_the_durable_baseline() {
    let durable = vec![generated_live_component(
        "durable.only",
        1,
        HealthState::Ready,
        "durable",
    )];
    let mut app = App::new(
        render_snapshot(Path::new("overlay-clear"), durable.clone()),
        Transports::all_in_process(),
        1000,
    );
    app.overlay_plugins(vec![generated_live_component(
        "live.a",
        2,
        HealthState::Ready,
        "a",
    )]);
    assert_eq!(panel_ids(&app).len(), 2);

    // `None` from the tick (no attached host) drops the live layer entirely.
    app.clear_live_plugins();
    assert_eq!(app.snapshot.plugins, durable);
}

// ---------------------------------------------------------------------------
// Property cases (recorded seed + budget)
// ---------------------------------------------------------------------------

fn random_text(rng: &mut XorShift64, max: usize) -> String {
    let len = rng.below((max + 1) as u64) as usize;
    (0..len)
        .map(|_| {
            // Bias into the ASCII/control range so escaping is exercised.
            (rng.below(140) as u8) as char
        })
        .collect()
}

fn random_value(rng: &mut XorShift64, depth: u32) -> Value {
    let upper = if depth == 0 { 4 } else { 6 };
    match rng.below(upper) {
        0 => Value::Null,
        1 => Value::Bool(rng.below(2) == 0),
        2 => json!(rng.next_u64() as i64),
        3 => Value::String(random_text(rng, 40)),
        4 => Value::Array(
            (0..rng.below(40))
                .map(|_| random_value(rng, depth - 1))
                .collect(),
        ),
        _ => {
            let mut object = serde_json::Map::new();
            for index in 0..rng.below(5) + 1 {
                object.insert(format!("field{index}"), random_value(rng, depth - 1));
            }
            Value::Object(object)
        }
    }
}

fn block_strings(block: &ViewBlock) -> Vec<String> {
    match block {
        ViewBlock::Status { value } => vec![value.clone()],
        ViewBlock::Metrics { rows } => rows
            .iter()
            .flat_map(|row| [row.name.clone(), row.value.clone()])
            .collect(),
        ViewBlock::Table { headers, rows, .. } => headers
            .iter()
            .cloned()
            .chain(rows.iter().flatten().cloned())
            .collect(),
        ViewBlock::Timeline { rows } => rows
            .iter()
            .flat_map(|row| [row.time.clone(), row.text.clone()])
            .collect(),
        ViewBlock::Alert { level, message } => vec![level.clone(), message.clone()],
        ViewBlock::ArtifactLink { label, target } => vec![label.clone(), target.clone()],
    }
}

#[test]
fn rendered_blocks_are_always_escaped_and_bounded() {
    let mut rng = XorShift64::new(PROPERTY_SEED);
    let fields = vec![
        "alpha".to_string(),
        "beta.gamma".to_string(),
        "level".to_string(),
    ];
    let kinds = [
        ViewKind::Status,
        ViewKind::Metrics,
        ViewKind::Table,
        ViewKind::Timeline,
        ViewKind::Alert,
        ViewKind::ArtifactLink,
    ];

    for case in 0..PROPERTY_CASES {
        let payload = random_value(&mut rng, 2);
        for kind in kinds {
            let blocks = render_blocks(kind, &fields, &payload);
            assert!(!blocks.is_empty(), "case {case} {kind:?} produced no block");
            for block in &blocks {
                for text in block_strings(block) {
                    assert_eq!(
                        escape_control(&text),
                        text,
                        "case {case} {kind:?} unescaped {text:?}"
                    );
                    assert!(
                        text.chars().count() <= MAX_VIEW_TEXT_CHARS,
                        "case {case} {kind:?} unbounded text ({} chars)",
                        text.chars().count()
                    );
                }
                match block {
                    ViewBlock::Table {
                        headers,
                        rows,
                        truncated,
                    } => {
                        assert!(rows.len() <= MAX_VIEW_BLOCK_ROWS);
                        let _ = truncated;
                        for row in rows {
                            assert_eq!(row.len(), headers.len(), "case {case} ragged table row");
                        }
                    }
                    ViewBlock::Timeline { rows } => {
                        assert!(rows.len() <= MAX_VIEW_BLOCK_ROWS);
                    }
                    ViewBlock::Metrics { rows } => {
                        assert!(rows.len() <= MAX_VIEW_BLOCK_ROWS);
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Live-overlay property cases (recorded seed + budget)
// ---------------------------------------------------------------------------

/// Recorded deterministic property-test seed for the live-overlay invariant.
const OVERLAY_PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_006b;
/// Recorded property-test case budget for the live-overlay invariant.
const OVERLAY_PROPERTY_CASES: u64 = 60;

/// Build one generated live plugin component independently of the production
/// `derive_plugin_components` (the property assertions must not mirror the
/// implementation under test).
fn generated_live_component(
    plugin_id: &str,
    generation: u64,
    state: HealthState,
    detail: &str,
) -> PluginComponent {
    let (freshness, lifecycle, stale, disconnected) = match state {
        HealthState::Ready => ("fresh", "ready", false, false),
        HealthState::Degraded => ("stale", "ready", true, false),
        HealthState::Unavailable => ("unavailable", "failed", false, true),
    };
    PluginComponent {
        plugin_id: plugin_id.to_string(),
        origin: "discovered".to_string(),
        desired: "running".to_string(),
        lifecycle: lifecycle.to_string(),
        generation: Some(generation),
        freshness: freshness.to_string(),
        stale,
        disconnected,
        incomplete: false,
        last_error: None,
        health: Some(HealthView {
            state: match state {
                HealthState::Ready => "ready",
                HealthState::Degraded => "degraded",
                HealthState::Unavailable => "unavailable",
            }
            .to_string(),
            detail: detail.to_string(),
            observed_at_unix_ms: START_MS,
        }),
        actions: Vec::new(),
        views: Vec::new(),
    }
}

/// Property: the production overlay applies the live panel exactly — a live
/// component replaces any durable entry with the same plugin id (transient
/// health wins), durable-only entries are preserved verbatim, and the rendered
/// frame shows every live identity, generation and health detail. Generated over
/// a recorded seed and bounded case budget.
#[test]
fn overlay_health_property_preserves_identity_generation_and_refresh() {
    let mut rng = XorShift64::new(OVERLAY_PROPERTY_SEED);
    let states = [
        HealthState::Ready,
        HealthState::Degraded,
        HealthState::Unavailable,
    ];

    for case in 0..OVERLAY_PROPERTY_CASES {
        let pool: Vec<String> = (0..4).map(|index| format!("prop.plugin.{index}")).collect();

        // The durable stateless panel holds an arbitrary subset with an opaque
        // freshness, a distinct generation and no live health.
        let mut baseline: Vec<PluginComponent> = Vec::new();
        for plugin_id in &pool {
            if rng.below(2) != 0 {
                continue;
            }
            let mut component = generated_live_component(
                plugin_id,
                rng.below(5) + 1,
                HealthState::Degraded,
                "durable",
            );
            component.health = None;
            component.freshness = "unknown".to_string();
            component.stale = false;
            component.disconnected = false;
            baseline.push(component);
        }

        // The live host panel holds an independent subset with generated
        // identity/generation/health.
        let mut live: Vec<PluginComponent> = Vec::new();
        for plugin_id in &pool {
            if rng.below(2) != 0 {
                continue;
            }
            let generation = rng.below(5) + 1;
            let state = states[rng.below(3) as usize].clone();
            let detail = format!("detail-{case}-{plugin_id}");
            live.push(generated_live_component(plugin_id, generation, state, &detail));
        }

        let mut app = App::new(
            render_snapshot(Path::new("overlay-prop"), baseline.clone()),
            Transports::all_in_process(),
            1000,
        );
        app.overlay_plugins(live.clone());

        // The merged panel is exactly the union of ids, one entry each.
        let mut expected_ids: BTreeSet<String> =
            baseline.iter().map(|c| c.plugin_id.clone()).collect();
        expected_ids.extend(live.iter().map(|c| c.plugin_id.clone()));
        let merged_ids: BTreeSet<String> = app
            .snapshot
            .plugins
            .iter()
            .map(|c| c.plugin_id.clone())
            .collect();
        assert_eq!(merged_ids, expected_ids, "case {case} id union");

        // Each live component wins for its id, with its exact identity/health.
        for live_component in &live {
            let merged = app
                .snapshot
                .plugins
                .iter()
                .find(|c| c.plugin_id == live_component.plugin_id)
                .unwrap_or_else(|| panic!("case {case} missing live id"));
            assert_eq!(
                merged.generation, live_component.generation,
                "case {case} generation"
            );
            assert_eq!(merged.health, live_component.health, "case {case} health");
            assert_eq!(
                merged.freshness, live_component.freshness,
                "case {case} freshness"
            );
            assert_eq!(
                merged.disconnected, live_component.disconnected,
                "case {case} disconnected"
            );
        }

        // Durable-only ids keep their baseline entry verbatim.
        for baseline_component in &baseline {
            if live
                .iter()
                .any(|c| c.plugin_id == baseline_component.plugin_id)
            {
                continue;
            }
            let merged = app
                .snapshot
                .plugins
                .iter()
                .find(|c| c.plugin_id == baseline_component.plugin_id)
                .unwrap_or_else(|| panic!("case {case} durable-only id dropped"));
            assert_eq!(*merged, *baseline_component, "case {case} durable-only preserved");
        }

        // The rendered frame shows every live identity, generation and detail.
        let text = render_to_string(&app, 160, 200);
        for live_component in &live {
            assert!(
                text.contains(&live_component.plugin_id),
                "case {case} identity not rendered"
            );
            let generation = live_component.generation.expect("live generation");
            assert!(
                text.contains(&format!("gen{generation}")),
                "case {case} generation not rendered"
            );
            let detail = &live_component
                .health
                .as_ref()
                .expect("live health")
                .detail;
            assert!(
                text.contains(detail.as_str()),
                "case {case} health detail not rendered"
            );
        }
    }
}

/// Recorded deterministic seed for the live-overlay sequence invariant.
const OVERLAY_SEQUENCE_SEED: u64 = 0x0bd3_5eed_0000_006c;
/// Recorded case budget for the live-overlay sequence invariant.
const OVERLAY_SEQUENCE_CASES: u64 = 40;

/// Property: over a sequence of authoritative live panels, each one **replaces**
/// the previous live layer over the durable baseline. After every step the panel
/// is exactly the durable-only entries plus the current live entries: an omitted
/// live-only plugin disappears and a durable-only plugin persists, step after
/// step. Generated over a recorded seed and bounded case budget.
#[test]
fn overlay_sequence_property_replaces_the_live_layer() {
    let mut rng = XorShift64::new(OVERLAY_SEQUENCE_SEED);
    let states = [
        HealthState::Ready,
        HealthState::Degraded,
        HealthState::Unavailable,
    ];

    for case in 0..OVERLAY_SEQUENCE_CASES {
        let pool: Vec<String> = (0..5).map(|index| format!("seq.plugin.{index}")).collect();

        // A fixed durable baseline (opaque freshness, no live health).
        let mut durable: Vec<PluginComponent> = Vec::new();
        for plugin_id in &pool {
            if rng.below(2) != 0 {
                continue;
            }
            let mut component = generated_live_component(
                plugin_id,
                rng.below(5) + 1,
                HealthState::Degraded,
                "durable",
            );
            component.health = None;
            component.freshness = "unknown".to_string();
            component.stale = false;
            component.disconnected = false;
            durable.push(component);
        }

        let mut app = App::new(
            render_snapshot(Path::new("overlay-seq"), durable.clone()),
            Transports::all_in_process(),
            1000,
        );

        let steps = 1 + rng.below(4);
        for step in 0..steps {
            let mut live: Vec<PluginComponent> = Vec::new();
            for plugin_id in &pool {
                if rng.below(2) != 0 {
                    continue;
                }
                let generation = rng.below(5) + 1;
                let state = states[rng.below(3) as usize].clone();
                let detail = format!("seq-{case}-{step}-{plugin_id}");
                live.push(generated_live_component(plugin_id, generation, state, &detail));
            }
            app.overlay_plugins(live.clone());

            // The panel must be exactly the durable-only entries plus the
            // current live entries — never the previous step's live layer.
            let mut expected: BTreeMap<String, PluginComponent> = durable
                .iter()
                .cloned()
                .map(|component| (component.plugin_id.clone(), component))
                .collect();
            for component in &live {
                expected.insert(component.plugin_id.clone(), component.clone());
            }
            let actual: BTreeMap<String, PluginComponent> = app
                .snapshot
                .plugins
                .iter()
                .cloned()
                .map(|component| (component.plugin_id.clone(), component))
                .collect();
            assert_eq!(actual, expected, "case {case} step {step}");
        }
    }
}
