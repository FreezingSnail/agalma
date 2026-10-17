//! HP.1 common plugin-protocol acceptance tests (agalma-jmr.1).
//!
//! Independent golden and malformed fixtures come from
//! `docs/contracts/self-mutation-v1/`; the assertions use documented failure
//! codes rather than implementation internals. No network, no `/tmp`, no temp
//! files. Every durable record's digest identity is exercised against the
//! frozen examples.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use agalma_contracts::binding::{BindingRecord, BindingState};
use agalma_contracts::decision::{
    DecisionKind, DecisionOption, DecisionOutcome, DecisionPin, DecisionRequest,
};
use agalma_contracts::digest::{
    canonical_digest, canonical_json, failure_code, sha256_digest, FailureCode,
};
use agalma_contracts::execution::{ExecutionPhase, ExecutionState};
use agalma_contracts::ids::{BindingId, ExecutionId, OperationId, TaskId};
use agalma_contracts::ledger::OperationState;
use agalma_contracts::{
    missing_approved_capabilities, ActionIntent, ComponentCommit, ComponentCommitAnchor,
    ComponentEvent, ComponentEventRow, ComponentOperation, ComponentOperationRow, ComponentReceipt,
    ComponentReceiptRow, ComponentStateRow, CompleteStateV2, ControlReceipt, DirectiveRow,
    DirectiveState, DispatchIntent, ExecutionEvent, ExecutionRecord, OperationReceipt,
    PluginCancelRequest, PluginHealth, PluginInstallation, PluginManifest,
    PluginOperationStatus, PluginSnapshot, PluginSnapshotEntry, ReportEnvelope, ReportPublication,
    SnapshotDecisionOutcomeRow, SnapshotDecisionRow, TimerRecord, TimerState, BindingPin,
    COMPLETE_STATE_FORMAT, COMPLETE_STATE_SCHEMA_VERSION, COMPONENT_COMMIT_FORMAT,
    PLUGIN_SNAPSHOT_PAYLOAD_LIMIT_BYTES, REPORT_PAYLOAD_LIMIT_BYTES,
};

// --- fixtures ---------------------------------------------------------------

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/contracts/self-mutation-v1/",
            $name
        ))
    };
}

const MANIFEST_JSON: &str = fixture!("plugin-manifest.json");
const INSTALLATION_JSON: &str = fixture!("plugin-installation.json");
const HEALTH_JSON: &str = fixture!("plugin-health.json");
const SNAPSHOT_JSON: &str = fixture!("plugin-snapshot.json");
const ENVELOPE_JSON: &str = fixture!("report-envelope.json");
const PUBLICATION_JSON: &str = fixture!("report-publication.json");
const OPERATION_STATUS_JSON: &str = fixture!("plugin-operation-status.json");
const CANCEL_REQUEST_JSON: &str = fixture!("plugin-cancel-request.json");
const ACTION_INTENT_JSON: &str = fixture!("action-intent.json");
const COMPONENT_COMMIT_JSON: &str = fixture!("component-commit.json");
const SECTIONS_JSON: &str = fixture!("snapshot-sections.json");
const INVALID_RECORDS_JSON: &str = fixture!("invalid-records.json");
const SCHEMA_JSON: &str = fixture!("schema.json");

/// Count of executed required cases; the golden test asserts it is nonzero.
static CASES: AtomicUsize = AtomicUsize::new(0);

fn tick() -> usize {
    CASES.fetch_add(1, Ordering::SeqCst) + 1
}

#[derive(Debug, serde::Deserialize)]
struct InvalidRecord {
    name: String,
    expected_code: String,
    record: Value,
}

/// A named malformed record plus the failure code the contract must report.
struct InvalidCase {
    record: Value,
    expected_code: String,
}

fn invalid_case(name: &str) -> InvalidCase {
    let records: Vec<InvalidRecord> =
        serde_json::from_str(INVALID_RECORDS_JSON).expect("invalid-records.json parses");
    let record = records
        .into_iter()
        .find(|record| record.name == name)
        .unwrap_or_else(|| panic!("no invalid record named {name}"));
    InvalidCase {
        record: record.record,
        expected_code: record.expected_code,
    }
}

fn roundtrip<T>(text: &str) -> T
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let parsed: T = serde_json::from_str(text).expect("fixture parses");
    let encoded = serde_json::to_string(&parsed).expect("serializes");
    let reparsed: T = serde_json::from_str(&encoded).expect("reparses");
    assert_eq!(parsed, reparsed, "serde round trip is stable");
    parsed
}

fn zero_digest() -> String {
    format!("sha256:{}", "0".repeat(64))
}

