//! CW.2 deterministic publisher/outbox tests (agalma-51dg.3).
//!
//! These tests use a logical clock, an in-memory durable-store fake and a
//! scripted transport. They establish the primitive ordering/invariant contract
//! from `docs/web-dashboard-design.md` § CW.0 freeze. They do not establish the
//! production end-to-end hub/ingestor path; that remains the separate CW.10
//! acceptance. Existing conductor `hp_reports` tests remain the supported local
//! hub-side integration and are not replaced by this file.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agalma_contracts::digest::{canonical_digest, canonical_json, failure_code};
use agalma_contracts::{
    BindingPin, ContractError, Durability, FailureCode, PackageArtifactRef, ReportEnvelope,
    ReportPublication, ReportSchemaRef, REPORT_PAYLOAD_LIMIT_BYTES,
};
use agalma_ipc::report_publisher::{
    AckCheckpoint, LoadedPublication, PublicationStore, PublishOutcome, PublisherBinding,
    PublisherLimits, ReportPublisher, ReportTransport, DEFAULT_COALESCE_AFTER_MS,
    DEFAULT_MAX_PAYLOAD_BYTES, DEFAULT_MAX_RETRY_BACKOFF_MS, DEFAULT_PUBLISH_DEADLINE_MS,
    INITIAL_RETRY_BACKOFF_MS,
};
use agalma_ipc::IpcError;
use serde_json::{json, Value};

const PLUGIN_ID: &str = "example.reporter";
const OTHER_PLUGIN_ID: &str = "example.other";
const SCHEMA_NAMESPACE: &str = "example.reporter.status";
const SCHEMA_VERSION: u64 = 1;
const START_MS: u64 = 1_791_820_800_000;
/// Recorded deterministic xorshift64 seed (one per generated sequence).
const PROPERTY_SEED: u64 = 0x0bd3_5eed_0000_0c20;
/// Recorded generated sequence budget.
const PROPERTY_CASES: u64 = 200;
/// Recorded operation budget per generated sequence.
const PROPERTY_STEPS: u64 = 32;

// ---------------------------------------------------------------------------
// Deterministic fixtures
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Clock {
    now: Arc<AtomicU64>,
}

impl Clock {
    fn new(start: u64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(start)),
        }
    }

    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn advance(&self, by: u64) {
        self.now.fetch_add(by, Ordering::SeqCst);
    }

    fn arc(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let now = self.now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    }
}

