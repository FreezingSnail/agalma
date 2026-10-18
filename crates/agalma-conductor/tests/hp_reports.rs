//! HP.5 durable plugin reports, component storage, complete-state and
//! quiescent v4 -> v5 migration acceptance tests (`agalma-jmr.5`).
//!
//! Every case runs nonzero assertions against an independent fixture under the
//! persistent scratch root (never `/tmp`, no network, no paid provider call).
//! The security-relevant cases drive the **assembled production host** and its
//! real `hub.PublishReport` RPC over a Unix socket; the migration and
//! complete-state cases run through both ledger transports (`in-process`
//! `SqliteLedger` and a real supervised `agalma-ledger-server` binary). A
//! missing server binary fails the process cases rather than skipping them.
//!
//! Coverage:
//!
//! * authenticated binding identity: a publication from a different plugin or
//!   binding generation is refused before any write (assigned from the grant);
//! * durable duplicate delivery returns the original hub observation time;
//!   changed content at the same identity is `IdentityConflict`;
//! * transient heartbeats are not persisted, durable observations are;
//! * a sequence gap marks the view incomplete and a snapshot at the gap
//!   watermark recovers it; out-of-order reports below the watermark never roll
//!   the view back;
//! * a retired (stale) generation cannot overwrite the active view;
//! * unknown schemas and oversized payloads are refused visibly;
//! * restart and complete-state/v2 export/import preserve the view and replay
//!   identity;
//! * a normal v4 open performs no v5 upgrade; explicit migration refuses
//!   non-quiescent state and succeeds transactionally with a backup and a
//!   durable receipt (both transports);
//! * the assembled production host persists/reconstructs registry desired state
//!   through the selected LedgerApi, routes `hub.PublishReport` to its attached
//!   ingestor, and stores the observation durably;
//! * configured, user-controlled plugin roots auto-install from each package's
//!   immutable default configuration (no separate approval record), and the
//!   automatic grant is exactly `requested ∩ {report.publish, ledger.read}` —
//!   `ledger.write` and unknown capabilities stay denied.
//!
//! The v4 fixture is built privately by this test from the existing `rusqlite`
//! dev-dependency; it does not require (or expose) a production fixture API.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use agalma_conductor::composition::{
    auto_approved_capabilities, default_installation, InMemoryInstallations, PluginHost,
    PLUGIN_REPORT_GRANT_ENV, SAFE_CAPABILITY_ALLOWLIST,
};
use agalma_conductor::config::PluginHostConfig;
use agalma_conductor::ledger_process::{LedgerHandle, LedgerTransport};
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_conductor::plugin_reports::{
    report_namespace, seal_publication, seal_snapshot, snapshot_entry, ReportIngestor,
    ReportStore, DEFAULT_FAILED_AFTER_MS, DEFAULT_STALE_AFTER_MS, REGISTRY_NAMESPACE,
};
use agalma_conductor::plugin_registry::{DesiredState, RegistryCheckpoint};
use agalma_conductor::plugin_router::PublishReportCall;
use agalma_contracts::digest::{failure_code, sha256_digest, FailureCode};
use agalma_contracts::task_package::BindingPin;
use agalma_contracts::{
    ComponentCommit, ComponentEvent, ComponentOperation, ComponentOperationState, ComponentReceipt,
    CompleteStateV2, ContractError, ControlReceipt, DecisionKind, DecisionOption, DecisionOutcome,
    DecisionPin, DecisionRequest, DigestRecord, Durability, ExecutionId, LedgerApi, OperationId,
    PluginInstallation, PluginManifest, ReportEnvelope, COMPONENT_COMMIT_FORMAT,
};
use agalma_ipc::{HubCallbackClient, LedgerOps};
use agalma_ledger::server::SERVER_BIN;
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

/// Recorded deterministic property-test seed (xorshift64).
const PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_00a5;
/// Recorded property-test case budget.
const PROPERTY_CASES: u64 = 40;
/// Start of the injected hub clock (milliseconds).
const START_MS: u64 = 1_791_820_800_000;

const PLUGIN_ID: &str = "example.reporter";
const SCHEMA_NAMESPACE: &str = "example.reporter.status";
const SCHEMA_VERSION: u64 = 1;
const REPORT_CAPABILITY: &str = "report.publish";

const CONFIG_SCHEMA_BYTES: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
const DEFAULT_CONFIG_BYTES: &[u8] = b"{}\n";
const REPORT_SCHEMA_BYTES: &[u8] = b"{\"type\":\"object\",\"required\":[\"ready\"]}\n";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Short, unique run directory under the persistent scratch root.
fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

/// A `Send + Sync` injected clock (the hub RPC handler is cross-thread).
struct ManualClockFn {
    now: Arc<AtomicU64>,
}

impl ManualClockFn {
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
        implementation_version: "1.2.3".to_string(),
        generation,
        config_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

fn allowed_schemas() -> BTreeSet<(String, u64)> {
    [(SCHEMA_NAMESPACE.to_string(), SCHEMA_VERSION)]
        .into_iter()
        .collect()
}

fn publication(
    plugin_id: &str,
    generation: u64,
    sequence: u64,
    durability: Durability,
    payload: Value,
) -> agalma_contracts::ReportPublication {
    seal_publication(
        plugin_id,
        binding(plugin_id, generation),
        SCHEMA_NAMESPACE,
        SCHEMA_VERSION,
        sequence,
        START_MS,
        durability,
        payload,
    )
    .expect("payload hashes")
}

/// A report ingestor over its own durable store (`SqliteLedger` in the fixture).
fn open_ingestor(
    dir: &Path,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    generation: u64,
) -> ReportIngestor {
    let sqlite = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
    let generations = [(PLUGIN_ID.to_string(), generation)].into_iter().collect();
    ReportIngestor::open(
        Box::new(Mutex::new(sqlite)),
        clock,
        generations,
        allowed_schemas(),
    )
    .expect("open ingestor")
}

fn handle_for(dir: &Path, transport: LedgerTransport) -> LedgerHandle {
    match transport {
        LedgerTransport::InProcess => LedgerHandle::in_process(dir).expect("in-process ledger"),
        LedgerTransport::Process => LedgerHandle::process(dir).expect("supervised ledger server"),
    }
}

fn failure(err: &ContractError) -> FailureCode {
    failure_code(err).expect("structured failure detail")
}

fn events(store: &dyn ReportStore, plugin_id: &str) -> Vec<agalma_contracts::ComponentEventRow> {
    store
        .component_events(&report_namespace(plugin_id), 0, 100)
        .unwrap()
}

// --- private v4 fixture + process transport helpers -----------------------

/// Simulate a genuine pre-HP.5 database: create the v5 layout, then drop the
/// additive v5 tables and record schema version 4. This fixture lives in the
/// test (existing `rusqlite` dev-dependency); it is not a production API.
fn write_v4_database(path: &Path) {
    drop(SqliteLedger::open(path).expect("create ledger"));
    let conn = rusqlite::Connection::open(path).expect("reopen ledger");
    conn.execute_batch(
        "DROP TABLE IF EXISTS component_states; \
         DROP TABLE IF EXISTS component_events; \
         DROP TABLE IF EXISTS component_operations; \
         DROP TABLE IF EXISTS component_receipts; \
         DROP TABLE IF EXISTS component_commits; \
         DROP TABLE IF EXISTS schema_migrations; \
         UPDATE meta SET value='4' WHERE key='schema_version';",
    )
    .expect("record v4 layout");
}

fn resolve_server_binary(program: &str) -> Result<PathBuf, String> {
    agalma_conductor::harness_process::resolve_server_binary(program)
}

/// Run the real ledger server binary in explicit migration mode.
fn migrate_via_process(state_dir: &Path) -> String {
    let program = resolve_server_binary(SERVER_BIN).unwrap_or_else(|e| {
        panic!("required ledger server binary `{SERVER_BIN}` is missing: {e}")
    });
    let output = Command::new(program)
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--migrate-v5")
        .output()
        .expect("run ledger server migration mode");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "process migration failed: status={:?} stdout={stdout} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("MIGRATED"), "receipt line: {stdout}");
    stdout
}

fn backups_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().contains("v4-backup"))
                .unwrap_or(false)
        })
        .collect()
}