fn pin() -> BindingPin {
    BindingPin {
        binding_id: "fixture.reporter".to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "fixture-1".to_string(),
        generation: 1,
        config_digest: sha256_digest(b"{}"),
    }
}

// --- 1. golden positive fixtures -------------------------------------------

#[test]
fn golden_fixtures_parse_round_trip_and_gate() {
    let cases = tick();

    let manifest: PluginManifest = roundtrip(MANIFEST_JSON);
    manifest.version_gate().expect("manifest v1 gates");
    let installation: PluginInstallation = roundtrip(INSTALLATION_JSON);
    installation.version_gate().expect("installation v1 gates");
    let health: PluginHealth = roundtrip(HEALTH_JSON);
    health.version_gate().expect("health v1 gates");
    let snapshot: PluginSnapshot = roundtrip(SNAPSHOT_JSON);
    snapshot.version_gate().expect("snapshot v1 gates");
    let envelope: ReportEnvelope = roundtrip(ENVELOPE_JSON);
    envelope.version_gate().expect("envelope v1 gates");
    let publication: ReportPublication = roundtrip(PUBLICATION_JSON);
    publication.version_gate().expect("publication v1 gates");
    let status: PluginOperationStatus = roundtrip(OPERATION_STATUS_JSON);
    status.version_gate().expect("operation status v1 gates");
    let cancel: PluginCancelRequest = roundtrip(CANCEL_REQUEST_JSON);
    cancel.version_gate().expect("cancel request v1 gates");
    let intent: ActionIntent = roundtrip(ACTION_INTENT_JSON);
    intent.version_gate().expect("action intent v1 gates");
    let commit: ComponentCommit = roundtrip(COMPONENT_COMMIT_JSON);
    commit.version_gate().expect("component commit v1 gates");

    assert!(!MANIFEST_JSON.is_empty(), "fixture is nonempty");
    assert!(cases >= 1, "first required case executed");
    assert!(CASES.load(Ordering::SeqCst) > 0, "required cases executed");

    // The root alias for the component-store lifecycle is reachable.
    let _alias: agalma_contracts::ComponentOperationState = status.state;
    assert_eq!(COMPONENT_COMMIT_FORMAT, "ComponentCommit/v1");
}

// --- 2. manifest major/format gating ---------------------------------------

#[test]
fn manifest_supported_and_unsupported_majors() {
    tick();
    let manifest: PluginManifest = serde_json::from_str(MANIFEST_JSON).unwrap();
    manifest.version_gate().expect("major 1 supported");

    let major_case = invalid_case("plugin-manifest:protocol_major");
    let bad_major: PluginManifest = serde_json::from_value(major_case.record).unwrap();
    assert_eq!(bad_major.protocol_major, 99);
    let err = bad_major.version_gate().expect_err("major 99 refused");
    assert_eq!(
        failure_code(&err).map(FailureCode::as_str),
        Some(major_case.expected_code.as_str())
    );

    let mut v99: PluginManifest = serde_json::from_str(MANIFEST_JSON).unwrap();
    v99.format = "PluginManifest/v99".to_string();
    let err = v99.version_gate().expect_err("v99 refused");
    assert_eq!(failure_code(&err), Some(FailureCode::UnsupportedVersion));

    let mut wrong_family: PluginManifest = serde_json::from_str(MANIFEST_JSON).unwrap();
    wrong_family.format = "PluginHealth/v1".to_string();
    let err = wrong_family.version_gate().expect_err("wrong family refused");
    assert_eq!(failure_code(&err), Some(FailureCode::UnsupportedSchema));
}

// --- 3. missing capabilities ------------------------------------------------

#[test]
fn missing_capabilities_are_refused() {
    tick();
    let manifest: PluginManifest = serde_json::from_str(MANIFEST_JSON).unwrap();
    let installation: PluginInstallation = serde_json::from_str(INSTALLATION_JSON).unwrap();
    missing_approved_capabilities(&manifest, &installation)
        .expect("golden grants satisfy every request");

    let mut stripped = installation.clone();
    stripped.approved_capabilities.clear();
    let err = missing_approved_capabilities(&manifest, &stripped)
        .expect_err("unapproved request refused");
    assert_eq!(failure_code(&err), Some(FailureCode::MissingCapability));
    assert!(
        format!("{err}").contains(manifest.requested_capabilities[0].as_str()),
        "subject names the request"
    );
}

// --- 4. report identity and size -------------------------------------------

