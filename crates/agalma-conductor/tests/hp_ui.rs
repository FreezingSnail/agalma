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
    derive_plugin_components, escape_control, render_blocks, ComponentView, PluginComponent,
    Transports, ViewBlock, MAX_VIEW_BLOCK_ROWS, MAX_VIEW_TEXT_CHARS,
};
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