// --- sealed package fixture (host assembly) -------------------------------

fn byte_ref(path: &str, bytes: &[u8], media_type: &str) -> Value {
    json!({
        "path": path,
        "digest": sha256_digest(bytes),
        "bytes": bytes.len() as u64,
        "media_type": media_type,
    })
}

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
        "requested_capabilities": [REPORT_CAPABILITY],
        "required_services": [],
        "optional_services": [],
        "config_schema": byte_ref("objects/config-schema.json", CONFIG_SCHEMA_BYTES, "application/json"),
        "default_configuration": byte_ref("objects/default-config.json", DEFAULT_CONFIG_BYTES, "application/json"),
        "report_schemas": [{
            "namespace": SCHEMA_NAMESPACE,
            "version": SCHEMA_VERSION,
            "schema": byte_ref("objects/report-schema.json", REPORT_SCHEMA_BYTES, "application/json")
        }],
        "views": [{
            "view_id": "status",
            "schema_namespace": SCHEMA_NAMESPACE,
            "kind": "Status",
            "title": "Reporter status",
            "schema_version": SCHEMA_VERSION,
            "fields": ["ready"]
        }],
        "actions": [],
        "state_compatibility": {"reads": ["reporter-state/v1"], "writes": "reporter-state/v1"}
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

fn write_files(package_dir: &Path, files: &[(String, Vec<u8>)]) {
    std::fs::create_dir_all(package_dir).unwrap();
    for (relative, bytes) in files {
        let path = package_dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
    }
}

fn seal_package(package_dir: &Path, manifest: &Value, script: &str) -> PluginManifest {
    let mut files = support_files();
    files.push(("bin/plugin".to_string(), script.as_bytes().to_vec()));
    write_files(package_dir, &files);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let entry = package_dir.join("bin/plugin");
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).unwrap();
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

fn installation_for(
    package: &agalma_conductor::plugin_manifest::DiscoveredPackage,
) -> PluginInstallation {
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

fn live_child() -> String {
    "#!/bin/sh\nprintf 'READY {\"api\":\"plugin\",\"api_version\":1,\"impl\":\"fixture\",\"impl_version\":\"1.2.3\",\"capabilities\":[\"report.publish\"]}\n'\ncat >/dev/null\n"
        .to_string()
}

fn package_fixture(root: &Path) -> agalma_conductor::plugin_manifest::DiscoveredPackage {
    let package_dir = root.join("pkg");
    seal_package(&package_dir, &base_manifest(PLUGIN_ID), &live_child());
    let report = discover_plugins(&[PluginRoot::required(root)]).unwrap();
    report.package(PLUGIN_ID).unwrap().clone()
}

/// A sealed package fixture whose manifest requests exactly `caps` (configured
/// simply as a plugin id must be registered before the report). Used to exercise
/// auto-install of arbitrary capability sets, including denied ones.
fn package_fixture_caps(
    root: &Path,
    caps: &[&str],
) -> agalma_conductor::plugin_manifest::DiscoveredPackage {
    let package_dir = root.join("pkg");
    let mut manifest = base_manifest(PLUGIN_ID);
    manifest["requested_capabilities"] = json!(caps);
    seal_package(&package_dir, &manifest, &live_child());
    let report = discover_plugins(&[PluginRoot::required(root)]).unwrap();
    report.package(PLUGIN_ID).unwrap().clone()
}

fn host_config(state_dir: &Path, root: &Path) -> PluginHostConfig {
    PluginHostConfig {
        state_dir: state_dir.to_path_buf(),
        plugin_roots: vec![PluginRoot::required(root)],
        once: true,
    }
}

// ---------------------------------------------------------------------------
// Report identity, replay and authentication (direct ingestor)
// ---------------------------------------------------------------------------

#[test]
fn authenticated_binding_identity_is_enforced() {
    let dir = fresh("hp5-auth");
    let clock = ManualClockFn::new(START_MS);
    let mut ingestor = open_ingestor(&dir, clock.clock(), 1);

    let wrong_plugin = publication("example.other", 1, 1, Durability::Durable, json!({"ready": true}));
    let err = ingestor
        .publish(PLUGIN_ID, &binding(PLUGIN_ID, 1), &wrong_plugin)
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);

    let wrong_generation = publication(PLUGIN_ID, 2, 1, Durability::Durable, json!({"ready": true}));
    let err = ingestor
        .publish(PLUGIN_ID, &binding(PLUGIN_ID, 1), &wrong_generation)
        .unwrap_err();
    assert_eq!(failure(&err), FailureCode::AuthorityDenied);
    assert!(ingestor.view(PLUGIN_ID).unwrap().entries.is_empty());
}

#[test]
fn durable_replay_returns_original_observation_and_changed_content_conflicts() {
    let dir = fresh("hp5-replay");
    let clock = ManualClockFn::new(START_MS);
    let mut ingestor = open_ingestor(&dir, clock.clock(), 1);
    let auth = binding(PLUGIN_ID, 1);

    let first = publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"ready": true}));
    let observed = ingestor.publish(PLUGIN_ID, &auth, &first).unwrap();
    let original_time = observed.observed_at_unix_ms;

    clock.advance(5_000);
    let replay = ingestor.publish(PLUGIN_ID, &auth, &first).unwrap();
    assert_eq!(replay.observed_at_unix_ms, original_time);
    assert_eq!(replay, observed);

    let changed = publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"ready": false}));
    let err = ingestor.publish(PLUGIN_ID, &auth, &changed).unwrap_err();
    assert_eq!(failure(&err), FailureCode::IdentityConflict);

    let rows = events(ingestor.store(), PLUGIN_ID);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.kind, "report.observed");
}