fn binding_pin(generation: u64) -> BindingPin {
    BindingPin {
        binding_id: PLUGIN_ID.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "1.2.3".to_string(),
        generation,
        config_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

fn declared_schema(namespace: &str, version: u64) -> ReportSchemaRef {
    ReportSchemaRef {
        namespace: namespace.to_string(),
        version,
        schema: PackageArtifactRef::new(
            format!("schemas/{namespace}-{version}.json"),
            b"{\"type\":\"object\"}\n",
            "application/schema+json",
        ),
    }
}

fn publisher_binding(generation: u64) -> PublisherBinding {
    PublisherBinding {
        plugin_id: PLUGIN_ID.to_string(),
        binding: binding_pin(generation),
        declared_schemas: vec![declared_schema(SCHEMA_NAMESPACE, SCHEMA_VERSION)],
    }
}

fn test_limits() -> PublisherLimits {
    PublisherLimits {
        max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
        publish_deadline_ms: 50,
        coalesce_after_ms: 5_000,
        max_retry_backoff_ms: 1_000,
    }
}

#[derive(Clone, Debug)]
struct StoreState {
    generation: u64,
    checkpoint: Option<AckCheckpoint>,
    pending: Option<ReportPublication>,
    fenced: Option<u64>,
    load_error: bool,
    begin_error: bool,
    ack_error: bool,
}

#[derive(Clone)]
struct FakeStore {
    state: Arc<Mutex<StoreState>>,
    clock: Clock,
}

impl FakeStore {
    fn new(generation: u64, clock: Clock) -> Self {
        Self {
            state: Arc::new(Mutex::new(StoreState {
                generation,
                checkpoint: None,
                pending: None,
                fenced: None,
                load_error: false,
                begin_error: false,
                ack_error: false,
            })),
            clock,
        }
    }

    fn snapshot(&self) -> StoreState {
        self.state.lock().unwrap().clone()
    }

    fn set_load_error(&self, value: bool) {
        self.state.lock().unwrap().load_error = value;
    }

    fn set_begin_error(&self, value: bool) {
        self.state.lock().unwrap().begin_error = value;
    }

    fn set_ack_error(&self, value: bool) {
        self.state.lock().unwrap().ack_error = value;
    }

    fn fence(&self, generation: u64) {
        self.state.lock().unwrap().fenced = Some(generation);
    }

    fn pending_count(&self) -> usize {
        usize::from(self.state.lock().unwrap().pending.is_some())
    }
}

impl PublicationStore for FakeStore {
    fn load(&mut self) -> Result<LoadedPublication, ContractError> {
        let state = self.state.lock().unwrap();
        if let Some(generation) = state.fenced {
            return Ok(LoadedPublication::Fenced { generation });
        }
        if state.load_error {
            return Err(
                FailureCode::StateUnavailable.failure(PLUGIN_ID, "injected store load failure")
            );
        }
        Ok(match state.pending.clone() {
            Some(publication) => LoadedPublication::Pending {
                publication,
                checkpoint: state.checkpoint.clone(),
            },
            None => LoadedPublication::Empty {
                checkpoint: state.checkpoint.clone(),
            },
        })
    }

    fn begin(&mut self, publication: &ReportPublication) -> Result<(), ContractError> {
        let mut state = self.state.lock().unwrap();
        if state.begin_error {
            return Err(
                FailureCode::StateUnavailable.failure(PLUGIN_ID, "injected store begin failure")
            );
        }
        if state.pending.is_some() {
            return Err(FailureCode::InvalidRecord
                .failure(PLUGIN_ID, "a pending publication already exists"));
        }
        if publication.binding.generation != state.generation {
            return Err(FailureCode::StaleGeneration.failure(
                PLUGIN_ID,
                "publication generation is not the store generation",
            ));
        }
        state.pending = Some(publication.clone());
        Ok(())
    }

    fn acknowledge(&mut self, envelope: &ReportEnvelope) -> Result<AckCheckpoint, ContractError> {
        let mut state = self.state.lock().unwrap();
        if state.ack_error {
            return Err(FailureCode::StateUnavailable
                .failure(PLUGIN_ID, "injected store acknowledgement failure"));
        }
        let pending = state.pending.clone().ok_or_else(|| {
            FailureCode::StateUnavailable.failure(PLUGIN_ID, "no pending publication")
        })?;
        if envelope.plugin_id != pending.plugin_id
            || envelope.binding != pending.binding
            || envelope.sequence != pending.sequence
            || envelope.payload_digest != pending.payload_digest
            || envelope.schema_namespace != pending.schema_namespace
            || envelope.schema_version != pending.schema_version
            || envelope.payload != pending.payload
        {
            return Err(FailureCode::IntegrityMismatch.failure(
                PLUGIN_ID,
                "acknowledgement does not match the pending record",
            ));
        }
        let checkpoint = AckCheckpoint {
            generation: pending.binding.generation,
            sequence: pending.sequence,
            payload_digest: pending.payload_digest.clone(),
            acknowledged_at_unix_ms: self.clock.now(),
        };
        state.checkpoint = Some(checkpoint.clone());
        state.pending = None;
        Ok(checkpoint)
    }
}

#[derive(Clone, Debug)]
enum Reply {
    Ack,
    LostResponse,
    UnknownOutcome,
    Malformed,
    Timeout,
    WrongSequence,
    WrongDigest,
    WrongBinding,
    WrongPlugin,
    WrongExecution,
    WrongDurability,
    StaleGeneration,
    AuthorityDenied,
    IdentityConflict,
    Protocol,
}

struct ScriptedTransport {
    replies: VecDeque<Reply>,
    default: Reply,
    sent: Vec<Value>,
    observed_at_unix_ms: u64,
}

impl ScriptedTransport {
    fn new(default: Reply) -> Self {
        Self {
            replies: VecDeque::new(),
            default,
            sent: Vec::new(),
            observed_at_unix_ms: START_MS + 1,
        }
    }

    fn push(&mut self, reply: Reply) {
        self.replies.push_back(reply);
    }

    fn sent_publications(&self) -> Vec<ReportPublication> {
        self.sent
            .iter()
            .map(|request| {
                serde_json::from_value(request.get("publication").cloned().unwrap()).unwrap()
            })
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
                serde_json::from_value(request.get("publication").cloned().unwrap()).unwrap();
            match reply {
                Reply::Ack => Ok(serde_json::to_value(ReportEnvelope::from_publication(
                    &publication,
                    self.observed_at_unix_ms,
                ))
                .unwrap()),
                Reply::LostResponse => Err(IpcError::Unavailable("response lost".into())),
                Reply::UnknownOutcome => {
                    Err(IpcError::unknown_outcome("effect outcome unobserved"))
                }
                Reply::Malformed => Ok(json!({"format": "ReportEnvelope/v1"})),
                Reply::Timeout => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(serde_json::to_value(ReportEnvelope::from_publication(
                        &publication,
                        self.observed_at_unix_ms,
                    ))
                    .unwrap())
                }
                Reply::WrongSequence => {
                    let mut envelope =
                        ReportEnvelope::from_publication(&publication, self.observed_at_unix_ms);
                    envelope.sequence = envelope.sequence.saturating_add(1);
                    Ok(serde_json::to_value(envelope).unwrap())
                }
                Reply::WrongDigest => {
                    let mut envelope =
                        ReportEnvelope::from_publication(&publication, self.observed_at_unix_ms);
                    envelope.payload_digest = canonical_digest(&json!({"other": true})).unwrap();
                    Ok(serde_json::to_value(envelope).unwrap())
                }
                Reply::WrongBinding => {
                    let mut envelope =
                        ReportEnvelope::from_publication(&publication, self.observed_at_unix_ms);
                    envelope.binding.implementation_version = "9.9.9".into();
                    Ok(serde_json::to_value(envelope).unwrap())
                }
                Reply::WrongPlugin => {
                    let mut envelope =
                        ReportEnvelope::from_publication(&publication, self.observed_at_unix_ms);
                    envelope.plugin_id = OTHER_PLUGIN_ID.into();
                    Ok(serde_json::to_value(envelope).unwrap())
                }
                Reply::WrongExecution => {
                    let mut envelope =
                        ReportEnvelope::from_publication(&publication, self.observed_at_unix_ms);
                    envelope.execution_id = Some("exec-unexpected".into());
                    Ok(serde_json::to_value(envelope).unwrap())
                }
                Reply::WrongDurability => {
                    let mut envelope =
                        ReportEnvelope::from_publication(&publication, self.observed_at_unix_ms);
                    envelope.durability = Durability::Transient;
                    Ok(serde_json::to_value(envelope).unwrap())
                }
                Reply::StaleGeneration => Err(IpcError::Contract(
                    FailureCode::StaleGeneration.failure(PLUGIN_ID, "retired generation"),
                )),
                Reply::AuthorityDenied => Err(IpcError::Contract(
                    FailureCode::AuthorityDenied.failure(PLUGIN_ID, "grant expired"),
                )),
                Reply::IdentityConflict => Err(IpcError::Contract(
                    FailureCode::IdentityConflict.failure(PLUGIN_ID, "changed content"),
                )),
                Reply::Protocol => Err(IpcError::Protocol("bad frame".into())),
            }
        })
    }
}