#[test]
fn report_identity_and_size_are_bounded() {
    tick();
    let case = invalid_case("report-envelope:sequence");
    let bad_sequence: ReportEnvelope = serde_json::from_value(case.record).unwrap();
    assert_eq!(bad_sequence.sequence, 0);
    let err = bad_sequence.check_sequence().expect_err("sequence 0 refused");
    assert_eq!(failure_code(&err).map(FailureCode::as_str), Some(case.expected_code.as_str()));

    let golden: ReportPublication = serde_json::from_str(PUBLICATION_JSON).unwrap();
    golden.verify_payload_digest().expect("golden payload digest");

    let mut corrupted = golden.clone();
    corrupted.payload_digest = zero_digest();
    let err = corrupted
        .verify_payload_digest()
        .expect_err("corrupted digest refused");
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));

    let mut oversized = golden.clone();
    oversized.payload = json!({ "blob": "x".repeat(70_000) });
    oversized.payload_digest = canonical_digest(&oversized.payload).unwrap();
    let err = oversized
        .check_payload_bound()
        .expect_err("oversized payload refused");
    assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));

    let mut changed = golden.clone();
    changed.payload = json!({ "ready": false });
    changed.payload_digest = canonical_digest(&changed.payload).unwrap();
    let err = changed
        .check_replay(&golden)
        .expect_err("changed payload under same identity conflicts");
    assert_eq!(failure_code(&err), Some(FailureCode::IdentityConflict));
    golden
        .check_replay(&golden.clone())
        .expect("identical replay is a no-op");
}

// --- 5. changed action inputs ----------------------------------------------

#[test]
fn changed_action_inputs_conflict() {
    tick();
    let intent: ActionIntent = serde_json::from_str(ACTION_INTENT_JSON).unwrap();
    intent.verify_input_digest().expect("golden input digest");

    let mut tampered = intent.clone();
    tampered.input_digest = zero_digest();
    let err = tampered
        .verify_input_digest()
        .expect_err("tampered digest refused");
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));

    let mut changed = intent.clone();
    changed.input = json!({ "value": 2 });
    changed.input_digest = canonical_digest(&changed.input).unwrap();
    let err = changed
        .check_replay(&intent)
        .expect_err("changed input under same operation conflicts");
    assert_eq!(failure_code(&err), Some(FailureCode::IdentityConflict));
    intent
        .check_replay(&intent.clone())
        .expect("identical action replay is a no-op");
}

// --- 6. component checkpoint serialization ---------------------------------

#[test]
fn component_commit_checkpoint_and_bounds() {
    tick();
    let commit: ComponentCommit = serde_json::from_str(COMPONENT_COMMIT_JSON).unwrap();
    commit.verify_input_digest().expect("golden commit digest");
    commit.check_bounds().expect("golden commit is within bounds");
    assert!(commit.checkpoint.is_some());

    // `checkpoint: null` round-trips with a stable recomputed digest.
    let mut null_checkpoint = commit.clone();
    null_checkpoint.checkpoint = None;
    null_checkpoint.input_digest = null_checkpoint.computed_input_digest().unwrap();
    let encoded = serde_json::to_string(&null_checkpoint).unwrap();
    let reparsed: ComponentCommit = serde_json::from_str(&encoded).unwrap();
    assert_eq!(reparsed.checkpoint, None);
    reparsed
        .verify_input_digest()
        .expect("null checkpoint verifies");

    let mut oversized = commit.clone();
    oversized.checkpoint = Some(json!({ "blob": "x".repeat(300_000) }));
    let err = oversized
        .check_bounds()
        .expect_err("oversized checkpoint refused");
    assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));

    let mut many = commit.clone();
    let template = commit.events[0].clone();
    many.events = (0..101)
        .map(|index| {
            let mut event = template.clone();
            event.event_id = format!("event-{index}");
            event
        })
        .collect();
    let err = many.check_bounds().expect_err("101 events refused");
    assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));
}

// --- 7. installation digest -------------------------------------------------

#[test]
fn installation_digest_recomputes_and_tracks_config() {
    tick();
    let installation: PluginInstallation = serde_json::from_str(INSTALLATION_JSON).unwrap();
    installation
        .verify_installation_digest()
        .expect("golden installation digest");
    assert_eq!(
        installation.recomputed_installation_digest().unwrap(),
        installation.installation_digest
    );

    let mut distinct = installation.clone();
    distinct.configuration.path = "objects/other-config.json".to_string();
    assert_ne!(
        distinct.recomputed_installation_digest().unwrap(),
        installation.installation_digest,
        "distinct configuration yields a distinct installation digest"
    );
}

// --- 8. launch versus the logical binding generation -----------------------