#[test]
fn transient_heartbeats_are_not_persisted_but_update_freshness() {
    let dir = fresh("hp5-heartbeat");
    let clock = ManualClockFn::new(START_MS);
    let mut ingestor = open_ingestor(&dir, clock.clock(), 1);
    let auth = binding(PLUGIN_ID, 1);

    let heartbeat = publication(PLUGIN_ID, 1, 1, Durability::Transient, json!({"ready": true}));
    let envelope = ingestor.publish(PLUGIN_ID, &auth, &heartbeat).unwrap();
    assert_eq!(
        ingestor.freshness(PLUGIN_ID),
        agalma_conductor::plugin_reports::Freshness::Fresh
    );
    assert!(ingestor
        .view(PLUGIN_ID)
        .unwrap()
        .entries
        .contains_key(SCHEMA_NAMESPACE));
    assert!(events(ingestor.store(), PLUGIN_ID).is_empty());

    clock.advance(DEFAULT_STALE_AFTER_MS + 1);
    assert_eq!(
        ingestor.freshness(PLUGIN_ID),
        agalma_conductor::plugin_reports::Freshness::Stale
    );
    clock.advance(DEFAULT_FAILED_AFTER_MS);
    assert_eq!(
        ingestor.freshness(PLUGIN_ID),
        agalma_conductor::plugin_reports::Freshness::Unavailable
    );
    assert_eq!(envelope.durability, Durability::Transient);
}

#[test]
fn gap_marks_incomplete_and_snapshot_recovers_without_rollback() {
    let dir = fresh("hp5-gap");
    let clock = ManualClockFn::new(START_MS);
    let mut ingestor = open_ingestor(&dir, clock.clock(), 1);
    let auth = binding(PLUGIN_ID, 1);

    ingestor
        .publish(PLUGIN_ID, &auth, &publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"value": 1})))
        .unwrap();
    ingestor
        .publish(PLUGIN_ID, &auth, &publication(PLUGIN_ID, 1, 3, Durability::Durable, json!({"value": 3})))
        .unwrap();
    let view = ingestor.view(PLUGIN_ID).unwrap();
    assert!(view.incomplete);
    assert!(view.needs_snapshot);
    assert_eq!(
        view.entries.get(SCHEMA_NAMESPACE).unwrap().payload,
        json!({"value": 1})
    );

    let snapshot = seal_snapshot(
        PLUGIN_ID,
        auth.clone(),
        3,
        vec![snapshot_entry(SCHEMA_NAMESPACE, SCHEMA_VERSION, json!({"value": 3})).unwrap()],
    )
    .unwrap();
    ingestor.apply_snapshot(PLUGIN_ID, &auth, &snapshot).unwrap();
    let view = ingestor.view(PLUGIN_ID).unwrap();
    assert!(!view.incomplete);
    assert_eq!(view.snapshot_watermark, 3);
    assert_eq!(view.entries.get(SCHEMA_NAMESPACE).unwrap().payload, json!({"value": 3}));

    ingestor
        .publish(PLUGIN_ID, &auth, &publication(PLUGIN_ID, 1, 2, Durability::Durable, json!({"value": 2})))
        .unwrap();
    assert_eq!(
        ingestor.view(PLUGIN_ID).unwrap().entries.get(SCHEMA_NAMESPACE).unwrap().payload,
        json!({"value": 3})
    );
    assert!(!ingestor.view(PLUGIN_ID).unwrap().incomplete);
}

#[test]
fn stale_generation_cannot_overwrite_the_active_view() {
    let dir = fresh("hp5-stale");
    let clock = ManualClockFn::new(START_MS);
    let mut ingestor = open_ingestor(&dir, clock.clock(), 2);
    let active = binding(PLUGIN_ID, 2);
    let retired = binding(PLUGIN_ID, 1);

    let retired_report = publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"value": "retired"}));
    let envelope = ingestor.publish(PLUGIN_ID, &retired, &retired_report).unwrap();
    assert_eq!(envelope.binding.generation, 1);
    assert!(ingestor.view(PLUGIN_ID).unwrap().entries.is_empty());

    ingestor
        .publish(PLUGIN_ID, &active, &publication(PLUGIN_ID, 2, 1, Durability::Durable, json!({"value": "active"})))
        .unwrap();
    assert_eq!(
        ingestor.view(PLUGIN_ID).unwrap().entries.get(SCHEMA_NAMESPACE).unwrap().payload,
        json!({"value": "active"})
    );
}

#[test]
fn unknown_schema_and_oversized_payload_are_refused_visibly() {
    let dir = fresh("hp5-refuse");
    let clock = ManualClockFn::new(START_MS);
    let mut ingestor = open_ingestor(&dir, clock.clock(), 1);
    let auth = binding(PLUGIN_ID, 1);

    let unknown = seal_publication(
        PLUGIN_ID,
        auth.clone(),
        "example.unknown",
        1,
        1,
        START_MS,
        Durability::Durable,
        json!({"ready": true}),
    )
    .unwrap();
    let err = ingestor.publish(PLUGIN_ID, &auth, &unknown).unwrap_err();
    assert_eq!(failure(&err), FailureCode::UnsupportedSchema);

    let oversized = json!({ "blob": "x".repeat((64 * 1024) + 100) });
    let publication = publication(PLUGIN_ID, 1, 1, Durability::Durable, oversized);
    let err = ingestor.publish(PLUGIN_ID, &auth, &publication).unwrap_err();
    assert!(matches!(
        err,
        ContractError::KnownFailure(_) | ContractError::UnsupportedCapability(_)
    ));
    assert!(ingestor.view(PLUGIN_ID).unwrap().entries.is_empty());
}

#[test]
fn restart_preserves_view_and_replay_identity() {
    let dir = fresh("hp5-restart");
    let clock = ManualClockFn::new(START_MS);
    let mut ingestor = open_ingestor(&dir, clock.clock(), 1);
    let auth = binding(PLUGIN_ID, 1);
    let first = publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"value": 1}));
    let original = ingestor.publish(PLUGIN_ID, &auth, &first).unwrap();
    ingestor
        .publish(PLUGIN_ID, &auth, &publication(PLUGIN_ID, 1, 2, Durability::Durable, json!({"value": 2})))
        .unwrap();
    drop(ingestor);

    let mut reopened = open_ingestor(&dir, clock.clock(), 1);
    let view = reopened.view(PLUGIN_ID).unwrap();
    assert_eq!(view.watermark, 2);
    assert_eq!(view.entries.get(SCHEMA_NAMESPACE).unwrap().payload, json!({"value": 2}));

    let replay = reopened.publish(PLUGIN_ID, &auth, &first).unwrap();
    assert_eq!(replay.observed_at_unix_ms, original.observed_at_unix_ms);
}