fn open_publisher(
    store: FakeStore,
    clock: &Clock,
    limits: PublisherLimits,
) -> Result<ReportPublisher<FakeStore>, ContractError> {
    ReportPublisher::open(store, publisher_binding(1), limits, clock.arc())
}

fn assert_state(
    publisher: &ReportPublisher<FakeStore>,
    store: &FakeStore,
    pending: Option<&ReportPublication>,
    checkpoint_sequence: Option<u64>,
) {
    assert_eq!(publisher.pending_publication(), pending);
    assert_eq!(store.snapshot().pending.as_ref(), pending);
    assert_eq!(
        publisher.checkpoint().map(|cp| cp.sequence),
        checkpoint_sequence
    );
    assert_eq!(
        store.snapshot().checkpoint.as_ref().map(|cp| cp.sequence),
        checkpoint_sequence
    );
    assert!(store.pending_count() <= 1);
}

fn assert_failure(code: FailureCode, result: Result<impl std::fmt::Debug, ContractError>) {
    let error = result.expect_err("operation should fail");
    assert_eq!(
        failure_code(&error),
        Some(code),
        "unexpected error: {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Concrete regression traces
// ---------------------------------------------------------------------------

#[test]
fn defaults_match_the_frozen_cw0_bounds() {
    let limits = PublisherLimits::default();
    assert_eq!(limits.max_payload_bytes, 64 * 1024);
    assert_eq!(limits.max_payload_bytes, DEFAULT_MAX_PAYLOAD_BYTES);
    assert_eq!(limits.publish_deadline_ms, 5_000);
    assert_eq!(limits.publish_deadline_ms, DEFAULT_PUBLISH_DEADLINE_MS);
    assert_eq!(limits.coalesce_after_ms, 5_000);
    assert_eq!(limits.coalesce_after_ms, DEFAULT_COALESCE_AFTER_MS);
    assert_eq!(limits.max_retry_backoff_ms, 5_000);
    assert_eq!(limits.max_retry_backoff_ms, DEFAULT_MAX_RETRY_BACKOFF_MS);
    assert_eq!(INITIAL_RETRY_BACKOFF_MS, 250);
    assert_eq!(REPORT_PAYLOAD_LIMIT_BYTES, 64 * 1024);
}

#[tokio::test]
async fn crash_before_send_restart_replays_exact_persisted_bytes() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();

    let sealed = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    assert_eq!(sealed.sequence, 1);
    assert_eq!(sealed.durability, Durability::Durable);
    assert_eq!(sealed.execution_id, None);
    assert_state(&publisher, &store, Some(&sealed), None);

    // Simulated crash before send: drop the in-memory publisher.
    let persisted = store.snapshot().pending.unwrap();
    drop(publisher);
    let mut restarted = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    assert_state(&restarted, &store, Some(&persisted), None);

    // No new sequence may be allocated while the record is pending.
    assert_failure(
        FailureCode::StateUnavailable,
        restarted.seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": false}),
            START_MS,
        ),
    );
    assert_state(&restarted, &store, Some(&persisted), None);

    let mut transport = ScriptedTransport::new(Reply::Ack);
    let outcome = restarted
        .publish_pending(&mut transport, "grant-token")
        .await
        .unwrap();
    let PublishOutcome::Acknowledged(envelope) = outcome else {
        panic!("expected acknowledgement, got {outcome:?}");
    };
    assert_eq!(envelope.sequence, 1);
    assert_eq!(envelope.payload_digest, persisted.payload_digest);
    assert_state(&restarted, &store, None, Some(1));
    assert_eq!(transport.sent_publications(), vec![persisted]);
}

