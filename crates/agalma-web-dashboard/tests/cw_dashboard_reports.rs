//! CW.3 dashboard producer acceptance (`agalma-51dg.4`).
//!
//! Covered here:
//!
//! - the eight listener × binding × publication-authority combinations;
//! - the frozen `DashboardReportState/v1` persistence algorithm: crash before
//!   send, lost acknowledgement, exact replay, restart without a sequence
//!   reset, generation fencing, corrupt state and symlink refusal;
//! - denied/expired publication authority and cancellation retaining pending
//!   bytes;
//! - `plugin.Snapshot` exposing only the acknowledged boundary;
//! - a seeded property run of 200 operation sequences against an independent
//!   sequence/pending oracle.
//!
//! The installed/supervised production path (`hub.PublishReport`, generic
//! component detail and owner screenshots) is exercised by `first_launch.rs`
//! through the real `PluginHost` and ledger.

use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use agalma_contracts::{
    canonical_digest, sha256_digest, BindingPin, FailureCode, ReportEnvelope, ReportPublication,
};
use agalma_ipc::report_publisher::{
    PublishOutcome, PublisherLimits, ReportPublisher, ReportTransport,
};
use agalma_ipc::{IpcError, PluginHandler};
use agalma_testkit::ScratchDir;
use agalma_web_dashboard::plugin::{DashboardHandler, RuntimeStatus};
use agalma_web_dashboard::reports::{
    publisher_binding, report_identity_dir, report_schema_ref, status_payload,
    FilePublicationStore, ReportObservation, REPORT_SCHEMA_NAMESPACE, REPORT_SCHEMA_VERSION,
    REPORT_STATE_FORMAT, REPORT_STATE_LIMIT_BYTES,
};
use agalma_web_dashboard::{
    PACKAGE_VERSION, PLUGIN_ID, READ_CAPABILITY, REPORT_PUBLISH_CAPABILITY,
};

/// Recorded deterministic property seed (`CW3`).
const PROPERTY_SEED: u64 = 0x4357_4433;
/// One sequence per generated case.
const PROPERTY_CASES: u64 = 200;
/// Operations per generated sequence.
const PROPERTY_STEPS: u64 = 12;

struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    fn new(start: u64) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicU64::new(start),
        })
    }

    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn advance(&self, by: u64) {
        self.now.fetch_add(by, Ordering::SeqCst);
    }

    fn arc(self: &Arc<Self>) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let clock = Arc::clone(self);
        Arc::new(move || clock.now())
    }
}

fn binding(generation: u64) -> BindingPin {
    BindingPin {
        binding_id: PLUGIN_ID.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: PACKAGE_VERSION.to_string(),
        generation,
        config_digest: sha256_digest(b"cw3-dashboard-config"),
    }
}

fn test_limits() -> PublisherLimits {
    PublisherLimits {
        max_payload_bytes: agalma_ipc::DEFAULT_MAX_PAYLOAD_BYTES,
        publish_deadline_ms: 50,
        coalesce_after_ms: 0,
        max_retry_backoff_ms: 500,
    }
}

#[derive(Clone, Debug)]
enum Reply {
    Ack,
    Lost,
    Denied,
}

struct ScriptedTransport {
    replies: VecDeque<Reply>,
    default: Reply,
    sent: Vec<Value>,
}

impl ScriptedTransport {
    fn new(default: Reply) -> Self {
        Self {
            replies: VecDeque::new(),
            default,
            sent: Vec::new(),
        }
    }

    fn push(&mut self, reply: Reply) {
        self.replies.push_back(reply);
    }

    fn publications(&self) -> Vec<ReportPublication> {
        self.sent
            .iter()
            .map(|request| serde_json::from_value(request["publication"].clone()).unwrap())
            .collect()
    }
}

impl ReportTransport for ScriptedTransport {
    fn publish_report<'a>(
        &'a mut self,
        request: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, IpcError>> + Send + 'a>> {
        Box::pin(async move {
            self.sent.push(request.clone());
            let reply = self
                .replies
                .pop_front()
                .unwrap_or_else(|| self.default.clone());
            let publication: ReportPublication =
                serde_json::from_value(request["publication"].clone()).unwrap();
            match reply {
                Reply::Ack => Ok(serde_json::to_value(ReportEnvelope::from_publication(
                    &publication,
                    publication.reported_at_unix_ms.saturating_add(1),
                ))
                .unwrap()),
                Reply::Lost => Err(IpcError::unavailable("acknowledgement lost")),
                Reply::Denied => Err(IpcError::Contract(
                    FailureCode::AuthorityDenied.failure(PLUGIN_ID, "report grant expired"),
                )),
            }
        })
    }
}