// ---------------------------------------------------------------------------
// Migration + complete-state over both ledger transports
// ---------------------------------------------------------------------------

#[test]
fn migration_and_complete_state_run_on_both_ledger_transports() {
    for transport in [LedgerTransport::InProcess, LedgerTransport::Process] {
        let label = transport.as_str();

        // --- explicit quiescent v4 -> v5 migration --------------------------
        let migrate_dir = fresh(&format!("hp5-migrate-{label}"));
        let db = migrate_dir.join("ledger.sqlite");
        write_v4_database(&db);
        match transport {
            LedgerTransport::InProcess => {
                let receipt = agalma_ledger::migrate_to_v5(&db).unwrap();
                assert_eq!((receipt.from_version, receipt.to_version), (4, 5));
                assert!(Path::new(&receipt.backup_path).is_file());
            }
            LedgerTransport::Process => {
                migrate_via_process(&migrate_dir);
            }
        }
        assert_eq!(backups_in(&migrate_dir).len(), 1, "{label}: one durable v4 backup");
        {
            let migrated = SqliteLedger::open(&db).unwrap();
            assert_eq!(migrated.schema_version().unwrap(), 5, "{label}");
            assert_eq!(migrated.migration_receipts().unwrap().len(), 1, "{label}");
            assert!(migrated.component_read("ns").unwrap().is_none());
        }

        // --- report ingest, complete-state export/import --------------------
        let source_dir = fresh(&format!("hp5-source-{label}"));
        let clock = ManualClockFn::new(START_MS);
        let exported_json;
        let original_first;
        {
            let mut ledger = handle_for(&source_dir, transport);
            let mut ingestor = {
                let sqlite = SqliteLedger::open(source_dir.join("ledger.sqlite")).unwrap();
                let generations =
                    [(PLUGIN_ID.to_string(), 1u64)].into_iter().collect::<BTreeMap<_, _>>();
                ReportIngestor::open(
                    Box::new(Mutex::new(sqlite)),
                    clock.clock(),
                    generations,
                    allowed_schemas(),
                )
                .unwrap()
            };
            let auth = binding(PLUGIN_ID, 1);
            let first = publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"value": 1}));
            original_first = ingestor.publish(PLUGIN_ID, &auth, &first).unwrap();
            ingestor
                .publish(PLUGIN_ID, &auth, &publication(PLUGIN_ID, 1, 2, Durability::Durable, json!({"value": 2})))
                .unwrap();
            assert_eq!(
                ingestor.view(PLUGIN_ID).unwrap().entries.get(SCHEMA_NAMESPACE).unwrap().payload,
                json!({"value": 2}),
                "{label}"
            );
            record_all_durable_sections(&mut ledger);

            let export_path = source_dir.join("complete-v2.json");
            ledger
                .export_complete_state_to(&export_path)
                .unwrap_or_else(|e| panic!("{label}: export complete state: {e}"));
            exported_json = std::fs::read_to_string(&export_path).unwrap();
            let parsed = CompleteStateV2::from_json(&exported_json).unwrap();
            parsed.validate_complete().unwrap();
            assert!(!parsed.decisions.is_empty(), "{label}: decisions");
            assert!(!parsed.decision_outcomes.is_empty(), "{label}: decision outcomes");
            assert!(!parsed.digests.is_empty(), "{label}: digests");
            assert!(!parsed.control_receipts.is_empty(), "{label}: control receipts");
            assert!(!parsed.component_states.is_empty(), "{label}: component states");
            assert_eq!(parsed.component_events.len(), 3, "{label}: events");
            assert_eq!(parsed.component_commits.len(), 3, "{label}: commit anchors");
            assert!(!parsed.component_operations.is_empty(), "{label}: operations");
            assert!(!parsed.component_receipts.is_empty(), "{label}: receipts");
        }

        let dest_dir = fresh(&format!("hp5-dest-{label}"));
        {
            let mut dest = handle_for(&dest_dir, transport);
            let import_path = dest_dir.join("complete-v2.json");
            std::fs::write(&import_path, &exported_json).unwrap();
            dest.import_complete_state_from(&import_path)
                .unwrap_or_else(|e| panic!("{label}: import complete state: {e}"));
            assert!(dest.import_complete_state_from(&import_path).is_err(), "{label}");

            let mut reopened = {
                let sqlite = SqliteLedger::open(dest_dir.join("ledger.sqlite")).unwrap();
                let generations =
                    [(PLUGIN_ID.to_string(), 1u64)].into_iter().collect::<BTreeMap<_, _>>();
                ReportIngestor::open(
                    Box::new(Mutex::new(sqlite)),
                    clock.clock(),
                    generations,
                    allowed_schemas(),
                )
                .unwrap()
            };
            let view = reopened.view(PLUGIN_ID).unwrap();
            assert_eq!(view.watermark, 2, "{label}");
            assert_eq!(
                view.entries.get(SCHEMA_NAMESPACE).unwrap().payload,
                json!({"value": 2}),
                "{label}"
            );
            let replay = reopened
                .publish(
                    PLUGIN_ID,
                    &binding(PLUGIN_ID, 1),
                    &publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"value": 1})),
                )
                .unwrap();
            assert_eq!(
                replay.observed_at_unix_ms, original_first.observed_at_unix_ms,
                "{label}: replay identity survives export/import"
            );
            let changed = publication(PLUGIN_ID, 1, 1, Durability::Durable, json!({"value": 9}));
            assert_eq!(
                failure(&reopened.publish(PLUGIN_ID, &binding(PLUGIN_ID, 1), &changed).unwrap_err()),
                FailureCode::IdentityConflict,
                "{label}"
            );
        }
    }
}