#[tokio::test]
async fn lost_ack_retains_pending_and_retries_identical_after_backoff() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    let sealed = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::Ack);
    transport.push(Reply::LostResponse);

    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Pending);
    assert_state(&publisher, &store, Some(&sealed), None);
    assert_eq!(publisher.retry_at_ms(), Some(START_MS + 250));
    assert_eq!(publisher.next_retry_backoff_ms(), 500);

    // No send before the backoff boundary.
    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Pending);
    assert_eq!(transport.sent.len(), 1);

    clock.advance(250);
    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert!(matches!(outcome, PublishOutcome::Acknowledged(_)));
    assert_eq!(transport.sent.len(), 2);
    assert_eq!(transport.sent[0], transport.sent[1]);
    assert_state(&publisher, &store, None, Some(1));
    assert_eq!(publisher.next_retry_backoff_ms(), 250);
}

#[tokio::test]
async fn retry_backoff_doubles_to_ceiling_and_resets_after_ack() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut limits = test_limits();
    limits.max_retry_backoff_ms = 1_000;
    let mut publisher = open_publisher(store.clone(), &clock, limits).unwrap();
    publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::LostResponse);

    let expected = [250u64, 500, 1_000, 1_000];
    for delay in expected {
        let now = clock.now();
        let outcome = publisher
            .publish_pending(&mut transport, "grant")
            .await
            .unwrap();
        assert_eq!(outcome, PublishOutcome::Pending);
        assert_eq!(publisher.retry_at_ms(), Some(now + delay));
        clock.advance(delay);
    }
    assert_eq!(transport.sent.len(), expected.len());

    transport.default = Reply::Ack;
    let before = clock.now();
    let sent = transport.sent.len();
    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert!(matches!(outcome, PublishOutcome::Acknowledged(_)));
    assert_eq!(transport.sent.len(), sent + 1);
    assert!(clock.now() >= before);
    assert_eq!(publisher.next_retry_backoff_ms(), 250);
}

#[tokio::test]
async fn deadline_expiry_retains_the_immutable_pending_record() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut limits = test_limits();
    limits.publish_deadline_ms = 5;
    let mut publisher = open_publisher(store.clone(), &clock, limits).unwrap();
    let sealed = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::Timeout);

    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Pending);
    assert_state(&publisher, &store, Some(&sealed), None);
    assert_eq!(transport.sent_publications(), vec![sealed]);
}