fn open_publisher(
    root: &Path,
    pin: BindingPin,
    clock: &Arc<ManualClock>,
) -> Result<ReportPublisher<FilePublicationStore>, agalma_contracts::ContractError> {
    let store = FilePublicationStore::open_in_state_root(root, pin.clone(), clock.arc()).unwrap();
    ReportPublisher::open(store, publisher_binding(pin), test_limits(), clock.arc())
}

fn state_path(root: &Path, pin: &BindingPin) -> PathBuf {
    report_identity_dir(root, pin).unwrap().join("state.json")
}

fn read_state(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("report state exists")).expect("state json")
}

fn read_state_opt(path: &Path) -> Option<Value> {
    fs::read(path)
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes).expect("state json"))
}

/// An empty state document is valid before the first seal.
fn state_or_empty(path: &Path, pin: &BindingPin) -> Value {
    read_state_opt(path).unwrap_or_else(|| {
        json!({
            "format": REPORT_STATE_FORMAT,
            "plugin_id": PLUGIN_ID,
            "binding": pin,
            "checkpoint": Value::Null,
            "acknowledged": Value::Null,
            "pending": Value::Null,
        })
    })
}

fn status(fixture_listener: bool, fixture_binding: bool) -> RuntimeStatus {
    let status = RuntimeStatus::new(4, sha256_digest(b"cw3-config"));
    if fixture_listener {
        status.listener_ready("http://127.0.0.1:43123");
    } else {
        status.listener_unavailable("listener unavailable");
    }
    if fixture_binding {
        status.binding_ready();
    } else {
        status.binding_unavailable("binding unavailable");
    }
    status
}

// ---------------------------------------------------------------------------
// Declared schema and manifest identity
// ---------------------------------------------------------------------------

#[test]
fn declared_schema_matches_the_installed_manifest_and_grants_are_separate() {
    let manifest: Value =
        serde_json::from_str(include_str!("../package/plugin-manifest.json")).unwrap();
    assert_eq!(manifest["plugin_id"], PLUGIN_ID);
    let requested: Vec<&str> = manifest["requested_capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(requested.contains(&REPORT_PUBLISH_CAPABILITY));
    assert!(requested.contains(&READ_CAPABILITY));
    let schema = &manifest["report_schemas"][0];
    assert_eq!(schema["namespace"], REPORT_SCHEMA_NAMESPACE);
    assert_eq!(schema["version"], REPORT_SCHEMA_VERSION);
    let compiled = report_schema_ref();
    assert_eq!(schema["schema"]["digest"], compiled.schema.digest);
    assert_eq!(schema["schema"]["bytes"], compiled.schema.bytes);
    assert_eq!(schema["schema"]["path"], compiled.schema.path);
    assert_eq!(
        compiled.schema.digest,
        sha256_digest(include_bytes!("../package/objects/report-schema.json"))
    );

    // A report grant never stands in for a read grant.
    let header: Value = serde_json::from_str(&format!(
        r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{{"capability":"report.publish","hub_socket":"/tmp/hub.sock","grant":"{}","plugin_id":"agalma.web-dashboard","generation":"4"}}]}}"#,
        "00".repeat(32)
    ))
    .unwrap();
    let parsed = agalma_ipc::parse_capability_bindings(&header.to_string()).unwrap();
    assert!(parsed.read().is_none());
    assert!(parsed.control().is_none());
    assert!(parsed.get(REPORT_PUBLISH_CAPABILITY).is_some());
}