#[test]
fn normal_v4_open_does_not_migrate_and_explicit_migration_is_quiescent() {
    let dir = fresh("hp5-v4-no-upgrade");
    let db = dir.join("ledger.sqlite");
    write_v4_database(&db);

    {
        let ledger = SqliteLedger::open(&db).unwrap();
        assert_eq!(ledger.schema_version().unwrap(), 4);
        assert!(ledger.component_read("ns").is_err());
    }
    assert_eq!(SqliteLedger::open(&db).unwrap().schema_version().unwrap(), 4);

    let receipt = agalma_ledger::migrate_to_v5(&db).unwrap();
    assert_eq!(receipt.to_version, 5);
    assert!(Path::new(&receipt.backup_path).is_file());
    let mut migrated = SqliteLedger::open(&db).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 5);
    assert_eq!(
        migrated.migration_receipt(&receipt.migration_id).unwrap().unwrap(),
        receipt
    );
    migrated.component_commit(&component_commit()).unwrap();

    // Non-quiescent refusal: a live execution blocks migration.
    let dir2 = fresh("hp5-v4-nonquiescent");
    let db2 = dir2.join("ledger.sqlite");
    {
        let mut live = SqliteLedger::open(&db2).unwrap();
        live.commit(agalma_contracts::CommitBatch {
            expected_revision: None,
            state: Some(agalma_contracts::ExecutionRecord {
                execution_id: ExecutionId::new("exec:hp5:1"),
                task_id: agalma_contracts::TaskId::new("hp5"),
                generation: 1,
                phase: agalma_contracts::ExecutionPhase::Build,
                state: agalma_contracts::ExecutionState::Running,
                attempt: 1,
                revision: 0,
            }),
            events: vec![],
            receipts: vec![],
            intents: vec![],
        })
        .unwrap();
    }
    // Record v4 without dropping data: the v5 tables already exist, so just set
    // the version back and drop only the v5 tables.
    {
        let conn = rusqlite::Connection::open(&db2).unwrap();
        conn.execute_batch(
            "DROP TABLE IF EXISTS component_states; DROP TABLE IF EXISTS component_events; \
             DROP TABLE IF EXISTS component_operations; DROP TABLE IF EXISTS component_receipts; \
             DROP TABLE IF EXISTS component_commits; DROP TABLE IF EXISTS schema_migrations; \
             UPDATE meta SET value='4' WHERE key='schema_version';",
        )
        .unwrap();
    }
    let err = SqliteLedger::open(&db2).unwrap().migrate_to_v5().unwrap_err();
    assert!(err.to_string().contains("not quiescent"), "{err}");
}

fn component_commit() -> ComponentCommit {
    let mut commit = ComponentCommit {
        format: COMPONENT_COMMIT_FORMAT.to_string(),
        namespace: "example.fixture".to_string(),
        commit_id: "commit-1".to_string(),
        input_digest: String::new(),
        expected_revision: 0,
        checkpoint: Some(json!({"value": 1})),
        events: vec![],
        operations: vec![],
        receipts: vec![],
    };
    commit.input_digest = commit.computed_input_digest().unwrap();
    commit
}

fn component_commit_with_operation() -> ComponentCommit {
    let binding = binding(PLUGIN_ID, 1);
    let operation_id = "example.fixture/op-1".to_string();
    let input_digest = format!("sha256:{}", "1".repeat(64));
    let mut commit = ComponentCommit {
        format: COMPONENT_COMMIT_FORMAT.to_string(),
        namespace: "example.fixture".to_string(),
        commit_id: "commit-with-operation".to_string(),
        input_digest: String::new(),
        expected_revision: 0,
        checkpoint: Some(json!({"value": 1})),
        events: vec![ComponentEvent {
            event_id: "event-1".to_string(),
            kind: "fixture.updated".to_string(),
            payload: json!({"value": 1}),
            recorded_at_unix_ms: START_MS,
        }],
        operations: vec![ComponentOperation {
            operation_id: operation_id.clone(),
            input_digest: input_digest.clone(),
            binding: binding.clone(),
            intent: json!({"value": 1}),
            state: ComponentOperationState::Completed,
            deadline_unix_ms: START_MS + 1_000,
        }],
        receipts: vec![ComponentReceipt {
            operation_id,
            input_digest,
            binding,
            outcome: json!({"value": 1}),
            completed_at_unix_ms: START_MS + 1,
        }],
    };
    commit.input_digest = commit.computed_input_digest().unwrap();
    commit
}

fn record_all_durable_sections(ledger: &mut LedgerHandle) {
    let operation_id = OperationId::new("op:exec:hp5:1:act");
    let request = DecisionRequest {
        version: 1,
        operation_id: operation_id.clone(),
        kind: DecisionKind::TriagePickNext,
        context: json!({"prompt": "hp5"}),
        artifacts: vec![],
        options: vec![DecisionOption {
            option_id: "a".to_string(),
            attributes: json!({}),
        }],
        deadline_unix_ms: START_MS + 1_000,
        pin: DecisionPin {
            policy: "policy/v1".to_string(),
            genome: "genome/v1".to_string(),
            model: "static".to_string(),
        },
    };
    ledger.record_decision(&request).unwrap();
    ledger
        .record_decision_outcome(
            &operation_id,
            &DecisionOutcome::Applied {
                chosen: Some("a".to_string()),
                scores: vec![],
            },
        )
        .unwrap();
    ledger
        .record_digest(&DigestRecord {
            execution_id: ExecutionId::new("exec:hp5:1"),
            version: 1,
            summary: json!({"cost_nusd": 1}),
            artifact_ref: None,
            recorded_at_unix_ms: START_MS,
        })
        .unwrap();
    ledger
        .record_control_receipt(&ControlReceipt {
            action_id: "hp5-action-1".to_string(),
            action: "pause".to_string(),
            target: String::new(),
            actor: "human".to_string(),
            request: json!({"outcome": "applied"}),
            recorded_at_unix_ms: START_MS,
        })
        .unwrap();
    ledger.component_commit(&component_commit_with_operation()).unwrap();
}

// ---------------------------------------------------------------------------
// Assembled production host + production PublishReport RPC
// ---------------------------------------------------------------------------