#[tokio::test]
async fn invalid_acknowledgements_never_advance_the_checkpoint() {
    let cases = [
        Reply::Malformed,
        Reply::Protocol,
        Reply::WrongSequence,
        Reply::WrongDigest,
        Reply::WrongBinding,
        Reply::WrongPlugin,
        Reply::WrongExecution,
        Reply::WrongDurability,
    ];
    for reply in cases {
        let clock = Clock::new(START_MS);
        let store = FakeStore::new(1, clock.clone());
        let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
        let sealed = publisher
            .seal(
                SCHEMA_NAMESPACE,
                SCHEMA_VERSION,
                json!({"ready": true}),
                START_MS,
            )
            .unwrap();
        let mut transport = ScriptedTransport::new(reply.clone());
        let result = publisher.publish_pending(&mut transport, "grant").await;
        assert!(result.is_err(), "reply {reply:?} was accepted: {result:?}");
        assert_state(&publisher, &store, Some(&sealed), None);
        assert_eq!(transport.sent_publications(), vec![sealed.clone()]);
    }
}

#[tokio::test]
async fn changed_content_conflict_surfaces_and_replays_the_same_sequence() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    let sealed = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::IdentityConflict);

    let error = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .expect_err("identity conflict must surface");
    assert_eq!(failure_code(&error), Some(FailureCode::IdentityConflict));
    assert_state(&publisher, &store, Some(&sealed), None);

    clock.advance(publisher.retry_at_ms().unwrap() - clock.now());
    transport.default = Reply::Ack;
    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert!(matches!(outcome, PublishOutcome::Acknowledged(_)));
    assert_state(&publisher, &store, None, Some(1));
    let sent = transport.sent_publications();
    assert_eq!(sent, vec![sealed.clone(), sealed]);
}

#[tokio::test]
async fn store_failures_retain_pending_and_do_not_allocate_again() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    store.set_begin_error(true);
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    assert_failure(
        FailureCode::StateUnavailable,
        publisher.seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        ),
    );
    assert_state(&publisher, &store, None, None);

    store.set_begin_error(false);
    let sealed = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    store.set_ack_error(true);
    let mut transport = ScriptedTransport::new(Reply::Ack);
    let error = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .expect_err("store acknowledgement failure must surface");
    assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));
    assert_state(&publisher, &store, Some(&sealed), None);

    // A retry after the durable store recovers replays the identical record.
    clock.advance(publisher.retry_at_ms().unwrap() - clock.now());
    store.set_ack_error(false);
    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert!(matches!(outcome, PublishOutcome::Acknowledged(_)));
    assert_state(&publisher, &store, None, Some(1));
    assert_eq!(transport.sent_publications(), vec![sealed.clone(), sealed]);
}

#[tokio::test]
async fn stale_generation_response_fences_without_dropping_the_outbox() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    let sealed = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::StaleGeneration);

    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Fenced);
    assert!(publisher.is_fenced());
    assert_state(&publisher, &store, Some(&sealed), None);

    let sent = transport.sent.len();
    let outcome = publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Fenced);
    assert_eq!(transport.sent.len(), sent);
    assert_failure(
        FailureCode::StaleGeneration,
        publisher.seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            clock.now(),
        ),
    );
    assert_state(&publisher, &store, Some(&sealed), None);
}

#[tokio::test]
async fn expired_grant_fences_and_never_widens_authority() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    let sealed = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            START_MS,
        )
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::AuthorityDenied);
    let outcome = publisher
        .publish_pending(&mut transport, "expired")
        .await
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Fenced);
    assert_state(&publisher, &store, Some(&sealed), None);
}

#[test]
fn fenced_store_is_refused_without_reading_or_rewriting_its_sequence() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(9, clock.clone());
    store.fence(9);
    let error = open_publisher(store.clone(), &clock, test_limits())
        .expect_err("fenced store must refuse open");
    assert_eq!(failure_code(&error), Some(FailureCode::StaleGeneration));
    assert_eq!(store.pending_count(), 0);
    assert_eq!(store.snapshot().checkpoint, None);

    // A future-generation store is also fenced for this publisher.
    let next_store = FakeStore::new(2, clock.clone());
    next_store.fence(2);
    let error = ReportPublisher::open(
        next_store.clone(),
        publisher_binding(1),
        test_limits(),
        clock.arc(),
    )
    .expect_err("mismatched generation must refuse open");
    assert_eq!(failure_code(&error), Some(FailureCode::StaleGeneration));
}