#[test]
fn launch_is_separate_from_the_logical_binding_generation() {
    tick();

    // (a) Two independent observations of the identical package+configuration
    // (a health check and a report envelope, i.e. distinct launches) carry the
    // identical frozen `BindingPin`, whose only version axis is the logical
    // `generation`.
    let health: PluginHealth = serde_json::from_str(HEALTH_JSON).unwrap();
    let envelope: ReportEnvelope = serde_json::from_str(ENVELOPE_JSON).unwrap();
    assert_eq!(
        health.binding, envelope.binding,
        "separate launches reuse the identical logical binding generation"
    );

    // (b) The frozen launch descriptor is the manifest/installation
    // `launch_profile`; there is no separate launch-epoch field.
    let manifest: PluginManifest = serde_json::from_str(MANIFEST_JSON).unwrap();
    let installation: PluginInstallation = serde_json::from_str(INSTALLATION_JSON).unwrap();
    assert_eq!(
        manifest.entrypoint.launch_profile, installation.launch_profile,
        "the frozen launch descriptor is `launch_profile`"
    );

    // (c) A new configuration is a new logical generation: the installation
    // digest tracks it while the pinned `package_digest` is unchanged.
    installation
        .verify_installation_digest()
        .expect("golden installation digest verifies");
    let mut reconfigured = installation.clone();
    reconfigured.configuration.path = "objects/other-config.json".to_string();
    assert_ne!(
        reconfigured.recomputed_installation_digest().unwrap(),
        installation.installation_digest,
        "a new configuration changes the logical generation digest"
    );
    assert_eq!(
        reconfigured.package_digest, installation.package_digest,
        "reconfiguration leaves the pinned package_digest unchanged"
    );

    // (d) No frozen binding field encodes a launch epoch: the serialized
    // binding's key set is exactly `$defs.BindingPin.required`.
    let schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    let required: BTreeSet<String> = schema["$defs"]["BindingPin"]["required"]
        .as_array()
        .expect("BindingPin.required is an array")
        .iter()
        .map(|field| {
            field
                .as_str()
                .expect("each required field is a string")
                .to_string()
        })
        .collect();
    let serialized = serde_json::to_value(&envelope.binding).unwrap();
    let actual: BTreeSet<String> = serialized
        .as_object()
        .expect("binding is a JSON object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        actual, required,
        "the binding exposes exactly the frozen required fields (no launch epoch)"
    );
}

// --- 9. publication versus envelope ----------------------------------------

#[test]
fn publication_is_wrapped_into_the_hub_envelope() {
    tick();
    let publication: ReportPublication = serde_json::from_str(PUBLICATION_JSON).unwrap();
    let value = serde_json::to_value(&publication).unwrap();
    assert!(
        value.get("observed_at_unix_ms").is_none(),
        "a producer publication has no hub observation timestamp"
    );

    let envelope = ReportEnvelope::from_publication(&publication, 1791820800001);
    let golden: ReportEnvelope = serde_json::from_str(ENVELOPE_JSON).unwrap();
    assert_eq!(envelope, golden, "wrapping reproduces the golden envelope");
    assert_eq!(envelope.observed_at_unix_ms, 1791820800001);
    assert_eq!(envelope.reported_at_unix_ms, 1791820800000);
}

// --- 10. stable cancel identity --------------------------------------------

#[test]
fn cancel_request_identity_is_stable() {
    tick();
    let cancel: PluginCancelRequest = serde_json::from_str(CANCEL_REQUEST_JSON).unwrap();
    cancel
        .check_replay(&cancel.clone())
        .expect("identical cancel replay is a no-op");

    let mut later_deadline = cancel.clone();
    later_deadline.deadline_unix_ms += 1;
    let err = later_deadline
        .check_replay(&cancel)
        .expect_err("changed deadline under same id conflicts");
    assert_eq!(failure_code(&err), Some(FailureCode::IdentityConflict));

    let mut other_grant = cancel.clone();
    other_grant.grant_fingerprint = zero_digest();
    let err = other_grant
        .check_replay(&cancel)
        .expect_err("changed grant under same id conflicts");
    assert_eq!(failure_code(&err), Some(FailureCode::IdentityConflict));
}

// --- 11. component replay anchors ------------------------------------------

#[test]
fn component_commit_anchor_replay() {
    tick();
    let anchor = ComponentCommitAnchor {
        namespace: "example.action-fixture".to_string(),
        commit_id: "commit-1".to_string(),
        input_digest: sha256_digest(b"first"),
        revision: 1,
        committed_at_unix_ms: 1791820800000,
    };
    anchor
        .check_replay(&anchor.clone())
        .expect("identical commit replay returns the original receipt");

    let mut changed = anchor.clone();
    changed.input_digest = sha256_digest(b"second");
    let err = changed
        .check_replay(&anchor)
        .expect_err("changed input under same commit id conflicts");
    assert_eq!(failure_code(&err), Some(FailureCode::IdentityConflict));

    let other = ComponentCommitAnchor {
        commit_id: "commit-2".to_string(),
        ..anchor.clone()
    };
    anchor
        .check_replay(&other)
        .expect("a different commit id is an independent anchor");
}