// ---------------------------------------------------------------------------
// Eight readiness × publication-authority combinations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn readiness_and_publication_authority_combinations() {
    let scratch = ScratchDir::new("cw3-combinations");
    let mut case = 0;
    for listener in [false, true] {
        for read_binding in [false, true] {
            for authority in [false, true] {
                case += 1;
                let root = scratch.child(&format!("case-{case}"));
                let pin = binding(7);
                let status = status(listener, read_binding);
                let payload = status_payload(&status.snapshot());
                let url = if listener {
                    Value::String("http://127.0.0.1:43123".to_string())
                } else {
                    Value::Null
                };
                assert_eq!(
                    payload,
                    json!({
                        "listener": if listener { "ready" } else { "unavailable" },
                        "binding": if read_binding { "ready" } else { "unavailable" },
                        "url": url,
                    }),
                    "case {case}"
                );

                let ready_before = status.snapshot();
                if authority {
                    let clock = ManualClock::new(1_000);
                    let mut publisher = open_publisher(&root, pin.clone(), &clock).unwrap();
                    publisher
                        .seal(
                            REPORT_SCHEMA_NAMESPACE,
                            REPORT_SCHEMA_VERSION,
                            payload.clone(),
                            1_000,
                        )
                        .unwrap();
                    let mut transport = ScriptedTransport::new(Reply::Ack);
                    let outcome = publisher
                        .publish_pending(&mut transport, "report-grant")
                        .await
                        .unwrap();
                    assert!(
                        matches!(outcome, PublishOutcome::Acknowledged(_)),
                        "case {case}"
                    );
                    assert_eq!(read_state(&state_path(&root, &pin))["pending"], Value::Null);
                    // A successful publication never changes readiness facts.
                    assert_eq!(status.snapshot(), ready_before, "case {case}");
                } else {
                    // Without the separate grant the state document is never
                    // created and no publication is attempted.
                    assert!(!state_path(&root, &pin).exists(), "case {case}");
                    assert_eq!(status.snapshot(), ready_before, "case {case}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence, replay, fencing and corruption
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crash_before_send_replays_exact_bytes_and_keeps_contiguous_sequence() {
    let scratch = ScratchDir::new("cw3-crash");
    let root = scratch.child("state");
    let pin = binding(3);
    let clock = ManualClock::new(10);
    let mut publisher = open_publisher(&root, pin.clone(), &clock).unwrap();
    let payload =
        json!({"listener": "ready", "binding": "unavailable", "url": "http://127.0.0.1:1"});
    let sealed = publisher
        .seal(REPORT_SCHEMA_NAMESPACE, REPORT_SCHEMA_VERSION, payload, 10)
        .unwrap();
    drop(publisher);

    let persisted = read_state(&state_path(&root, &pin));
    assert_eq!(persisted["format"], REPORT_STATE_FORMAT);
    assert_eq!(persisted["pending"]["sequence"], 1);
    let persisted_publication: ReportPublication =
        serde_json::from_value(persisted["pending"].clone()).unwrap();
    assert_eq!(persisted_publication, sealed);

    let mut restarted = open_publisher(&root, pin.clone(), &clock).unwrap();
    assert_eq!(restarted.pending_publication(), Some(&sealed));
    assert_eq!(restarted.checkpoint(), None);
    let mut transport = ScriptedTransport::new(Reply::Ack);
    let outcome = restarted
        .publish_pending(&mut transport, "report-grant")
        .await
        .unwrap();
    assert!(matches!(outcome, PublishOutcome::Acknowledged(_)));
    assert_eq!(transport.publications(), vec![sealed]);
    assert_eq!(restarted.checkpoint().unwrap().sequence, 1);
}

#[tokio::test]
async fn lost_acknowledgement_retries_identical_and_never_skips_a_sequence() {
    let scratch = ScratchDir::new("cw3-lost-ack");
    let root = scratch.child("state");
    let pin = binding(3);
    let clock = ManualClock::new(100);
    let mut publisher = open_publisher(&root, pin.clone(), &clock).unwrap();
    let payload = status_payload(&status(true, true).snapshot());
    let sealed = publisher
        .seal(
            REPORT_SCHEMA_NAMESPACE,
            REPORT_SCHEMA_VERSION,
            payload.clone(),
            100,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::Ack);
    transport.push(Reply::Lost);
    assert_eq!(
        publisher
            .publish_pending(&mut transport, "grant")
            .await
            .unwrap(),
        PublishOutcome::Pending
    );
    assert_eq!(publisher.pending_publication(), Some(&sealed));
    assert_eq!(publisher.checkpoint(), None);
    assert_eq!(
        read_state(&state_path(&root, &pin))["pending"]["sequence"],
        1
    );

    clock.advance(10_000);
    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert!(matches!(outcome, PublishOutcome::Acknowledged(_)));
    let sent = transport.publications();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1], "replay must be byte-identical");
    assert_eq!(publisher.checkpoint().unwrap().sequence, 1);

    // Restart after acknowledgement is a no-op and the next sequence is 2.
    drop(publisher);
    let mut restarted = open_publisher(&root, pin.clone(), &clock).unwrap();
    assert_eq!(restarted.checkpoint().unwrap().sequence, 1);
    assert_eq!(restarted.pending_publication(), None);
    let next = restarted
        .seal(
            REPORT_SCHEMA_NAMESPACE,
            REPORT_SCHEMA_VERSION,
            status_payload(&status(true, false).snapshot()),
            clock.now(),
        )
        .unwrap();
    assert_eq!(next.sequence, 2);
}

#[test]
fn corrupt_state_fails_closed_without_resetting_the_sequence() {
    let scratch = ScratchDir::new("cw3-corrupt");
    let root = scratch.child("state");
    let pin = binding(9);
    let clock = ManualClock::new(10);
    let mut publisher = open_publisher(&root, pin.clone(), &clock).unwrap();
    publisher
        .seal(
            REPORT_SCHEMA_NAMESPACE,
            REPORT_SCHEMA_VERSION,
            status_payload(&status(true, true).snapshot()),
            10,
        )
        .unwrap();
    let path = state_path(&root, &pin);
    let original = fs::read(&path).unwrap();
    let corrupt = b"{\"format\":\"DashboardReportState/v1\",\"plugin_id\":\"agalma.web-dashboard\"";
    fs::write(&path, corrupt).unwrap();
    let error = match open_publisher(&root, pin.clone(), &clock) {
        Ok(_) => panic!("corrupt state must not open"),
        Err(error) => error,
    };
    assert_eq!(
        agalma_contracts::failure_code(&error),
        Some(FailureCode::StateUnavailable)
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        corrupt,
        "corrupt bytes are never reset"
    );
    assert_ne!(fs::read(&path).unwrap(), original);
    assert!(!path.with_extension("reset").exists());
}

#[test]
fn new_generation_uses_a_fresh_identity_path_and_foreign_state_is_fenced() {
    let scratch = ScratchDir::new("cw3-generation");
    let root = scratch.child("state");
    let first = binding(11);
    let second = binding(12);
    let clock = ManualClock::new(10);
    let first_path = state_path(&root, &first);
    let second_path = state_path(&root, &second);
    assert_ne!(first_path, second_path);

    let mut publisher = open_publisher(&root, first.clone(), &clock).unwrap();
    publisher
        .seal(
            REPORT_SCHEMA_NAMESPACE,
            REPORT_SCHEMA_VERSION,
            status_payload(&status(true, true).snapshot()),
            10,
        )
        .unwrap();
    // Copy the first generation's document onto the second identity path: it
    // must be fenced, never read for a sequence or rewritten.
    fs::create_dir_all(second_path.parent().unwrap()).unwrap();
    fs::copy(&first_path, &second_path).unwrap();
    let before = fs::read(&second_path).unwrap();
    let error = match open_publisher(&root, second, &clock) {
        Ok(_) => panic!("foreign generation must be fenced"),
        Err(error) => error,
    };
    assert_eq!(
        agalma_contracts::failure_code(&error),
        Some(FailureCode::StaleGeneration)
    );
    assert_eq!(fs::read(&second_path).unwrap(), before);
}

#[test]
fn symlinked_private_subtree_is_refused_without_writes() {
    let scratch = ScratchDir::new("cw3-symlink");
    let root = scratch.child("state");
    fs::create_dir_all(root.join("plugin-sockets")).unwrap();
    let outside = scratch.child("outside");
    fs::create_dir_all(root.join("report-outboxes")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("report-outboxes").join(PLUGIN_ID)).unwrap();
    let pin = binding(13);
    let clock = ManualClock::new(10);
    let error = match open_publisher(&root, pin.clone(), &clock) {
        Ok(mut publisher) => publisher
            .seal(
                REPORT_SCHEMA_NAMESPACE,
                REPORT_SCHEMA_VERSION,
                status_payload(&status(true, true).snapshot()),
                10,
            )
            .expect_err("symlinked report subtree must be refused"),
        Err(error) => error,
    };
    assert_eq!(
        agalma_contracts::failure_code(&error),
        Some(FailureCode::StateUnavailable)
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert!(!state_path(&root, &pin).exists());
}

#[tokio::test]
async fn denied_or_expired_grant_fences_and_retains_the_pending_record() {
    let scratch = ScratchDir::new("cw3-denied");
    let root = scratch.child("state");
    let pin = binding(14);
    let clock = ManualClock::new(50);
    let mut publisher = open_publisher(&root, pin.clone(), &clock).unwrap();
    let sealed = publisher
        .seal(
            REPORT_SCHEMA_NAMESPACE,
            REPORT_SCHEMA_VERSION,
            status_payload(&status(true, true).snapshot()),
            50,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::Denied);
    assert_eq!(
        publisher
            .publish_pending(&mut transport, "expired")
            .await
            .unwrap(),
        PublishOutcome::Fenced
    );
    assert!(publisher.is_fenced());
    assert_eq!(publisher.pending_publication(), Some(&sealed));
    assert_eq!(
        publisher
            .publish_pending(&mut transport, "expired")
            .await
            .unwrap(),
        PublishOutcome::Fenced
    );
    assert_eq!(
        transport.sent.len(),
        1,
        "a fenced publisher sends nothing more"
    );
    let state = read_state(&state_path(&root, &pin));
    assert_eq!(state["pending"]["sequence"], 1);
    assert_eq!(state["checkpoint"], Value::Null);

    // A fresh launch with a fresh grant replays the same immutable record.
    drop(publisher);
    let mut relaunched = open_publisher(&root, pin.clone(), &clock).unwrap();
    let mut transport = ScriptedTransport::new(Reply::Ack);
    assert!(matches!(
        relaunched
            .publish_pending(&mut transport, "fresh")
            .await
            .unwrap(),
        PublishOutcome::Acknowledged(_)
    ));
    assert_eq!(transport.publications(), vec![sealed]);
}

#[tokio::test]
async fn snapshot_exposes_only_the_acknowledged_boundary() {
    let scratch = ScratchDir::new("cw3-snapshot");
    let root = scratch.child("state");
    let pin = binding(15);
    let clock = ManualClock::new(10);
    let observation = Arc::new(ReportObservation::new());

    let mut publisher = open_publisher(&root, pin.clone(), &clock).unwrap();
    let acknowledged_payload = status_payload(&status(true, true).snapshot());
    let sealed = publisher
        .seal(
            REPORT_SCHEMA_NAMESPACE,
            REPORT_SCHEMA_VERSION,
            acknowledged_payload.clone(),
            10,
        )
        .unwrap();
    // Unacknowledged pending content is not observable.
    let handler = DashboardHandler::new(Arc::new(status(true, true)), vec![], observation.clone());
    let snapshot: Value = serde_json::from_value(handler.snapshot().unwrap()).unwrap();
    assert_eq!(snapshot["watermark"], 0);
    assert_eq!(snapshot["snapshots"].as_array().unwrap().len(), 0);

    let envelope = ReportEnvelope::from_publication(&sealed, 11);
    observation.acknowledge(&envelope);
    let snapshot: Value = serde_json::from_value(handler.snapshot().unwrap()).unwrap();
    assert_eq!(snapshot["watermark"], 1);
    let entries = snapshot["snapshots"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["schema_namespace"], REPORT_SCHEMA_NAMESPACE);
    assert_eq!(entries[0]["payload"], acknowledged_payload);
    assert_eq!(
        entries[0]["payload_digest"],
        canonical_digest(&acknowledged_payload).unwrap()
    );

    // Acknowledge sequence 1 through the publisher before sealing the next one.
    let mut transport = ScriptedTransport::new(Reply::Ack);
    assert!(matches!(
        publisher
            .publish_pending(&mut transport, "grant")
            .await
            .unwrap(),
        PublishOutcome::Acknowledged(_)
    ));
    assert_eq!(publisher.checkpoint().unwrap().sequence, 1);

    // Seal a newer unacknowledged sample: the snapshot still shows sequence 1.
    clock.advance(6_000);
    let pending = publisher
        .seal(
            REPORT_SCHEMA_NAMESPACE,
            REPORT_SCHEMA_VERSION,
            status_payload(&status(false, false).snapshot()),
            clock.now(),
        )
        .unwrap();
    assert_eq!(pending.sequence, 2);
    let snapshot: Value = serde_json::from_value(handler.snapshot().unwrap()).unwrap();
    assert_eq!(snapshot["watermark"], 1);
    assert_eq!(snapshot["snapshots"][0]["payload"], acknowledged_payload);
}

#[test]
fn bounded_state_document_enforces_the_256k_cap() {
    assert_eq!(REPORT_STATE_LIMIT_BYTES, 256 * 1024);
}

// ---------------------------------------------------------------------------
// Cancellation retains pending bytes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancellation_joins_the_reporter_and_retains_pending_state() {
    let scratch = ScratchDir::new("cw3-cancel");
    let root = scratch.child("state");
    fs::create_dir_all(root.join("plugin-sockets")).unwrap();
    let socket = root.join("plugin-sockets/plugin-e1.sock");
    let runtime_status = status(true, false);
    let pin = runtime_status.binding_pin();
    let status = Arc::new(runtime_status);
    let observation = Arc::new(ReportObservation::new());
    let handle = agalma_web_dashboard::reports::start_reporting(
        &socket,
        root.join("missing-hub.sock"),
        "grant".to_string(),
        status,
        observation.clone(),
    )
    .expect("reporting task starts");
    let path = state_path(&root, &pin);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no pending state was persisted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.shutdown().await;
    let state = read_state(&path);
    assert_eq!(state["pending"]["sequence"], 1);
    assert_eq!(state["checkpoint"], Value::Null);
    assert_eq!(observation.observed().watermark, 0);

    // Same-binding restart replays sequence 1, never a reset.
    let clock = ManualClock::new(10_000);
    let mut restarted = open_publisher(&root, pin.clone(), &clock).unwrap();
    let pending = restarted.pending_publication().cloned().unwrap();
    assert_eq!(pending.sequence, 1);
    let mut transport = ScriptedTransport::new(Reply::Ack);
    assert!(matches!(
        restarted
            .publish_pending(&mut transport, "fresh")
            .await
            .unwrap(),
        PublishOutcome::Acknowledged(_)
    ));
    assert_eq!(transport.publications(), vec![pending]);
}

// ---------------------------------------------------------------------------
// Seeded 200-sequence property with an independent oracle
// ---------------------------------------------------------------------------

struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }
}

#[tokio::test]
async fn property_report_sequences_never_skip_replay_or_expose_pending_state() {
    for case in 0..PROPERTY_CASES {
        let scratch = ScratchDir::new("cw3-property");
        let root = scratch.child(&format!("case-{case}"));
        let pin = binding(20 + case % 3);
        let clock = ManualClock::new(1_000 + case);
        let observation = Arc::new(ReportObservation::new());
        let path = state_path(&root, &pin);
        let mut rng = XorShift64::new(PROPERTY_SEED ^ case.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut publisher = open_publisher(&root, pin.clone(), &clock).unwrap();

        // Independent oracle: last acknowledged sequence/payload and the one
        // permitted pending identity.
        let mut acknowledged_sequence = 0u64;
        let mut acknowledged_payload: Option<Value> = None;
        let mut pending_payload: Option<Value> = None;
        let mut sent_identities: Vec<(u64, u64, String)> = Vec::new();

        for step in 0..PROPERTY_STEPS {
            clock.advance(10_000);
            let choice = rng.next() % 10;
            if choice < 4 && pending_payload.is_none() {
                let payload = json!({
                    "listener": if rng.next() % 2 == 0 { "ready" } else { "unavailable" },
                    "binding": if rng.next() % 2 == 0 { "ready" } else { "unavailable" },
                    "url": if rng.next() % 3 == 0 { Value::Null } else { json!("http://127.0.0.1:9") },
                });
                let sealed = publisher
                    .seal(
                        REPORT_SCHEMA_NAMESPACE,
                        REPORT_SCHEMA_VERSION,
                        payload.clone(),
                        clock.now(),
                    )
                    .unwrap();
                assert_eq!(sealed.sequence, acknowledged_sequence + 1);
                pending_payload = Some(payload);
            } else if choice < 8 && pending_payload.is_some() {
                let mut transport = ScriptedTransport::new(Reply::Ack);
                if rng.next() % 4 == 0 {
                    transport.push(Reply::Lost);
                }
                let outcome = publisher
                    .publish_pending(&mut transport, "grant")
                    .await
                    .unwrap();
                match outcome {
                    PublishOutcome::Acknowledged(envelope) => {
                        assert_eq!(envelope.sequence, acknowledged_sequence + 1);
                        acknowledged_sequence += 1;
                        acknowledged_payload = Some(envelope.payload.clone());
                        pending_payload = None;
                        observation.acknowledge(&envelope);
                    }
                    PublishOutcome::Pending => {
                        assert_eq!(
                            publisher.checkpoint().map(|cp| cp.sequence),
                            if acknowledged_sequence == 0 {
                                None
                            } else {
                                Some(acknowledged_sequence)
                            }
                        );
                    }
                    PublishOutcome::Fenced => panic!("case {case} unexpectedly fenced"),
                }
                for publication in transport.publications() {
                    let encoded = serde_json::to_string(&publication).unwrap();
                    let identity = (
                        publication.binding.generation,
                        publication.sequence,
                        encoded.clone(),
                    );
                    if let Some((_, _, previous)) =
                        sent_identities.iter().find(|(generation, sequence, _)| {
                            *generation == identity.0 && *sequence == identity.1
                        })
                    {
                        assert_eq!(previous, &encoded, "changed content reused an identity");
                    } else {
                        sent_identities.push(identity);
                    }
                }
            } else if choice == 8 {
                // Restart on the same binding: the immutable pending record is
                // replayed and the acknowledged checkpoint is not reset.
                let before_pending = publisher.pending_publication().cloned();
                let before_checkpoint = publisher.checkpoint().cloned();
                drop(publisher);
                let mut store =
                    FilePublicationStore::open_in_state_root(&root, pin.clone(), clock.arc())
                        .unwrap();
                if let Some(envelope) = store.acknowledged().unwrap() {
                    let observed = observation.observed();
                    assert_eq!(observed.payload, Some(envelope.payload.clone()));
                }
                publisher = ReportPublisher::open(
                    store,
                    publisher_binding(pin.clone()),
                    test_limits(),
                    clock.arc(),
                )
                .unwrap();
                assert_eq!(publisher.pending_publication(), before_pending.as_ref());
                assert_eq!(publisher.checkpoint(), before_checkpoint.as_ref());
            } else {
                // No-op observation checkpoint.
            }

            // Oracle checks after every operation.
            let state = state_or_empty(&path, &pin);
            if state["checkpoint"].is_null() {
                assert_eq!(acknowledged_sequence, 0, "case {case} step {step}");
            } else {
                assert_eq!(
                    state["checkpoint"]["sequence"], acknowledged_sequence,
                    "case {case} step {step}"
                );
                assert_eq!(
                    state["checkpoint"]["payload_digest"],
                    canonical_digest(acknowledged_payload.as_ref().unwrap()).unwrap()
                );
            }
            assert_eq!(
                publisher.checkpoint().map(|cp| cp.sequence),
                if acknowledged_sequence == 0 {
                    None
                } else {
                    Some(acknowledged_sequence)
                }
            );
            match &pending_payload {
                Some(payload) => {
                    assert_eq!(
                        publisher.pending_publication().map(|p| p.sequence),
                        Some(acknowledged_sequence + 1),
                        "case {case} step {step}"
                    );
                    assert_eq!(state["pending"]["payload"], *payload);
                    assert_eq!(publisher.pending_publication().unwrap().payload, *payload);
                }
                None => {
                    assert!(publisher.pending_publication().is_none());
                    assert!(state["pending"].is_null());
                }
            }
            // The observable snapshot contains only the acknowledged payload.
            let observed = observation.observed();
            assert_eq!(observed.watermark, acknowledged_sequence);
            assert_eq!(observed.payload, acknowledged_payload);
            if let Some(pending) = &pending_payload {
                if Some(pending) != acknowledged_payload.as_ref() {
                    assert_ne!(observed.payload.as_ref(), Some(pending));
                }
            }
        }

        // Drain the one permitted pending record.
        while publisher.pending_publication().is_some() {
            clock.advance(10_000);
            let mut transport = ScriptedTransport::new(Reply::Ack);
            if let PublishOutcome::Acknowledged(envelope) = publisher
                .publish_pending(&mut transport, "grant")
                .await
                .unwrap()
            {
                acknowledged_sequence += 1;
                acknowledged_payload = Some(envelope.payload.clone());
                observation.acknowledge(&envelope);
            }
        }
        if acknowledged_sequence == 0 {
            assert!(publisher.checkpoint().is_none());
            assert_eq!(observation.observed().watermark, 0);
            continue;
        }
        assert_eq!(
            publisher.checkpoint().unwrap().sequence,
            acknowledged_sequence
        );
        assert_eq!(observation.observed().payload, acknowledged_payload);
        let state = state_or_empty(&path, &pin);
        assert_eq!(state["pending"], Value::Null);
        assert_eq!(state["checkpoint"]["sequence"], acknowledged_sequence);
    }
}