#[test]
fn corrupt_or_gapped_store_state_never_resets_to_zero() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    {
        let mut state = store.state.lock().unwrap();
        let publication = ReportPublication {
            format: "ReportPublication/v1".into(),
            plugin_id: PLUGIN_ID.into(),
            binding: binding_pin(1),
            schema_namespace: SCHEMA_NAMESPACE.into(),
            schema_version: SCHEMA_VERSION,
            sequence: 5,
            reported_at_unix_ms: START_MS,
            execution_id: None,
            durability: Durability::Durable,
            payload: json!({"ready": true}),
            payload_digest: canonical_digest(&json!({"ready": true})).unwrap(),
        };
        state.pending = Some(publication);
    }
    let error = open_publisher(store.clone(), &clock, test_limits())
        .expect_err("gapped pending record must refuse open");
    assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));

    store.set_load_error(true);
    store.state.lock().unwrap().pending = None;
    let error = open_publisher(store.clone(), &clock, test_limits())
        .expect_err("store load failure must refuse open");
    assert_eq!(failure_code(&error), Some(FailureCode::StateUnavailable));
}

#[tokio::test]
async fn sequence_overflow_refuses_new_source_publications() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    store.state.lock().unwrap().checkpoint = Some(AckCheckpoint {
        generation: 1,
        sequence: u64::MAX,
        payload_digest: canonical_digest(&json!({"ready": true})).unwrap(),
        acknowledged_at_unix_ms: START_MS,
    });
    clock.advance(5_000);
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    assert_failure(
        FailureCode::StateUnavailable,
        publisher.seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"ready": true}),
            clock.now(),
        ),
    );
    assert_eq!(store.pending_count(), 0);
    assert_eq!(publisher.checkpoint().unwrap().sequence, u64::MAX);
}

#[test]
fn undeclared_schema_and_payload_bounds_fail_locally_without_a_sequence() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut limits = test_limits();
    limits.max_payload_bytes = 32;
    let mut publisher = open_publisher(store.clone(), &clock, limits).unwrap();

    assert_failure(
        FailureCode::UnsupportedSchema,
        publisher.seal("example.undeclared", SCHEMA_VERSION, json!({}), START_MS),
    );
    assert_failure(
        FailureCode::UnsupportedSchema,
        publisher.seal(SCHEMA_NAMESPACE, 2, json!({}), START_MS),
    );
    assert_failure(
        FailureCode::InvalidRecord,
        publisher.seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"value": "x".repeat(64)}),
            START_MS,
        ),
    );
    assert_state(&publisher, &store, None, None);
    assert_eq!(store.snapshot().pending, None);

    let accepted = publisher
        .seal(SCHEMA_NAMESPACE, SCHEMA_VERSION, json!({"v": 1}), START_MS)
        .unwrap();
    assert_eq!(accepted.sequence, 1);
}

#[test]
fn exactly_the_64k_canonical_payload_boundary_is_accepted() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, PublisherLimits::default()).unwrap();
    let at_limit = "a".repeat(65_534); // + two quote bytes == 65_536
    assert_eq!(canonical_json(&json!(at_limit)).unwrap().len(), 65_536);
    let publication = publisher
        .seal(SCHEMA_NAMESPACE, SCHEMA_VERSION, json!(at_limit), START_MS)
        .unwrap();
    assert_eq!(
        publication.payload_digest,
        canonical_digest(&publication.payload).unwrap()
    );

    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, PublisherLimits::default()).unwrap();
    let over_limit = "a".repeat(65_535); // 65_537 canonical bytes
    assert_failure(
        FailureCode::InvalidRecord,
        publisher.seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!(over_limit),
            START_MS,
        ),
    );
    assert_eq!(store.pending_count(), 0);
}

#[tokio::test]
async fn coalescing_window_gates_the_next_sequence_and_restart_respects_the_checkpoint() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    publisher
        .seal(SCHEMA_NAMESPACE, SCHEMA_VERSION, json!({"n": 1}), START_MS)
        .unwrap();
    let mut transport = ScriptedTransport::new(Reply::Ack);
    publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    assert_eq!(publisher.next_seal_at_ms(), Some(clock.now() + 5_000));

    clock.advance(4_999);
    assert_failure(
        FailureCode::StateUnavailable,
        publisher.seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"n": 2}),
            clock.now(),
        ),
    );
    clock.advance(1);
    let second = publisher
        .seal(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"n": 2}),
            clock.now(),
        )
        .unwrap();
    assert_eq!(second.sequence, 2);

    drop(publisher);
    drop(transport);
    let restarted = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    assert_eq!(restarted.pending_publication(), Some(&second));
    assert_eq!(restarted.checkpoint().unwrap().sequence, 1);
    assert_eq!(restarted.next_seal_at_ms(), None); // pending forbids seals first
}