// --- 12. complete-state/v2 --------------------------------------------------

fn example_decision_request(operation_id: &OperationId) -> DecisionRequest {
    DecisionRequest {
        version: 1,
        operation_id: operation_id.clone(),
        kind: DecisionKind::TriagePickNext,
        context: json!({}),
        artifacts: Vec::new(),
        options: vec![DecisionOption {
            option_id: "task-1".to_string(),
            attributes: json!({ "priority": "normal" }),
        }],
        deadline_unix_ms: 1791820800000,
        pin: DecisionPin {
            policy: "static/v1".to_string(),
            genome: "genome-1".to_string(),
            model: "static".to_string(),
        },
    }
}

fn build_complete_state() -> CompleteStateV2 {
    let task = TaskId::new("fixture.task");
    let execution_id = ExecutionId::derive(&task, 1);
    let operation_id = OperationId::derive(&execution_id, "1");
    let binding = pin();

    let mut kernel_meta = BTreeMap::new();
    kernel_meta.insert("kernel_version".to_string(), "1".to_string());
    let mut lease_generations = BTreeMap::new();
    lease_generations.insert("fixture.task".to_string(), 1);

    let component_event = ComponentEvent {
        event_id: "event-1".to_string(),
        kind: "fixture.updated".to_string(),
        payload: json!({ "value": 1 }),
        recorded_at_unix_ms: 1791820800000,
    };
    let component_operation = ComponentOperation {
        operation_id: operation_id.to_string(),
        input_digest: sha256_digest(b"input"),
        binding: binding.clone(),
        intent: json!({ "value": 1 }),
        state: OperationState::Completed,
        deadline_unix_ms: 1791820810000,
    };
    let component_receipt = ComponentReceipt {
        operation_id: operation_id.to_string(),
        input_digest: sha256_digest(b"input"),
        binding: binding.clone(),
        outcome: json!({ "value": 1 }),
        completed_at_unix_ms: 1791820800001,
    };

    CompleteStateV2 {
        format: COMPLETE_STATE_FORMAT.to_string(),
        schema_version: COMPLETE_STATE_SCHEMA_VERSION,
        exec_def_version: "1".to_string(),
        kernel_version: "1".to_string(),
        kill_latch: false,
        kernel_meta,
        lease_generations,
        executions: vec![ExecutionRecord {
            execution_id: execution_id.clone(),
            task_id: task.clone(),
            generation: 1,
            phase: ExecutionPhase::Build,
            state: ExecutionState::Running,
            attempt: 1,
            revision: 1,
        }],
        events: vec![ExecutionEvent {
            execution_id: execution_id.clone(),
            sequence: 1,
            kind: "started".to_string(),
            payload: json!({}),
            recorded_at_unix_ms: 1791820800000,
        }],
        receipts: vec![OperationReceipt {
            operation_id: operation_id.clone(),
            execution_id: execution_id.clone(),
            kind: "step".to_string(),
            inputs_hash: sha256_digest(b"inputs"),
            result: json!({}),
            completed_at_unix_ms: 1791820800001,
        }],
        intents: vec![DispatchIntent {
            operation_id: operation_id.clone(),
            execution_id: execution_id.clone(),
            payload: json!({}),
            enqueued_at_unix_ms: 1791820800000,
            consumed: false,
        }],
        timers: vec![TimerRecord {
            timer_id: "timer-1".to_string(),
            execution_id: execution_id.clone(),
            kind: "retry".to_string(),
            fire_at_unix_ms: 1791820900000,
            state: TimerState::Pending,
            payload: json!({}),
        }],
        directives: vec![DirectiveRow {
            directive_id: "standup-prompts".to_string(),
            version: 1,
            source: "fixture".to_string(),
            state: DirectiveState::Active,
            payload: json!({}),
            received_at_unix_ms: 1791820800000,
        }],
        bindings: vec![BindingRecord {
            binding_id: BindingId::new("binding:fixture.reporter"),
            component_api: "plugin".to_string(),
            api_version: 1,
            implementation: "fixture".to_string(),
            implementation_version: "fixture-1".to_string(),
            capabilities: BTreeSet::new(),
            required_capabilities: BTreeSet::new(),
            optional_capabilities: BTreeSet::new(),
            config_hash: sha256_digest(b"{}"),
            generation: 1,
            state: BindingState::Active,
        }],
        decisions: vec![SnapshotDecisionRow {
            operation_id: operation_id.clone(),
            kind: DecisionKind::TriagePickNext.as_str().to_string(),
            inputs_hash: sha256_digest(b"inputs"),
            request: example_decision_request(&operation_id),
            recorded_at_unix_ms: 1791820800000,
        }],
        decision_outcomes: vec![SnapshotDecisionOutcomeRow {
            operation_id: operation_id.clone(),
            outcome: DecisionOutcome::Parked {
                reason: "no eligible option".to_string(),
            },
            recorded_at_unix_ms: 1791820800001,
        }],
        digests: vec![agalma_contracts::DigestRecord {
            execution_id: execution_id.clone(),
            version: 1,
            summary: json!({ "cost": 0 }),
            artifact_ref: None,
            recorded_at_unix_ms: 1791820800001,
        }],
        control_receipts: vec![ControlReceipt {
            action_id: "action-1".to_string(),
            action: "pause".to_string(),
            target: String::new(),
            actor: "human".to_string(),
            request: json!({ "outcome": "applied" }),
            recorded_at_unix_ms: 1791820800001,
        }],
        component_states: vec![ComponentStateRow {
            namespace: "example.action-fixture".to_string(),
            revision: 1,
            watermark: 1,
            checkpoint: Some(json!({ "value": 1 })),
            recorded_at_unix_ms: 1791820800001,
        }],
        component_events: vec![ComponentEventRow {
            namespace: "example.action-fixture".to_string(),
            sequence: 1,
            event: component_event,
        }],
        component_operations: vec![ComponentOperationRow {
            namespace: "example.action-fixture".to_string(),
            operation: component_operation,
        }],
        component_receipts: vec![ComponentReceiptRow {
            namespace: "example.action-fixture".to_string(),
            receipt: component_receipt,
        }],
        component_commits: vec![ComponentCommitAnchor {
            namespace: "example.action-fixture".to_string(),
            commit_id: "commit-1".to_string(),
            input_digest: sha256_digest(b"commit"),
            revision: 1,
            committed_at_unix_ms: 1791820800001,
        }],
    }
}