async fn wait_for_socket(socket: &Path) {
    for _ in 0..500 {
        if socket.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("socket {} did not appear", socket.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembled_host_routes_production_publish_report_and_reconstructs_desired_state() {
    let scratch = fresh("hp5-host-rpc");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let package = package_fixture(&root);
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_for(&package));
    let config = host_config(&state_dir, &root);
    let clock = ManualClockFn::new(START_MS);
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    // Assemble, persist desired state, restart from the same selected ledger.
    let (socket, router, reports, generation, token, auth_binding) = {
        let mut host =
            PluginHost::assemble_with_ledger(&config, &installations, ledger.clone(), clock.clock())
                .expect("assembled production host");
        assert!(host.ledger().is_some());
        assert!(host.reports.is_some(), "report ingestor attached to the host");
        host.set_desired(PLUGIN_ID, DesiredState::Running).unwrap();
        drop(host);

        let mut restarted = PluginHost::assemble_with_ledger(
            &config,
            &installations,
            ledger.clone(),
            clock.clock(),
        )
        .expect("restarted production host");
        assert_eq!(
            restarted.registry.desired(PLUGIN_ID),
            DesiredState::Running,
            "desired state reconstructed from the selected ledger after restart"
        );
        let generation = *restarted.active_generations().get(PLUGIN_ID).unwrap();

        // `launch_request` issues the narrow per-launch `hub.PublishReport`
        // grant; use the credentials from the actual launch request.
        let launch = restarted
            .launch_request(PLUGIN_ID)
            .expect("launch issues report credentials");
        assert!(
            restarted.report_grant(PLUGIN_ID).is_some(),
            "a narrow report grant is issued for a report-declaring plugin"
        );
        let token = launch
            .env
            .get(PLUGIN_REPORT_GRANT_ENV)
            .cloned()
            .expect("launch request carries the PublishReport grant token");
        let auth_binding = restarted
            .report_grant(PLUGIN_ID)
            .expect("narrow report grant issued")
            .scope()
            .binding
            .clone();
        assert_eq!(auth_binding.generation, generation);
        let reports = restarted.reports.clone().unwrap();
        (
            restarted.hub_socket.clone(),
            restarted.router.clone(),
            reports,
            generation,
            token,
            auth_binding,
        )
    };

    // The durable registry checkpoint (selected-ledger component store) carries
    // the desired state and the active generation.
    let registry = ledger
        .component_read(REGISTRY_NAMESPACE)
        .unwrap()
        .expect("registry checkpoint");
    let checkpoint: RegistryCheckpoint =
        serde_json::from_value(registry.checkpoint.unwrap()).unwrap();
    assert_eq!(
        checkpoint.desired.get(PLUGIN_ID),
        Some(&DesiredState::Running)
    );
    assert!(checkpoint.active_generations.contains_key(PLUGIN_ID));

    // Serve the production hub capability socket and publish through the real RPC.
    let serve_socket = socket.clone();
    tokio::spawn(async move {
        let _ = agalma_conductor::plugin_router::serve_hub(&serve_socket, router).await;
    });
    wait_for_socket(&socket).await;

    // The publication carries the authenticated binding identity from the
    // actual launch grant.
    let publication = seal_publication(
        PLUGIN_ID,
        auth_binding,
        SCHEMA_NAMESPACE,
        SCHEMA_VERSION,
        1,
        START_MS,
        Durability::Durable,
        json!({"ready": true}),
    )
    .unwrap();
    let call = PublishReportCall {
        grant: token,
        plugin_id: PLUGIN_ID.to_string(),
        generation,
        publication,
    };
    let mut hub = HubCallbackClient::connect(&socket).await.expect("connect hub");
    let result = hub
        .publish_report(serde_json::to_value(&call).unwrap())
        .await
        .expect("authorized report routes over the production RPC");
    let envelope: ReportEnvelope = serde_json::from_value(result).unwrap();
    assert_eq!(envelope.plugin_id, PLUGIN_ID);
    assert_eq!(envelope.binding.generation, generation);

    // The attached ingestor observed it durably through the selected ledger.
    let ingestor = reports.lock().unwrap();
    assert_eq!(
        ingestor.view(PLUGIN_ID).unwrap().entries.get(SCHEMA_NAMESPACE).unwrap().payload,
        json!({"ready": true})
    );
    let rows = events(ingestor.store(), PLUGIN_ID);
    assert_eq!(rows.len(), 1, "durable observation stored through the ledger");
    assert_eq!(rows[0].event.kind, "report.observed");
}

/// The assembled host attached to the **process** ledger transport ingests a
/// report through the real RPC into the supervised server's component store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembled_host_over_process_ledger_routes_publish_report() {
    let scratch = fresh("hp5-host-process");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let package = package_fixture(&root);
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_for(&package));
    let config = host_config(&state_dir, &root);
    let clock = ManualClockFn::new(START_MS);

    // Selected LedgerApi over the supervised process transport.
    let ledger = LedgerHandle::process(&state_dir).expect("supervised ledger server");

    let (socket, router, reports, generation, token, auth_binding) = {
        let mut host =
            PluginHost::assemble_with_ledger(&config, &installations, ledger, clock.clock())
                .expect("assembled production host over the process ledger");
        let generation = *host.active_generations().get(PLUGIN_ID).unwrap();
        let launch = host.launch_request(PLUGIN_ID).expect("launch credentials");
        let token = launch.env.get(PLUGIN_REPORT_GRANT_ENV).cloned().unwrap();
        let auth_binding = host
            .report_grant(PLUGIN_ID)
            .expect("report grant")
            .scope()
            .binding
            .clone();
        let reports = host.reports.clone().unwrap();
        (
            host.hub_socket.clone(),
            host.router.clone(),
            reports,
            generation,
            token,
            auth_binding,
        )
    };

    let serve_socket = socket.clone();
    tokio::spawn(async move {
        let _ = agalma_conductor::plugin_router::serve_hub(&serve_socket, router).await;
    });
    wait_for_socket(&socket).await;

    let publication = seal_publication(
        PLUGIN_ID,
        auth_binding,
        SCHEMA_NAMESPACE,
        SCHEMA_VERSION,
        1,
        START_MS,
        Durability::Durable,
        json!({"ready": true}),
    )
    .unwrap();
    let call = PublishReportCall {
        grant: token,
        plugin_id: PLUGIN_ID.to_string(),
        generation,
        publication,
    };
    let mut hub = HubCallbackClient::connect(&socket).await.expect("connect hub");
    let result = hub
        .publish_report(serde_json::to_value(&call).unwrap())
        .await
        .expect("report routes over the process-backed ledger");
    let envelope: ReportEnvelope = serde_json::from_value(result).unwrap();
    assert_eq!(envelope.binding.generation, generation);

    let ingestor = reports.lock().unwrap();
    assert_eq!(
        ingestor.view(PLUGIN_ID).unwrap().entries.get(SCHEMA_NAMESPACE).unwrap().payload,
        json!({"ready": true})
    );
    let rows = events(ingestor.store(), PLUGIN_ID);
    assert_eq!(rows.len(), 1, "durable observation stored through the process ledger");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_publish_report_refuses_an_unauthorized_token_with_zero_effects() {    let scratch = fresh("hp5-host-deny");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let package = package_fixture(&root);
    let mut installations = InMemoryInstallations::new();
    installations.insert(installation_for(&package));
    let config = host_config(&state_dir, &root);
    let clock = ManualClockFn::new(START_MS);
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    let (socket, router, generation) = {
        let host =
            PluginHost::assemble_with_ledger(&config, &installations, ledger, clock.clock())
                .expect("assembled production host");
        let generation = *host.active_generations().get(PLUGIN_ID).unwrap();
        (host.hub_socket.clone(), host.router.clone(), generation)
    };
    let serve_socket = socket.clone();
    tokio::spawn(async move {
        let _ = agalma_conductor::plugin_router::serve_hub(&serve_socket, router).await;
    });
    wait_for_socket(&socket).await;

    let call = PublishReportCall {
        grant: "00".repeat(32),
        plugin_id: PLUGIN_ID.to_string(),
        generation,
        publication: publication(PLUGIN_ID, generation, 1, Durability::Durable, json!({"ready": true})),
    };
    let mut hub = HubCallbackClient::connect(&socket).await.expect("connect hub");
    let error = hub
        .publish_report(serde_json::to_value(&call).unwrap())
        .await
        .expect_err("an unknown grant is refused");
    assert!(
        format!("{error:?}").contains("AuthorityDenied"),
        "refusal carries an authority detail: {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Configured-root auto-install (agalma-jmr.5 repair)
// ---------------------------------------------------------------------------

/// A configured, user-controlled plugin root alone authorizes installation: the
/// package auto-installs from its immutable default configuration (no separate
/// approval record), and the pinned generation, desired state and ingested
/// report all survive a restart through the **selected** LedgerApi.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_root_auto_installs_defaults_and_resumes_through_selected_ledger() {
    let scratch = fresh("hp5-auto-install");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let package = package_fixture(&root);
    let config = host_config(&state_dir, &root);
    let clock = ManualClockFn::new(START_MS);
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    // No installation source is supplied: the configured root is the operator's
    // allowlist, so the package auto-installs from its declared defaults.
    let installations = InMemoryInstallations::new();

    let (generation, install_digest) = {
        let mut host = PluginHost::assemble_with_ledger(
            &config,
            &installations,
            ledger.clone(),
            clock.clock(),
        )
        .expect("auto-installed production host");
        assert!(
            host.diagnostics.is_empty(),
            "configured root auto-installs with no diagnostic: {:?}",
            host.diagnostics
        );
        assert!(host.reports.is_some(), "report ingestor attached to the host");

        let generation = *host
            .installed
            .get(PLUGIN_ID)
            .expect("configured root auto-installs a generation");
        let gen = host
            .registry
            .entry(PLUGIN_ID)
            .unwrap()
            .generations
            .get(&generation)
            .unwrap();
        // Package defaults are pinned: the frozen configuration ref, its digest
        // and the launch profile all come from the sealed manifest.
        assert_eq!(gen.configuration, package.manifest.default_configuration);
        assert_eq!(gen.config_digest, package.manifest.default_configuration.digest);
        assert_eq!(gen.launch_profile, package.manifest.entrypoint.launch_profile);
        assert_eq!(
            gen.approved_capabilities,
            vec![REPORT_CAPABILITY.to_string()],
            "automatic grant is requested ∩ the fixed safe allowlist"
        );
        let install_digest = gen.installation_digest.clone();

        host.set_desired(PLUGIN_ID, DesiredState::Running).unwrap();
        let _launch = host
            .launch_request(PLUGIN_ID)
            .expect("launch issues report credentials");
        let auth = host
            .report_grant(PLUGIN_ID)
            .expect("auto-installed report grant")
            .scope()
            .binding
            .clone();
        // Publish one durable observation through the host's selected-ledger
        // ingestor.
        let sealed = seal_publication(
            PLUGIN_ID,
            auth.clone(),
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            1,
            START_MS,
            Durability::Durable,
            json!({"ready": true}),
        )
        .unwrap();
        {
            let reports = host.reports.clone().unwrap();
            let mut ingestor = reports.lock().unwrap();
            ingestor.publish(PLUGIN_ID, &auth, &sealed).unwrap();
        }
        (generation, install_digest)
    };

    // Restart through the same selected ledger: the desired state, the registry
    // checkpoint, the pinned generation identity and the ingested report are all
    // reconstructed with no private-storage fallback.
    let mut restarted = PluginHost::assemble_with_ledger(
        &config,
        &installations,
        ledger.clone(),
        clock.clock(),
    )
    .expect("restarted auto-installed host");
    assert_eq!(
        restarted.registry.desired(PLUGIN_ID),
        DesiredState::Running,
        "desired state reconstructed from the selected ledger"
    );
    let restarted_generation = *restarted.installed.get(PLUGIN_ID).unwrap();
    let gen = restarted
        .registry
        .entry(PLUGIN_ID)
        .unwrap()
        .generations
        .get(&restarted_generation)
        .unwrap();
    assert_eq!(
        gen.installation_digest, install_digest,
        "the synthesized pin reconstructs with a stable identity across restart"
    );
    assert_eq!(restarted_generation, generation);

    // The report rehydrates from the selected ledger and its durable event is
    // still present.
    {
        let ingestor = restarted.reports.clone().unwrap();
        let ingestor = ingestor.lock().unwrap();
        assert_eq!(
            ingestor
                .view(PLUGIN_ID)
                .unwrap()
                .entries
                .get(SCHEMA_NAMESPACE)
                .unwrap()
                .payload,
            json!({"ready": true})
        );
        assert_eq!(events(ingestor.store(), PLUGIN_ID).len(), 1);
    }

    // The durable registry checkpoint carries the desired state and generation.
    let registry = ledger
        .component_read(REGISTRY_NAMESPACE)
        .unwrap()
        .expect("registry checkpoint");
    let checkpoint: RegistryCheckpoint =
        serde_json::from_value(registry.checkpoint.unwrap()).unwrap();
    assert_eq!(
        checkpoint.desired.get(PLUGIN_ID),
        Some(&DesiredState::Running)
    );
    assert!(checkpoint.active_generations.contains_key(PLUGIN_ID));

    let _ = restarted.shutdown();
}

/// Auto-install grants are exactly `requested ∩ {report.publish, ledger.read}`:
/// `ledger.write` and any unknown capability the package requests are denied,
/// while the immutable package default is pinned and reconstructible.
#[test]
fn auto_install_denies_ledger_write_and_unknown_capabilities_and_pins_defaults() {
    let scratch = fresh("hp5-auto-deny");
    let state_dir = scratch.child("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = scratch.child("root");
    let package = package_fixture_caps(
        &root,
        &[REPORT_CAPABILITY, "ledger.write", "mystery.widgets"],
    );
    let config = host_config(&state_dir, &root);
    let clock = ManualClockFn::new(START_MS);
    let ledger = LedgerHandle::in_process(&state_dir).unwrap();

    let host = PluginHost::assemble_with_ledger(
        &config,
        &InMemoryInstallations::new(),
        ledger,
        clock.clock(),
    )
    .expect("auto-installed production host");
    let generation = *host.installed.get(PLUGIN_ID).expect("auto-installed");
    let gen = host
        .registry
        .entry(PLUGIN_ID)
        .unwrap()
        .generations
        .get(&generation)
        .unwrap();
    assert_eq!(
        gen.requested_capabilities,
        vec![
            REPORT_CAPABILITY.to_string(),
            "ledger.write".to_string(),
            "mystery.widgets".to_string(),
        ],
        "the package still requests the denied capabilities"
    );
    assert_eq!(
        gen.approved_capabilities,
        vec![REPORT_CAPABILITY.to_string()],
        "only the allowlisted capability is granted"
    );
    assert!(!gen.approved_capabilities.iter().any(|c| c == "ledger.write"));
    assert!(!gen.approved_capabilities.iter().any(|c| c == "mystery.widgets"));

    // Package defaults are pinned in the installed generation.
    assert_eq!(gen.configuration, package.manifest.default_configuration);
    assert_eq!(gen.config_digest, package.manifest.default_configuration.digest);

    // The synthesized pin is a deterministic, reconstructible function of the
    // sealed package.
    let first = default_installation(&package).unwrap();
    let second = default_installation(&package).unwrap();
    assert_eq!(first, second, "default pin is deterministic");
    assert_eq!(
        first.installation_digest, gen.installation_digest,
        "the installed generation carries the synthesized pin digest"
    );
    assert_eq!(first.format, "PluginInstallation/v1".to_string());
    assert_eq!(
        first.approved_capabilities,
        vec![REPORT_CAPABILITY.to_string()]
    );
}

// ---------------------------------------------------------------------------
// Deterministic property test
// ---------------------------------------------------------------------------

#[test]
fn property_report_sequences_preserve_replay_gap_and_restart_invariants() {
    let mut rng = XorShift64::new(PROPERTY_SEED);
    for case in 0..PROPERTY_CASES {
        let dir = fresh(&format!("hp5-prop-{case}"));
        let clock = ManualClockFn::new(START_MS);
        let generation = 1 + rng.below(3);
        let mut ingestor = open_ingestor(&dir, clock.clock(), generation);
        let auth = binding(PLUGIN_ID, generation);

        let mut seen: BTreeMap<u64, (String, u64)> = BTreeMap::new();
        let mut durable: Vec<(u64, String)> = Vec::new();
        let steps = 1 + rng.below(12);
        for _ in 0..steps {
            let sequence = 1 + rng.below(8);
            let value = rng.below(4);
            let payload = json!({"ready": true, "value": value});
            let pubn =
                publication(PLUGIN_ID, generation, sequence, Durability::Durable, payload);
            let digest = pubn.payload_digest.clone();
            match seen.get(&sequence) {
                Some((seen_digest, seen_time)) if *seen_digest == digest => {
                    let replay = ingestor
                        .publish(PLUGIN_ID, &auth, &pubn)
                        .unwrap_or_else(|e| panic!("case {case} sequence {sequence}: replay: {e}"));
                    assert_eq!(
                        &replay.observed_at_unix_ms, seen_time,
                        "case {case} sequence {sequence}: replay returns the original observation"
                    );
                }
                Some((seen_digest, _)) => {
                    assert_ne!(seen_digest, &digest);
                    let err = ingestor.publish(PLUGIN_ID, &auth, &pubn).unwrap_err();
                    assert_eq!(
                        failure(&err),
                        FailureCode::IdentityConflict,
                        "case {case} sequence {sequence}: changed content"
                    );
                }
                None => {
                    let envelope = ingestor
                        .publish(PLUGIN_ID, &auth, &pubn)
                        .unwrap_or_else(|e| panic!("case {case} sequence {sequence}: {e}"));
                    seen.insert(sequence, (digest.clone(), envelope.observed_at_unix_ms));
                    durable.push((sequence, digest));
                }
            }
        }

        let view = ingestor.view(PLUGIN_ID).unwrap();
        assert_eq!(
            view.incomplete,
            !view.queued.is_empty(),
            "case {case}: incomplete view iff queued reports remain"
        );
        assert!(view.watermark <= 8);

        drop(ingestor);
        let reopened = open_ingestor(&dir, clock.clock(), generation);
        for (sequence, digest) in durable {
            if let Some(envelope) = reopened
                .view(PLUGIN_ID)
                .unwrap()
                .envelopes
                .get(&(generation, sequence))
            {
                assert_eq!(
                    envelope.payload_digest, digest,
                    "case {case} sequence {sequence}: durable identity survives restart"
                );
            }
        }
    }
}

/// The automatic grant is exactly `requested ∩ {report.publish, ledger.read}`,
/// over deterministic generated capability sets that include duplicates, the
/// empty string, unknown capabilities and the explicitly denied `ledger.write`.
///
/// The oracle is defined independently of the implementation: for each generated
/// request it recomputes the deduplicated intersection with the fixed safe
/// allowlist and asserts equality, containment and denial.
#[test]
fn property_auto_grants_never_exceed_requested_intersection_allowlist() {
    const ALLOW: [&str; 2] = ["report.publish", "ledger.read"];
    const ALPHABET: [&str; 8] = [
        "report.publish",
        "ledger.read",
        "ledger.write",
        "",
        "mystery.widgets",
        "report.write",
        "ledger.admin",
        "report.publish",
    ];
    // The fixed allowlist the implementation must honor (spec, not oracle).
    assert_eq!(SAFE_CAPABILITY_ALLOWLIST, &ALLOW[..]);

    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x00c0_ffee);
    for case in 0..PROPERTY_CASES {
        let len = rng.below(10) as usize;
        let mut requested: Vec<String> = Vec::with_capacity(len);
        for _ in 0..len {
            requested.push(ALPHABET[rng.below(ALPHABET.len() as u64) as usize].to_string());
        }
        let approved = auto_approved_capabilities(&requested);

        // Independent oracle: first-occurrence dedup intersection with ALLOW.
        let mut expected: Vec<String> = Vec::new();
        for capability in &requested {
            if !ALLOW.contains(&capability.as_str()) {
                continue;
            }
            if expected.iter().any(|seen| seen == capability) {
                continue;
            }
            expected.push(capability.clone());
        }
        assert_eq!(
            approved, expected,
            "case {case}: grants == requested ∩ allowlist"
        );

        // Grants never exceed requested ∩ allowlist and never leak a denial.
        assert!(approved.len() <= requested.len());
        let mut deduped = approved.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), approved.len(), "case {case}: duplicate-free");
        for granted in &approved {
            assert!(
                requested.iter().any(|r| r == granted),
                "case {case}: granted {granted:?} was not requested"
            );
            assert!(
                ALLOW.contains(&granted.as_str()),
                "case {case}: granted {granted:?} outside the safe allowlist"
            );
            assert_ne!(granted, "ledger.write", "case {case}: ledger.write denied");
            assert!(!granted.is_empty(), "case {case}: empty capability denied");
        }
    }

    // Concrete denial regression: `ledger.write` is requested but never granted;
    // unknown capabilities and the empty string are never granted either.
    assert_eq!(
        auto_approved_capabilities(&["ledger.write".to_string(), "report.publish".to_string()]),
        vec!["report.publish".to_string()]
    );
    assert!(auto_approved_capabilities(&["ledger.write".to_string()]).is_empty());
    assert!(auto_approved_capabilities(&["".to_string(), "mystery".to_string()]).is_empty());
    assert!(auto_approved_capabilities(&[]).is_empty());
}