#[tokio::test]
async fn execution_reference_is_only_present_when_caller_supplied() {
    let clock = Clock::new(START_MS);
    let store = FakeStore::new(1, clock.clone());
    let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
    let first = publisher
        .seal(SCHEMA_NAMESPACE, SCHEMA_VERSION, json!({"n": 1}), START_MS)
        .unwrap();
    assert_eq!(first.execution_id, None);
    let mut transport = ScriptedTransport::new(Reply::Ack);
    publisher
        .publish_pending(&mut transport, "grant")
        .await
        .unwrap();
    clock.advance(5_000);
    let second = publisher
        .seal_with_execution(
            SCHEMA_NAMESPACE,
            SCHEMA_VERSION,
            json!({"n": 2}),
            clock.now(),
            Some("exec-1".into()),
        )
        .unwrap();
    assert_eq!(second.execution_id.as_deref(), Some("exec-1"));
    assert_eq!(second.durability, Durability::Durable);
}

// ---------------------------------------------------------------------------
// Deterministic generated property
// ---------------------------------------------------------------------------

struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[tokio::test]
async fn property_report_sequences_preserve_outbox_replay_and_fencing_invariants() {
    for case in 0..PROPERTY_CASES {
        let clock = Clock::new(START_MS);
        let store = FakeStore::new(1, clock.clone());
        let mut publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
        let mut transport = ScriptedTransport::new(Reply::Ack);
        let mut rng = XorShift64::new(PROPERTY_SEED ^ case.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut acked_sequence = 0u64;
        let mut sent_identities: BTreeMap<(u64, u64), String> = BTreeMap::new();

        for _step in 0..PROPERTY_STEPS {
            match rng.next() % 10 {
                0 | 1 | 2 => {
                    let now = clock.now();
                    let invalid_schema = rng.next() % 13 == 0;
                    let payload = json!({"tick": rng.next() % 5, "text": "bounded"});
                    let before_pending = publisher.pending_publication().cloned();
                    let before_store_pending = store.snapshot().pending;
                    let before_checkpoint = publisher.checkpoint().cloned();
                    let result = if invalid_schema {
                        publisher.seal("example.undeclared", 1, payload, now)
                    } else {
                        publisher.seal(SCHEMA_NAMESPACE, SCHEMA_VERSION, payload, now)
                    };
                    match result {
                        Ok(publication) => {
                            assert!(before_pending.is_none());
                            assert!(before_store_pending.is_none());
                            assert!(publisher
                                .next_seal_at_ms()
                                .map_or(true, |ready| now >= ready));
                            assert_eq!(publication.sequence, acked_sequence + 1);
                            assert_eq!(publication.binding.generation, 1);
                            assert_eq!(publication.durability, Durability::Durable);
                            assert_eq!(publisher.pending_publication(), Some(&publication));
                            assert_eq!(store.snapshot().pending.as_ref(), Some(&publication));
                            assert_eq!(publisher.checkpoint(), before_checkpoint.as_ref());
                        }
                        Err(error) => {
                            assert!(
                                before_pending.is_some()
                                    || invalid_schema
                                    || matches!(
                                        failure_code(&error),
                                        Some(
                                            FailureCode::StateUnavailable
                                                | FailureCode::UnsupportedSchema
                                                | FailureCode::InvalidRecord
                                        )
                                    ),
                                "unexpected seal failure: {error:?}"
                            );
                            assert_eq!(publisher.pending_publication(), before_pending.as_ref());
                            assert_eq!(store.snapshot().pending, before_store_pending);
                            assert_eq!(publisher.checkpoint(), before_checkpoint.as_ref());
                        }
                    }
                }
                3 | 4 | 5 => {
                    let before_pending = publisher.pending_publication().cloned();
                    let before_ack = acked_sequence;
                    if before_pending.is_none() {
                        let result = publisher.publish_pending(&mut transport, "grant").await;
                        assert!(result.is_err());
                        assert_eq!(store.pending_count(), 0);
                        continue;
                    }
                    let now = clock.now();
                    let backoff_wait = publisher
                        .retry_at_ms()
                        .is_some_and(|retry_at| now < retry_at);
                    let sent_before = transport.sent.len();
                    let reply = match rng.next() % 8 {
                        0 => Reply::LostResponse,
                        1 => Reply::UnknownOutcome,
                        2 => Reply::Malformed,
                        3 => Reply::IdentityConflict,
                        _ => Reply::Ack,
                    };
                    if !backoff_wait {
                        transport.push(reply.clone());
                    }
                    let result = publisher.publish_pending(&mut transport, "grant").await;
                    if backoff_wait {
                        assert_eq!(result.unwrap(), PublishOutcome::Pending);
                        assert_eq!(transport.sent.len(), sent_before);
                    } else {
                        match reply {
                            Reply::Ack => {
                                let PublishOutcome::Acknowledged(envelope) = result.unwrap() else {
                                    panic!("ack case did not acknowledge");
                                };
                                acked_sequence += 1;
                                assert_eq!(envelope.sequence, acked_sequence);
                                assert_eq!(
                                    publisher.checkpoint().map(|cp| cp.sequence),
                                    Some(acked_sequence)
                                );
                                assert!(publisher.pending_publication().is_none());
                                assert_eq!(store.pending_count(), 0);
                            }
                            Reply::LostResponse | Reply::UnknownOutcome => {
                                assert_eq!(result.unwrap(), PublishOutcome::Pending);
                                assert_eq!(
                                    publisher.pending_publication(),
                                    before_pending.as_ref()
                                );
                                assert_eq!(store.pending_count(), 1);
                            }
                            Reply::Malformed | Reply::IdentityConflict => {
                                let error = result.expect_err("invalid acknowledgement must fail");
                                assert_eq!(
                                    publisher.pending_publication(),
                                    before_pending.as_ref()
                                );
                                assert_eq!(store.pending_count(), 1);
                                let _ = error;
                            }
                            _ => unreachable!(),
                        }
                    }
                    assert!(acked_sequence == before_ack || acked_sequence == before_ack + 1);
                }
                6 => {
                    clock.advance(rng.next() % 7_000);
                }
                7 => {
                    let pending = publisher.pending_publication().cloned();
                    let checkpoint = publisher.checkpoint().cloned();
                    drop(publisher);
                    publisher = open_publisher(store.clone(), &clock, test_limits()).unwrap();
                    assert_eq!(publisher.pending_publication(), pending.as_ref());
                    assert_eq!(publisher.checkpoint(), checkpoint.as_ref());
                }
                8 => {
                    // No identity reuse with changed content: every request
                    // carrying one identity has byte-identical publication JSON.
                    for request in &transport.sent {
                        let publication: ReportPublication =
                            serde_json::from_value(request["publication"].clone()).unwrap();
                        let key = (publication.binding.generation, publication.sequence);
                        let encoded = serde_json::to_string(&publication).unwrap();
                        match sent_identities.get(&key) {
                            Some(previous) => assert_eq!(previous, &encoded),
                            None => {
                                sent_identities.insert(key, encoded);
                            }
                        }
                    }
                    assert_eq!(
                        store.pending_count(),
                        usize::from(publisher.pending_publication().is_some())
                    );
                }
                _ => {
                    assert!(store.pending_count() <= 1);
                    if let Some(pending) = publisher.pending_publication() {
                        assert_eq!(pending.sequence, acked_sequence + 1);
                        assert_eq!(pending.plugin_id, PLUGIN_ID);
                        assert_eq!(pending.binding.generation, 1);
                        assert_eq!(store.snapshot().pending.as_ref(), Some(pending));
                        pending.validate().unwrap();
                    }
                }
            }
        }

        // Drain any pending record, then verify contiguous acknowledged state.
        if publisher.pending_publication().is_some() {
            loop {
                let now = clock.now();
                if publisher
                    .retry_at_ms()
                    .is_some_and(|retry_at| now < retry_at)
                {
                    clock.advance(publisher.retry_at_ms().unwrap() - now);
                }
                transport.default = Reply::Ack;
                if matches!(
                    publisher
                        .publish_pending(&mut transport, "grant")
                        .await
                        .unwrap(),
                    PublishOutcome::Acknowledged(_)
                ) {
                    acked_sequence += 1;
                    break;
                }
            }
        }
        assert_eq!(publisher.pending_publication(), None);
        assert_eq!(
            publisher.checkpoint().map(|cp| cp.sequence),
            Some(acked_sequence)
        );
        assert_eq!(store.pending_count(), 0);
        assert_eq!(
            store.snapshot().checkpoint.as_ref().map(|cp| cp.sequence),
            Some(acked_sequence)
        );
        // A restart after the final acknowledgement is a no-op.
        let checkpoint = publisher.checkpoint().cloned();
        drop(publisher);
        let restarted = open_publisher(store.clone(), &clock, test_limits()).unwrap();
        assert_eq!(restarted.checkpoint(), checkpoint.as_ref());
        assert_eq!(restarted.pending_publication(), None);
        assert_eq!(
            restarted.checkpoint().map(|cp| cp.sequence),
            Some(acked_sequence)
        );
    }
}