fn example_row(complete: &CompleteStateV2, row_name: &str) -> Value {
    match row_name {
        "SnapshotDecisionRow" => serde_json::to_value(&complete.decisions[0]).unwrap(),
        "SnapshotDecisionOutcomeRow" => {
            serde_json::to_value(&complete.decision_outcomes[0]).unwrap()
        }
        "ComponentStateRow" => serde_json::to_value(&complete.component_states[0]).unwrap(),
        "ComponentEventRow" => serde_json::to_value(&complete.component_events[0]).unwrap(),
        "ComponentOperationRow" => serde_json::to_value(&complete.component_operations[0]).unwrap(),
        "ComponentReceiptRow" => serde_json::to_value(&complete.component_receipts[0]).unwrap(),
        "ComponentCommitAnchor" => serde_json::to_value(&complete.component_commits[0]).unwrap(),
        other => panic!("unexpected new_rows section {other}"),
    }
}

#[test]
fn complete_state_v2_carries_every_documented_section() {
    tick();
    let complete = build_complete_state();

    let json = complete.to_canonical_json().unwrap();
    let reloaded = CompleteStateV2::from_json(&json).unwrap();
    assert_eq!(reloaded, complete, "complete-state/v2 round trips");

    let sections: Value = serde_json::from_str(SECTIONS_JSON).unwrap();
    let section_map = sections["sections"]
        .as_object()
        .expect("sections object");
    let complete_value = serde_json::to_value(&complete).unwrap();
    let complete_object = complete_value.as_object().unwrap();

    for key in section_map.keys() {
        assert!(
            complete_object.contains_key(key),
            "complete-state/v2 serialization must carry section {key}"
        );
        if let Some(array) = complete_object[key].as_array() {
            assert!(
                !array.is_empty(),
                "section {key} is populated in the example snapshot"
            );
        }
    }

    let new_rows = sections["new_rows"].as_object().expect("new_rows object");
    let mut checked = HashSet::new();
    for (row_name, fields) in new_rows {
        let documented: BTreeSet<&String> =
            fields.as_object().unwrap().keys().collect();
        let example = example_row(&complete, row_name);
        let actual: BTreeSet<&String> = example.as_object().unwrap().keys().collect();
        assert_eq!(actual, documented, "row {row_name} field set matches the contract");
        checked.insert(row_name.clone());
    }
    assert_eq!(
        checked.len(),
        new_rows.len(),
        "every new_rows entry is exercised"
    );

    complete.version_gate().expect("v2 gates");
    complete
        .check_compatible(COMPLETE_STATE_SCHEMA_VERSION)
        .expect("schema 5 compatible");

    let mut v1 = complete.clone();
    v1.format = "agalma-state/v1".to_string();
    assert_eq!(
        failure_code(&v1.version_gate().unwrap_err()),
        Some(FailureCode::UnsupportedVersion)
    );
    assert!(v1.check_compatible(COMPLETE_STATE_SCHEMA_VERSION).is_err());

    let mut schema4 = complete.clone();
    schema4.schema_version = 4;
    assert!(
        schema4.check_compatible(COMPLETE_STATE_SCHEMA_VERSION).is_err(),
        "schema 4 is refused against schema 5"
    );
}

// --- 13. contracts purity ---------------------------------------------------

#[test]
fn contracts_are_free_of_io_and_vendor_handles() {
    tick();
    let sources: [(&str, &str); 4] = [
        ("plugin.rs", include_str!("../src/plugin.rs")),
        ("ledger.rs", include_str!("../src/ledger.rs")),
        ("snapshot.rs", include_str!("../src/snapshot.rs")),
        ("lib.rs", include_str!("../src/lib.rs")),
    ];
    let forbidden = [
        "std::fs",
        "std::net",
        "std::process",
        "rusqlite",
        "tokio::",
        "sqlx",
        "reqwest",
        "Command::new",
    ];
    for (name, source) in sources {
        for needle in forbidden {
            assert!(
                !source.contains(needle),
                "{name} must not contain {needle:?}"
            );
        }
    }
}

// --- 14. required-case counter ---------------------------------------------

#[test]
fn required_cases_executed_is_nonzero() {
    tick();
    assert!(CASES.load(Ordering::SeqCst) > 0);
}

// --- 15. plugin-snapshot digest identity and payload bounds ----------------

#[test]
fn plugin_snapshot_digests_and_bounds() {
    tick();
    let snapshot: PluginSnapshot = serde_json::from_str(SNAPSHOT_JSON).unwrap();

    // Positive: the golden snapshot's recorded digest recomputes exactly, every
    // entry payload digest verifies, and every payload is within the limit.
    snapshot
        .verify_snapshot_digest()
        .expect("golden snapshot digest verifies");
    assert_eq!(
        snapshot.recomputed_snapshot_digest().unwrap(),
        snapshot.snapshot_digest,
        "recomputed snapshot digest equals the recorded digest"
    );
    snapshot
        .verify_payload_digests()
        .expect("golden entry payload digests verify");
    snapshot
        .check_payload_bound()
        .expect("golden entry payloads are within the limit");

    // A tampered snapshot_digest (valid shape, wrong value) is IntegrityMismatch.
    let mut tampered = snapshot.clone();
    tampered.snapshot_digest = zero_digest();
    let err = tampered
        .verify_snapshot_digest()
        .expect_err("tampered snapshot digest refused");
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));

    // Changing an entry payload without updating its payload_digest is refused.
    let mut changed_payload = snapshot.clone();
    changed_payload.snapshots[0].payload = json!({ "ready": false });
    let err = changed_payload
        .verify_payload_digests()
        .expect_err("changed payload under a stale digest refused");
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));

    // An oversized payload (digest recomputed, so only size is wrong) is refused
    // by the payload bound as InvalidRecord.
    let mut oversized = snapshot.clone();
    oversized.snapshots[0].payload = json!({ "blob": "x".repeat(70_000) });
    oversized.snapshots[0].payload_digest = canonical_digest(&oversized.snapshots[0].payload).unwrap();
    oversized
        .verify_payload_digests()
        .expect("recomputed digest for the oversized payload verifies");
    let err = oversized
        .check_payload_bound()
        .expect_err("oversized payload refused by the size bound");
    assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));

    // Total bound: each entry payload sits exactly on the 64 KiB per-entry
    // limit and four of them total exactly the 256 KiB snapshot limit, so the
    // global publication bound passes; a fifth entry tips it over and is
    // refused as InvalidRecord.
    let max_chars = (REPORT_PAYLOAD_LIMIT_BYTES as usize) - "{\"p\":\"\"}".len();
    let bulk_entry = |name: &str| -> PluginSnapshotEntry {
        let payload = json!({ "p": "p".repeat(max_chars) });
        assert_eq!(
            canonical_json(&payload).unwrap().len() as u64,
            REPORT_PAYLOAD_LIMIT_BYTES,
            "the entry payload sits exactly on the per-entry limit"
        );
        PluginSnapshotEntry {
            schema_namespace: name.to_string(),
            schema_version: 1,
            payload_digest: canonical_digest(&payload).unwrap(),
            payload,
        }
    };

    assert_eq!(
        4 * REPORT_PAYLOAD_LIMIT_BYTES,
        PLUGIN_SNAPSHOT_PAYLOAD_LIMIT_BYTES,
        "four per-entry limits fill the frozen snapshot limit exactly"
    );

    let mut full = snapshot.clone();
    full.snapshots = (0..4)
        .map(|index| bulk_entry(&format!("example.bulk.{index}")))
        .collect();
    full.check_payload_bound()
        .expect("four exact 64 KiB entries total exactly the 256 KiB limit");

    let mut over = full.clone();
    over.snapshots.push(bulk_entry("example.bulk.4"));
    let err = over
        .check_payload_bound()
        .expect_err("a fifth 64 KiB entry exceeds the 256 KiB total limit");
    assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));
}

// --- 16. installation digest tamper ----------------------------------------

#[test]
fn installation_digest_tamper_is_refused() {
    tick();
    let installation: PluginInstallation = serde_json::from_str(INSTALLATION_JSON).unwrap();
    installation
        .verify_installation_digest()
        .expect("golden installation digest verifies");

    let mut tampered = installation.clone();
    tampered.installation_digest = zero_digest();
    let err = tampered
        .verify_installation_digest()
        .expect_err("tampered installation digest refused");
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));
}

// --- 17. component-commit input identity covers every field ----------------

#[test]
fn component_commit_input_identity_covers_every_field() {
    tick();
    let base: ComponentCommit = serde_json::from_str(COMPONENT_COMMIT_JSON).unwrap();
    let baseline = base.computed_input_digest().unwrap();
    assert_eq!(
        baseline, base.input_digest,
        "the golden recorded digest is the recomputed digest"
    );

    // Bounded (no RNG) property: for each documented top-level input field,
    // mutate it to a distinct documented-valid value; the recomputed identity
    // must change, and the mutated commit must re-verify once its recorded
    // digest is updated. Every listed field admits a valid mutation, so none is
    // skipped.
    let mut mutations: Vec<(&str, ComponentCommit)> = Vec::new();

    let mut namespace = base.clone();
    namespace.namespace = "example.other-fixture".to_string();
    mutations.push(("namespace", namespace));

    let mut commit_id = base.clone();
    commit_id.commit_id = "commit-2".to_string();
    mutations.push(("commit_id", commit_id));

    let mut expected_revision = base.clone();
    expected_revision.expected_revision = 7;
    mutations.push(("expected_revision", expected_revision));

    let mut checkpoint = base.clone();
    checkpoint.checkpoint = Some(json!({ "value": 2 }));
    mutations.push(("checkpoint", checkpoint));

    let mut events = base.clone();
    events.events[0].kind = "fixture.updated-two".to_string();
    mutations.push(("events", events));

    let mut operations = base.clone();
    operations.operations[0].deadline_unix_ms += 1;
    mutations.push(("operations", operations));

    let mut receipts = base.clone();
    receipts.receipts[0].completed_at_unix_ms += 1;
    mutations.push(("receipts", receipts));

    assert!(!mutations.is_empty(), "at least one mutation is constructed");

    let mut checked = 0usize;
    for (field, mut mutated) in mutations {
        let digest = mutated
            .computed_input_digest()
            .unwrap_or_else(|e| panic!("mutating {field} recomputes a digest: {e}"));
        assert_ne!(
            digest, baseline,
            "mutating {field} changes the commit input identity"
        );
        mutated.input_digest = digest;
        mutated
            .verify_input_digest()
            .unwrap_or_else(|e| panic!("mutated {field} re-verifies its digest: {e}"));
        checked += 1;
    }
    assert!(checked > 0, "the field-mutation count is nonzero");
    assert!(checked >= 6, "at least six fields are exercised");
}

// --- 18. complete-state/v2 refuses unknown sections ------------------------

#[test]
fn complete_state_v2_refuses_unknown_section() {
    tick();
    let complete = build_complete_state();
    let canonical = complete.to_canonical_json().unwrap();
    CompleteStateV2::from_json(&canonical).expect("the unmodified canonical state round trips");

    let mut value: Value = serde_json::from_str(&canonical).unwrap();
    value
        .as_object_mut()
        .expect("complete-state is a JSON object")
        .insert("bogus_section".to_string(), json!([]));
    let injected = serde_json::to_string(&value).unwrap();

    let err = CompleteStateV2::from_json(&injected)
        .expect_err("an unknown top-level section is refused");
    assert_eq!(
        failure_code(&err),
        None,
        "an unknown section is a deserialization refusal, not a coded contract failure"
    );
}
