//! WEB.9 `FactoryRuntime/v1` + `FactoryControl/v1` contract acceptance tests
//! (`agalma-rwn.9`).
//!
//! Independent wire fixtures live in `fixtures/factory-runtime-v1/` and
//! `fixtures/factory-control-v1/`; they are hand-written, not generated from the
//! Rust types. Assertions use documented `FailureDetail/v1` codes rather than
//! implementation internals. No network, no `/tmp`, no temp files.
//!
//! Replay:
//! `make test-focused SELECTION='-p agalma-contracts --test factory_contracts'`.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};

use agalma_contracts::digest::{canonical_digest, failure_code, is_sha256, FailureCode};
use agalma_contracts::{
    check_factory_cursor, ExactU64, FactoryActionRequest, FactoryControlActor,
    FactoryControlOutcome, FactoryControlReceiptBody, FactoryControlRequest,
    FactoryControlResponse, FactoryControlResult, FactoryEligibleEntry, FactoryEligiblePage,
    FactoryLookupRequest, FactoryQueueCursor, FactoryRuntime, FactoryRuntimePermissions,
    FactoryRuntimeRequest, FactoryRuntimeStatus, FactorySourceName, RuntimeObservationSource,
    SourceState, TaskFamily, TaskPriority, FACTORY_CONTROL_CAPABILITY, FACTORY_CONTROL_FORMAT,
    FACTORY_CURSOR_MAX_BYTES, FACTORY_PAGE_MAX_ENTRIES, FACTORY_QUEUE_CURSOR_FORMAT,
    FACTORY_READ_CAPABILITY, FACTORY_RUNTIME_FORMAT, FACTORY_SNAPSHOT_DEFAULT_LIMIT,
    FACTORY_SNAPSHOT_MAX_LIMIT, FACTORY_SNAPSHOT_MIN_LIMIT, FACTORY_TITLE_MAX_BYTES,
};

// --- fixtures ---------------------------------------------------------------

macro_rules! runtime_fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/factory-runtime-v1/",
            $name
        ))
    };
}

macro_rules! control_fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/factory-control-v1/",
            $name
        ))
    };
}

const RUNTIME_STATUSES_JSON: &str = runtime_fixture!("runtime-statuses.json");
const RUNTIME_UNAVAILABLE_JSON: &str = runtime_fixture!("runtime-unavailable.json");
const ELIGIBLE_QUEUE_JSON: &str = runtime_fixture!("eligible-queue.json");
const BOUNDARIES_JSON: &str = runtime_fixture!("boundaries.json");
const PERMISSIONS_JSON: &str = runtime_fixture!("permissions.json");
const CURSOR_JSON: &str = runtime_fixture!("cursor.json");
const RUNTIME_REQUESTS_JSON: &str = runtime_fixture!("runtime-requests.json");
const RUNTIME_INVALID_JSON: &str = runtime_fixture!("runtime-invalid.json");
const CONTROL_REQUESTS_JSON: &str = control_fixture!("requests.json");
const CONTROL_RESPONSES_JSON: &str = control_fixture!("responses.json");
const CONTROL_INVALID_JSON: &str = control_fixture!("control-invalid.json");

/// Count of executed required cases; the final test asserts it is nonzero.
static CASES: AtomicUsize = AtomicUsize::new(0);

fn tick() -> usize {
    CASES.fetch_add(1, Ordering::SeqCst) + 1
}

fn assert_code(error: agalma_contracts::ContractError, expected: &str) {
    assert_eq!(
        failure_code(&error).map(FailureCode::as_str),
        Some(expected),
        "error was: {error}"
    );
}

fn parse_value(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture is JSON")
}

/// Expand generated value descriptors: `{"fill","len"}` becomes a repeated
/// string and `{"repeat","value"}` becomes an array of copies. Any other value
/// is expanded recursively.
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            if map.len() == 2 && map.contains_key("fill") && map.contains_key("len") {
                let fill = map["fill"].as_str().expect("fill is a string");
                let len = map["len"].as_u64().expect("len is a u64") as usize;
                return Value::String(fill.repeat(len));
            }
            if map.len() == 2 && map.contains_key("repeat") && map.contains_key("value") {
                let n = map["repeat"].as_u64().expect("repeat is a u64") as usize;
                let inner = expand(&map["value"]);
                return Value::Array(vec![inner; n]);
            }
            Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), expand(item)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        other => other.clone(),
    }
}

fn descend<'a>(current: &'a mut Value, part: &str) -> &'a mut Value {
    match current {
        Value::Object(map) => map
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new())),
        Value::Array(items) => {
            let index: usize = part
                .parse()
                .unwrap_or_else(|_| panic!("array index {part:?}"));
            &mut items[index]
        }
        _ => panic!("path parent is not an object or array"),
    }
}

fn assign(current: &mut Value, part: &str, new_value: Value) {
    match current {
        Value::Object(map) => {
            map.insert(part.to_string(), new_value);
        }
        Value::Array(items) => {
            let index: usize = part.parse().expect("array index");
            items[index] = new_value;
        }
        _ => panic!("path parent is not an object or array"),
    }
}

fn set_path(object: &mut Value, path: &str, new_value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = object;
    for (index, part) in parts.iter().enumerate() {
        if index + 1 == parts.len() {
            assign(current, part, new_value);
            return;
        }
        current = descend(current, part);
    }
}

fn remove_path(object: &mut Value, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = object;
    for (index, part) in parts.iter().enumerate() {
        if index + 1 == parts.len() {
            if let Value::Object(map) = current {
                map.remove(*part);
            }
            return;
        }
        match current {
            Value::Object(map) => match map.get_mut(*part) {
                Some(next) => current = next,
                None => return,
            },
            Value::Array(items) => {
                let slot: usize = part.parse().expect("array index");
                current = &mut items[slot];
            }
            _ => return,
        }
    }
}

/// Apply a case's `set`/`remove` patch to `base`, expanding descriptors.
fn patched(base: &Value, case: &Value) -> Value {
    let mut value = base.clone();
    if let Some(set) = case.get("set").and_then(Value::as_object) {
        for (path, raw) in set {
            set_path(&mut value, path, expand(raw));
        }
    }
    if let Some(remove) = case.get("remove").and_then(Value::as_array) {
        for path in remove {
            remove_path(&mut value, path.as_str().expect("remove path is a string"));
        }
    }
    value
}

/// A valid runtime record for the generated property/mock cases.
fn runtime_stub() -> FactoryRuntime {
    FactoryRuntime {
        format: FACTORY_RUNTIME_FORMAT.to_string(),
        repository_id: "agalma".to_string(),
        state_id: "state-stub".to_string(),
        runtime_instance_id: Some("rt-stub".to_string()),
        observed_at_unix_ms: ExactU64::from(1_791_820_800_000u64),
        status: FactoryRuntimeStatus::Idle,
        reason: None,
        sources: FactorySourceName::ALL
            .iter()
            .map(|&source| agalma_contracts::FactoryRuntimeSource {
                source,
                state: SourceState::Available,
                detail: None,
                observed_at_unix_ms: Some(ExactU64::from(1_791_820_800_000u64)),
            })
            .collect(),
        stop: None,
        current_execution: None,
        eligible: FactoryEligiblePage {
            entries: Vec::new(),
            revision: None,
            total: None,
            next_cursor: None,
        },
        permissions: Default::default(),
    }
}

// --- 1. golden statuses and source states ----------------------------------

#[test]
fn golden_statuses_cover_every_status_and_source_state() {
    tick();
    let records: Vec<FactoryRuntime> =
        serde_json::from_str(RUNTIME_STATUSES_JSON).expect("statuses parse");
    assert_eq!(records.len(), 7, "one record per status");

    let mut statuses: Vec<FactoryRuntimeStatus> = Vec::new();
    let mut states: Vec<SourceState> = Vec::new();
    for record in &records {
        record.validate().expect("status record validates");
        assert_eq!(record.sources.len(), 5, "exactly five named sources");
        if !statuses.contains(&record.status) {
            statuses.push(record.status);
        }
        for source in &record.sources {
            if !states.contains(&source.state) {
                states.push(source.state);
            }
        }
    }
    assert_eq!(statuses.len(), 7, "all seven statuses are covered");
    assert_eq!(states.len(), 5, "all five source states are covered");

    // The set of sources present is exactly the five required names.
    for record in &records {
        let mut names: Vec<FactorySourceName> =
            record.sources.iter().map(|source| source.source).collect();
        names.sort_by_key(|name| format!("{name:?}"));
        names.dedup();
        assert_eq!(names.len(), 5, "source names are unique");
    }
}

// --- 2. golden unavailable with null identity ------------------------------

#[test]
fn golden_unavailable_has_null_identity() {
    tick();
    let record: FactoryRuntime =
        serde_json::from_str(RUNTIME_UNAVAILABLE_JSON).expect("unavailable parses");
    record.validate().expect("unavailable validates");
    assert_eq!(record.status, FactoryRuntimeStatus::Unavailable);
    assert!(record.runtime_instance_id.is_none(), "null identity");
    assert!(record.current_execution.is_none(), "unknown, not a stub");
    assert!(record.stop.is_none(), "unknown, not a stub");
    assert!(record.eligible.entries.is_empty());
    assert!(
        record.eligible.total.is_none(),
        "unknown total is null, not 0"
    );
    assert!(record.eligible.revision.is_none());
    assert!(!record.permissions.pause);
    assert!(!record.permissions.resume);
    assert_eq!(record.observed_at_unix_ms.as_str(), "9223372036854775807");
}

// --- 3. golden eligible queue pages ----------------------------------------

#[test]
fn golden_eligible_pages_and_cursor() {
    tick();
    let doc = parse_value(ELIGIBLE_QUEUE_JSON);
    let page = |name: &str| -> FactoryEligiblePage {
        serde_json::from_value(doc[name].clone()).expect("page parses")
    };

    let empty = page("empty");
    empty.validate().expect("empty page validates");
    assert_eq!(empty.entries.len(), 0);
    assert!(empty.revision.is_none() && empty.total.is_none() && empty.next_cursor.is_none());

    let partial = page("partial");
    partial.validate().expect("partial page validates");
    assert_eq!(partial.entries.len(), 2);
    assert_eq!(partial.total.as_ref().unwrap().as_u64().unwrap(), 2);
    assert!(partial.next_cursor.is_none());
    assert!(partial.entries.len() < FACTORY_PAGE_MAX_ENTRIES);

    let paged = page("paged");
    paged.validate().expect("paged page validates");
    assert_eq!(paged.entries.len(), 3);
    assert_eq!(
        paged.total.as_ref().unwrap().as_str(),
        "9007199254740993",
        "a total beyond f64 precision is exact"
    );
    let raw = paged.next_cursor.clone().expect("continuation cursor");

    // The opaque cursor pins the queue scope/revision/runtime.
    let cursor = FactoryQueueCursor::decode(&raw).expect("cursor decodes");
    assert_eq!(cursor.repository_id, "agalma");
    assert_eq!(cursor.state_id, "state-fixture-running");
    assert_eq!(cursor.revision, "rev-fixture-43");
    assert_eq!(cursor.runtime_instance_id, "rt-fixture-4");
    cursor
        .check_binding("agalma", "state-fixture-running")
        .expect("cursor binds to its scope");
    cursor
        .check_pinned("rev-fixture-43", "rt-fixture-4")
        .expect("cursor revision/runtime are current");

    // The live source sorts by task_id; the fixture preserves display order.
    let task_ids: Vec<&str> = paged.entries.iter().map(|e| e.task_id.as_str()).collect();
    assert_eq!(task_ids, ["bench-load", "fix-answer", "mutate-core"]);
}

// --- 4. golden boundary identities -----------------------------------------

#[test]
fn golden_boundaries_are_exact() {
    tick();
    let record: FactoryRuntime = serde_json::from_str(BOUNDARIES_JSON).expect("boundaries parse");
    record.validate().expect("boundaries validate");
    assert_eq!(record.observed_at_unix_ms.as_u64().unwrap(), u64::MAX);
    assert_eq!(
        record
            .current_execution
            .as_ref()
            .unwrap()
            .generation
            .as_u64()
            .unwrap(),
        9_007_199_254_740_993,
        "generation beyond f64 precision is exact"
    );
    assert_eq!(
        record
            .current_execution
            .as_ref()
            .unwrap()
            .updated_at_unix_ms
            .as_ref()
            .unwrap()
            .as_u64()
            .unwrap(),
        u64::MAX
    );
    assert_eq!(
        record.eligible.total.as_ref().unwrap().as_u64().unwrap(),
        u64::MAX
    );
    assert_eq!(
        record.eligible.revision.as_deref(),
        Some("9007199254740993")
    );
    assert_eq!(
        record.sources[0]
            .observed_at_unix_ms
            .as_ref()
            .unwrap()
            .as_u64()
            .unwrap(),
        u64::MAX
    );

    // Serde round-trips the exact decimal strings (never floats).
    let encoded = serde_json::to_string(&record).unwrap();
    let reparsed: FactoryRuntime = serde_json::from_str(&encoded).unwrap();
    assert_eq!(reparsed, record);
    assert!(encoded.contains("\"18446744073709551615\""));
    assert!(
        !encoded.contains("18446744073709552000"),
        "no float rounding"
    );
}

// --- 5. golden permissions --------------------------------------------------

#[test]
fn golden_permissions_allowed_and_disabled() {
    tick();
    let doc = parse_value(PERMISSIONS_JSON);
    let allowed: FactoryRuntime = serde_json::from_value(doc["allowed"].clone()).unwrap();
    allowed.validate().unwrap();
    assert!(allowed.permissions.pause && allowed.permissions.resume);

    let disabled: FactoryRuntime = serde_json::from_value(doc["disabled"].clone()).unwrap();
    disabled.validate().unwrap();
    assert!(!disabled.permissions.pause && !disabled.permissions.resume);

    // The programmatic default is all-false; both wire fields are required.
    let default = FactoryRuntimePermissions::default();
    assert!(!default.pause && !default.resume);
    assert!(
        serde_json::from_value::<FactoryRuntimePermissions>(json!({})).is_err(),
        "an omitted permissions object is malformed"
    );
}

// --- 6. golden cursor encoding ---------------------------------------------

#[test]
fn golden_cursor_encoding_is_exact() {
    tick();
    let doc = parse_value(CURSOR_JSON);
    let cursor: FactoryQueueCursor =
        serde_json::from_value(doc["cursor"].clone()).expect("cursor parses");
    let encoded = cursor.encode().expect("cursor encodes");
    assert_eq!(
        encoded,
        doc["encoded"].as_str().unwrap(),
        "canonical JSON encoding is exact"
    );
    assert_eq!(FactoryQueueCursor::decode(&encoded).unwrap(), cursor);

    // A wrong repository/state scope is malformed (InvalidRecord).
    assert_code(
        FactoryQueueCursor::decode(&encoded)
            .unwrap()
            .check_binding("other-repo", "state-fixture-running")
            .unwrap_err(),
        "InvalidRecord",
    );
    // A mismatched revision/runtime is RevisionConflict.
    assert_code(
        FactoryQueueCursor::decode(&encoded)
            .unwrap()
            .check_pinned("rev-fixture-99", "rt-fixture-4")
            .unwrap_err(),
        "RevisionConflict",
    );

    // `check_factory_cursor` applies both rules.
    let bound = check_factory_cursor(
        Some(&encoded),
        "agalma",
        "state-fixture-running",
        Some("rev-fixture-43"),
        Some("rt-fixture-4"),
    )
    .unwrap()
    .expect("a valid cursor is returned");
    assert_eq!(bound, cursor);
    assert_code(
        check_factory_cursor(
            Some(&encoded),
            "other-repo",
            "state-fixture-running",
            Some("rev-fixture-43"),
            Some("rt-fixture-4"),
        )
        .unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        check_factory_cursor(
            Some(&encoded),
            "agalma",
            "state-fixture-running",
            Some("rev-fixture-99"),
            Some("rt-fixture-4"),
        )
        .unwrap_err(),
        "RevisionConflict",
    );
    assert!(
        check_factory_cursor(None, "agalma", "state-fixture-running", None, None)
            .unwrap()
            .is_none()
    );

    // Malformed tokens are InvalidRecord, never accepted.
    for bad in [
        "not json",
        "{}",
        "[]",
        "{\"format\":\"FactoryQueueCursor/v1\"}",
    ] {
        assert_code(
            FactoryQueueCursor::decode(bad).unwrap_err(),
            "InvalidRecord",
        );
    }
}

// --- 7. runtime request validation -----------------------------------------

#[test]
fn runtime_requests_accept_defaults_and_reject_overlimits() {
    tick();
    let doc = parse_value(RUNTIME_REQUESTS_JSON);

    for case in doc["valid"].as_array().unwrap() {
        let request: FactoryRuntimeRequest =
            serde_json::from_value(expand(&case["request"])).expect("valid request parses");
        request.validate().expect("valid request validates");
    }
    let default: FactoryRuntimeRequest =
        serde_json::from_value(expand(&doc["valid"][0]["request"])).unwrap();
    assert_eq!(
        default.effective_limit().unwrap(),
        FACTORY_SNAPSHOT_DEFAULT_LIMIT
    );
    assert_eq!(FACTORY_SNAPSHOT_MIN_LIMIT, 1);
    assert_eq!(FACTORY_SNAPSHOT_MAX_LIMIT, 100);

    for case in doc["invalid"].as_array().unwrap() {
        let expected = case["expected_code"].as_str().unwrap();
        let value = expand(&case["request"]);
        match serde_json::from_value::<FactoryRuntimeRequest>(value) {
            Ok(request) => assert_code(request.validate().unwrap_err(), expected),
            Err(_) => assert_eq!(
                expected, "InvalidRecord",
                "serde malformed -> InvalidRecord"
            ),
        }
    }
}

// --- 8. runtime record invalid/overlimit cases -----------------------------

#[test]
fn runtime_records_reject_malformed_and_overlimits() {
    tick();
    let doc = parse_value(RUNTIME_INVALID_JSON);
    let base = &doc["base"];
    let cases = doc["cases"].as_array().unwrap();
    assert!(!cases.is_empty());

    let mut codes: Vec<String> = Vec::new();
    for case in cases {
        let expected = case["expected_code"].as_str().unwrap();
        let value = patched(base, case);
        let code = match serde_json::from_value::<FactoryRuntime>(value) {
            Ok(record) => failure_code(&record.validate().unwrap_err())
                .map(|code| code.as_str().to_string())
                .unwrap_or_else(|| panic!("case {} unexpectedly valid", case["name"])),
            Err(_) => "InvalidRecord".to_string(),
        };
        assert_eq!(code, expected, "case {} expected {expected}", case["name"]);
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    for expected in ["InvalidRecord", "UnsupportedVersion", "UnsupportedSchema"] {
        assert!(codes.contains(&expected.to_string()), "covered {expected}");
    }
}

// --- 9. control request parse and identity round-trip ----------------------

#[test]
fn control_requests_parse_and_pin_digests() {
    tick();
    let doc = parse_value(CONTROL_REQUESTS_JSON);

    let lookup: FactoryLookupRequest =
        serde_json::from_value(doc["lookup"].clone()).expect("lookup parses");
    lookup.validate().expect("lookup validates");
    let request = FactoryControlRequest::parse(&serde_json::to_string(&lookup).unwrap())
        .expect("lookup parses");
    assert!(matches!(request, FactoryControlRequest::Lookup(_)));

    for (name, operation) in [("pause", "pause"), ("resume", "resume")] {
        let action: FactoryActionRequest =
            serde_json::from_value(doc[name].clone()).expect("action parses");
        action.validate().expect("action validates");
        assert_eq!(action.operation, operation);
        let value = action.request_value().unwrap();
        let digest = action.request_digest().unwrap();
        assert_eq!(digest, canonical_digest(&value).unwrap());
        assert!(is_sha256(&digest));
        assert_eq!(
            digest,
            doc["digests"][name].as_str().unwrap(),
            "the fixture digest is the exact validated action object digest"
        );
        let parsed =
            FactoryControlRequest::parse(&serde_json::to_string(&action).unwrap()).unwrap();
        match parsed {
            FactoryControlRequest::Action(parsed) => assert_eq!(parsed, action),
            _ => panic!("pause/resume must parse as an action request"),
        }
    }

    // Confirmation is preserved exactly (including a null) for the digest.
    let mut with_spaces = doc["pause"].clone();
    with_spaces["confirmation"] = json!("  yes  ");
    let request: FactoryActionRequest = serde_json::from_value(with_spaces).unwrap();
    request.validate().unwrap();
    assert_eq!(request.confirmation.as_deref(), Some("  yes  "));
}

// --- 10. control response result variants are distinct ---------------------

#[test]
fn control_responses_keep_three_distinct_results() {
    tick();
    let doc = parse_value(CONTROL_RESPONSES_JSON);

    let applied = FactoryControlResponse::from_value(doc["applied"].clone()).expect("applied");
    match &applied {
        FactoryControlResponse::Receipt(response) => {
            assert_eq!(response.result, FactoryControlResult::Receipt);
            assert_eq!(response.receipt.outcome, FactoryControlOutcome::Applied);
            assert_eq!(response.receipt.actor, FactoryControlActor::Human);
            assert!(response.receipt.reason.is_none());
        }
        _ => panic!("applied must be a receipt"),
    }

    let denied = FactoryControlResponse::from_value(doc["denied"].clone()).expect("denied");
    match &denied {
        FactoryControlResponse::Receipt(response) => {
            assert_eq!(response.result, FactoryControlResult::Receipt);
            assert_eq!(response.receipt.outcome, FactoryControlOutcome::Denied);
            assert!(response.receipt.reason.is_some());
        }
        _ => panic!("denied must be a receipt"),
    }

    let unconfirmed =
        FactoryControlResponse::from_value(doc["unconfirmed"].clone()).expect("unconfirmed");
    match &unconfirmed {
        FactoryControlResponse::Unconfirmed(response) => {
            assert_eq!(response.result, FactoryControlResult::Unconfirmed);
            assert_eq!(
                response.code,
                agalma_contracts::FactoryUnconfirmedCode::EffectUnconfirmed
            );
            assert!(!response.reason.is_empty());
        }
        _ => panic!("unconfirmed must be the unconfirmed variant"),
    }

    let not_found =
        FactoryControlResponse::from_value(doc["not_found"].clone()).expect("not_found");
    match &not_found {
        FactoryControlResponse::NotFound(response) => {
            assert_eq!(response.result, FactoryControlResult::NotFound);
        }
        _ => panic!("not_found must be the not_found variant"),
    }

    // The three results stay distinct; unconfirmed is never a receipt.
    assert_ne!(applied, unconfirmed);
    assert_ne!(unconfirmed, not_found);
    // A round trip through the wire text is stable.
    let reparsed =
        FactoryControlResponse::from_wire_str(&serde_json::to_string(&applied).unwrap()).unwrap();
    assert_eq!(reparsed, applied);
}

// --- 11. control invalid cases ---------------------------------------------

#[test]
fn control_requests_and_responses_reject_malformed() {
    tick();
    let doc = parse_value(CONTROL_INVALID_JSON);

    for (base_key, cases_key) in [
        ("request_base", "request_cases"),
        ("lookup_base", "lookup_cases"),
    ] {
        let base = &doc[base_key];
        for case in doc[cases_key].as_array().unwrap() {
            let expected = case["expected_code"].as_str().unwrap();
            let value = patched(base, case);
            let code = match FactoryControlRequest::from_value(value) {
                Ok(parsed) => panic!("case {} unexpectedly valid: {parsed:?}", case["name"]),
                Err(error) => failure_code(&error)
                    .map(|code| code.as_str().to_string())
                    .unwrap_or_default(),
            };
            assert_eq!(code, expected, "case {}", case["name"]);
        }
    }

    for case in doc["response_cases"].as_array().unwrap() {
        let expected = case["expected_code"].as_str().unwrap();
        let base = &doc["response_bases"][case["variant"].as_str().unwrap()];
        let value = patched(base, case);
        let code = match FactoryControlResponse::from_value(value) {
            Ok(parsed) => panic!("case {} unexpectedly valid: {parsed:?}", case["name"]),
            Err(error) => failure_code(&error)
                .map(|code| code.as_str().to_string())
                .unwrap_or_default(),
        };
        assert_eq!(code, expected, "case {}", case["name"]);
    }
}

// --- 12. nullable response keys must exist ---------------------------------

#[test]
fn nullable_response_keys_must_be_present() {
    tick();
    let value = parse_value(RUNTIME_UNAVAILABLE_JSON);
    // Explicit nulls decode fine (the keys are present).
    let _: FactoryRuntime = serde_json::from_value(value.clone()).expect("nulls decode");
    // Removing any nullable response key is malformed.
    for path in [
        "runtime_instance_id",
        "reason",
        "stop",
        "current_execution",
        "eligible.revision",
        "eligible.total",
        "eligible.next_cursor",
        "sources.0.detail",
        "sources.0.observed_at_unix_ms",
    ] {
        let mut candidate = value.clone();
        remove_path(&mut candidate, path);
        assert!(
            serde_json::from_value::<FactoryRuntime>(candidate).is_err(),
            "{path} must be required-present"
        );
    }

    let responses = parse_value(CONTROL_RESPONSES_JSON);
    for (variant, path) in [
        ("applied", "observed_stop"),
        ("applied", "receipt.reason"),
        ("unconfirmed", "observed_stop"),
        ("not_found", "observed_stop"),
    ] {
        let mut candidate = responses[variant].clone();
        remove_path(&mut candidate, path);
        assert!(
            FactoryControlResponse::from_value(candidate).is_err(),
            "{variant}.{path} must be required-present"
        );
    }
}

// --- 13. trait shape and object safety -------------------------------------

struct MockSource;

impl RuntimeObservationSource for MockSource {
    fn snapshot(
        &self,
        request: &FactoryRuntimeRequest,
    ) -> Result<FactoryRuntime, agalma_contracts::ContractError> {
        request.validate()?;
        let mut record = runtime_stub();
        record.state_id = "state-mock".to_string();
        Ok(record)
    }
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn runtime_source_trait_is_send_sync_object_safe() {
    tick();
    assert_send_sync::<MockSource>();
    let source: &dyn RuntimeObservationSource = &MockSource;
    let request = FactoryRuntimeRequest {
        format: FACTORY_RUNTIME_FORMAT.to_string(),
        operation: "snapshot".to_string(),
        limit: Some(10),
        cursor: None,
    };
    let record = source.snapshot(&request).expect("mock snapshot");
    record.validate().expect("mock record validates");

    let bad = FactoryRuntimeRequest {
        format: FACTORY_RUNTIME_FORMAT.to_string(),
        operation: "snapshot".to_string(),
        limit: Some(0),
        cursor: None,
    };
    assert_code(source.snapshot(&bad).unwrap_err(), "InvalidRecord");
}

// --- 14. contracts purity and capability separation ------------------------

#[test]
fn factory_contracts_are_free_of_io_and_vendor_handles() {
    tick();
    let source = include_str!("../src/factory.rs");
    for needle in [
        "std::fs",
        "std::net",
        "std::process",
        "std::io",
        "rusqlite",
        "tokio::",
        "sqlx",
        "reqwest",
        "Command::new",
        "LedgerApi",
    ] {
        assert!(
            !source.contains(needle),
            "factory.rs must not contain {needle:?}"
        );
    }
    // The contracts expose no effectful operation beyond pause/resume/lookup.
    for needle in ["start_worker", "release_task", "cancel_action", "shell"] {
        assert!(
            !source.contains(needle),
            "the factory contract must not carry {needle:?}"
        );
    }
    assert_eq!(FACTORY_READ_CAPABILITY, "factory.read/v1");
    assert_eq!(FACTORY_CONTROL_CAPABILITY, "factory.control/v1");
    assert_eq!(FACTORY_CONTROL_FORMAT, "FactoryControl/v1");
    assert_eq!(FACTORY_QUEUE_CURSOR_FORMAT, "FactoryQueueCursor/v1");
}

// --- 15. property: 256 scalar / format / boundary cases --------------------

#[test]
fn property_scalar_format_and_boundary_cases() {
    tick();
    const SEED: u64 = 0x5745_4209;
    const BUDGET: usize = 256;
    let mut state = SEED;
    let mut next = move || -> u64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut executed = 0usize;
    for case in 0..BUDGET {
        let value = next();

        // ExactU64 round trip is exact text.
        let exact = ExactU64::from(value);
        assert_eq!(exact.as_str(), value.to_string());
        assert_eq!(exact.as_u64().unwrap(), value);
        assert_eq!(ExactU64::parse(exact.as_str()).unwrap(), exact);
        assert_eq!(
            serde_json::to_string(&exact).unwrap(),
            format!("\"{value}\"")
        );

        if case % 8 == 0 {
            for bad in ["", "-1", "1.5", "1e3", " 1", "1 ", "0x10", "1_000"] {
                assert_code(ExactU64::parse(bad).unwrap_err(), "InvalidRecord");
            }
        }

        if case % 4 == 0 {
            let mut version = runtime_stub();
            version.format = "FactoryRuntime/v2".to_string();
            assert_code(version.validate().unwrap_err(), "UnsupportedVersion");
            let mut family = runtime_stub();
            family.format = "SomeOther/v1".to_string();
            assert_code(family.validate().unwrap_err(), "UnsupportedSchema");
            let mut bare = runtime_stub();
            bare.format = "FactoryRuntime".to_string();
            assert_code(bare.validate().unwrap_err(), "UnsupportedSchema");
        }

        if case % 2 == 0 {
            let limit = (value % 200) as u32;
            let request = FactoryRuntimeRequest {
                format: FACTORY_RUNTIME_FORMAT.to_string(),
                operation: "snapshot".to_string(),
                limit: Some(limit),
                cursor: None,
            };
            if (FACTORY_SNAPSHOT_MIN_LIMIT..=FACTORY_SNAPSHOT_MAX_LIMIT).contains(&limit) {
                assert_eq!(request.effective_limit().unwrap(), limit);
            } else {
                assert_code(request.effective_limit().unwrap_err(), "InvalidRecord");
            }
        }
        executed += 1;
    }
    assert_eq!(executed, BUDGET, "the full generated budget executed");
}

// --- 16. property: 128 request identity mutations --------------------------

#[test]
fn property_request_identity_mutations() {
    tick();
    const SEED: u64 = 0x5745_4209 ^ 0x128;
    const BUDGET: usize = 128;
    let mut state = SEED;
    let mut next = move || -> u64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut executed = 0usize;
    for case in 0..BUDGET {
        let action_id = format!("action-{:x}", next());
        let operation = if case % 2 == 0 { "pause" } else { "resume" };
        let confirmation = match case % 4 {
            0 => None,
            1 => Some("yes".to_string()),
            2 => Some("no".to_string()),
            _ => Some("confirm".to_string()),
        };
        let request = FactoryActionRequest {
            format: FACTORY_CONTROL_FORMAT.to_string(),
            operation: operation.to_string(),
            action_id: action_id.clone(),
            confirmation,
        };
        request.validate().expect("generated action validates");
        let value = request.request_value().unwrap();
        let digest = request.request_digest().unwrap();
        assert_eq!(digest, canonical_digest(&value).unwrap());
        assert!(is_sha256(&digest));

        let body = FactoryControlReceiptBody {
            action_id: action_id.clone(),
            request: request.clone(),
            request_digest: digest.clone(),
            outcome: if case % 3 == 0 {
                FactoryControlOutcome::Denied
            } else {
                FactoryControlOutcome::Applied
            },
            reason: None,
            ledger_action_id: format!("ledger:{}", action_id),
            actor: FactoryControlActor::Human,
            recorded_at_unix_ms: ExactU64::from(1_791_820_805_000u64),
        };
        body.validate()
            .expect("receipt with exact digest validates");

        // A mutated request under the unchanged digest is an identity conflict
        // (the action id still matches the outer action id).
        let mut mutated = request.clone();
        mutated.confirmation = Some("mutated-confirmation".to_string());
        let mutated_body = FactoryControlReceiptBody {
            request: mutated,
            ..body.clone()
        };
        assert_code(mutated_body.validate().unwrap_err(), "IdentityConflict");

        // A recorded request whose inner action id differs from the outer
        // action id is malformed, even when its own digest is self-consistent.
        let mut mismatched = request.clone();
        mismatched.action_id = format!("{action_id}-other");
        let mismatched_body = FactoryControlReceiptBody {
            request: mismatched.clone(),
            request_digest: mismatched.request_digest().unwrap(),
            ..body.clone()
        };
        assert_code(mismatched_body.validate().unwrap_err(), "InvalidRecord");

        // An omitted required nullable field is malformed.
        let mut wire = serde_json::to_value(&request).unwrap();
        wire.as_object_mut().unwrap().remove("confirmation");
        assert!(
            serde_json::from_value::<FactoryActionRequest>(wire).is_err(),
            "an omitted confirmation is malformed"
        );
        executed += 1;
    }
    assert_eq!(executed, BUDGET, "the full generated budget executed");
}

// --- 17. property: 64 cursor / page cases ----------------------------------

#[test]
fn property_cursor_and_page_cases() {
    tick();
    const SEED: u64 = 0x5745_4209 ^ 0x64;
    const BUDGET: usize = 64;
    let mut state = SEED;
    let mut next = move || -> u64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut executed = 0usize;
    for _ in 0..BUDGET {
        let value = next();
        let cursor = FactoryQueueCursor {
            format: FACTORY_QUEUE_CURSOR_FORMAT.to_string(),
            repository_id: "agalma".to_string(),
            state_id: format!("state-{}", value % 7),
            runtime_instance_id: format!("rt-{}", value % 5 + 1),
            revision: format!("rev-{:x}", next()),
            after: format!("task-{:x}", next()),
        };
        let raw = cursor.encode().expect("cursor encodes");
        assert!(raw.len() <= FACTORY_CURSOR_MAX_BYTES);
        assert_eq!(FactoryQueueCursor::decode(&raw).unwrap(), cursor);
        cursor
            .check_binding(&cursor.repository_id, &cursor.state_id)
            .expect("binding matches");
        cursor
            .check_pinned(&cursor.revision, &cursor.runtime_instance_id)
            .expect("pinned matches");
        assert_code(
            cursor.check_binding("other", &cursor.state_id).unwrap_err(),
            "InvalidRecord",
        );
        assert_code(
            cursor
                .check_pinned(
                    &format!("{}-stale", cursor.revision),
                    &cursor.runtime_instance_id,
                )
                .unwrap_err(),
            "RevisionConflict",
        );

        let count = (value % (FACTORY_PAGE_MAX_ENTRIES as u64 + 1)) as usize;
        let entries: Vec<FactoryEligibleEntry> = (0..count)
            .map(|index| FactoryEligibleEntry {
                task_id: format!("task-{index}"),
                title: "T".repeat(index % FACTORY_TITLE_MAX_BYTES),
                priority: match index % 4 {
                    0 => TaskPriority::Low,
                    1 => TaskPriority::Normal,
                    2 => TaskPriority::High,
                    _ => TaskPriority::Critical,
                },
                family: match index % 4 {
                    0 => TaskFamily::Bench,
                    1 => TaskFamily::Feature,
                    2 => TaskFamily::Fix,
                    _ => TaskFamily::Mutation,
                },
            })
            .collect();
        let page = FactoryEligiblePage {
            entries,
            revision: Some(format!("rev-{:x}", value)),
            total: Some(ExactU64::from(value)),
            next_cursor: None,
        };
        page.validate().expect("generated page validates");
        executed += 1;
    }
    assert_eq!(executed, BUDGET, "the full generated budget executed");
}

// --- 18. page overlimit is refused -----------------------------------------

#[test]
fn page_overlimit_is_refused() {
    tick();
    let entry = FactoryEligibleEntry {
        task_id: "fix-answer".to_string(),
        title: "T".to_string(),
        priority: TaskPriority::Low,
        family: TaskFamily::Fix,
    };
    let at_cap = FactoryEligiblePage {
        entries: vec![entry.clone(); FACTORY_PAGE_MAX_ENTRIES],
        revision: None,
        total: None,
        next_cursor: None,
    };
    at_cap.validate().expect("exactly the page cap is allowed");
    let over = FactoryEligiblePage {
        entries: vec![entry; FACTORY_PAGE_MAX_ENTRIES + 1],
        revision: None,
        total: None,
        next_cursor: None,
    };
    assert_code(over.validate().unwrap_err(), "InvalidRecord");
}

// --- 20. receipt/unconfirmed requests are strictly typed --------------------

#[test]
fn receipt_and_unconfirmed_requests_are_strictly_typed() {
    tick();
    let responses = parse_value(CONTROL_RESPONSES_JSON);

    // (variant, request path, digest path)
    for (variant, request_path) in [("applied", "receipt.request"), ("unconfirmed", "request")] {
        let reject = |path: &str, new_value: Value, expected: &str| {
            let mut value = responses[variant].clone();
            set_path(&mut value, path, new_value);
            let code = match FactoryControlResponse::from_value(value) {
                Ok(parsed) => panic!("{variant} {path} unexpectedly valid: {parsed:?}"),
                Err(error) => failure_code(&error)
                    .map(|code| code.as_str().to_string())
                    .unwrap_or_default(),
            };
            assert_eq!(code, expected, "{variant} {path}");
        };

        // lookup/cancel are not valid action operations.
        reject(
            &format!("{request_path}.operation"),
            json!("lookup"),
            "InvalidRecord",
        );
        reject(
            &format!("{request_path}.operation"),
            json!("cancel"),
            "InvalidRecord",
        );
        // An extra request field is malformed.
        reject(&format!("{request_path}.extra"), json!(1), "InvalidRecord");
        // The inner action id must equal the outer action id.
        reject(
            &format!("{request_path}.action_id"),
            json!("action-other"),
            "InvalidRecord",
        );
        // A self-consistent request under a stale digest conflicts.
        reject(
            &format!("{request_path}.confirmation"),
            json!("different"),
            "IdentityConflict",
        );
        // A missing required-nullable confirmation is malformed.
        let mut value = responses[variant].clone();
        remove_path(&mut value, &format!("{request_path}.confirmation"));
        assert!(
            FactoryControlResponse::from_value(value).is_err(),
            "{variant} request confirmation is required-present"
        );
    }

    // The serialization/digest helpers themselves refuse a malformed action
    // request rather than minting an identity for it.
    let invalid = [
        (
            "format-version",
            FactoryActionRequest {
                format: "FactoryControl/v2".to_string(),
                operation: "pause".to_string(),
                action_id: "op-1".to_string(),
                confirmation: None,
            },
            "UnsupportedVersion",
        ),
        (
            "operation-lookup",
            FactoryActionRequest {
                format: FACTORY_CONTROL_FORMAT.to_string(),
                operation: "lookup".to_string(),
                action_id: "op-1".to_string(),
                confirmation: None,
            },
            "InvalidRecord",
        ),
        (
            "operation-cancel",
            FactoryActionRequest {
                format: FACTORY_CONTROL_FORMAT.to_string(),
                operation: "cancel".to_string(),
                action_id: "op-1".to_string(),
                confirmation: None,
            },
            "InvalidRecord",
        ),
        (
            "action-id-charset",
            FactoryActionRequest {
                format: FACTORY_CONTROL_FORMAT.to_string(),
                operation: "pause".to_string(),
                action_id: "bad id".to_string(),
                confirmation: None,
            },
            "InvalidRecord",
        ),
        (
            "confirmation-overlimit",
            FactoryActionRequest {
                format: FACTORY_CONTROL_FORMAT.to_string(),
                operation: "pause".to_string(),
                action_id: "op-1".to_string(),
                confirmation: Some("c".repeat(65)),
            },
            "InvalidRecord",
        ),
    ];
    for (_name, bad, expected) in invalid {
        assert_code(bad.request_value().unwrap_err(), expected);
        assert_code(bad.request_digest().unwrap_err(), expected);
    }

    // Property: for any overlimit action id, both helpers fail with
    // InvalidRecord. Generation scheme: xorshift64, seed 0x57454209 ^ 0x9; 64
    // cases drawing a length in 129..=192.
    let mut state = 0x5745_4209u64 ^ 0x9;
    for _ in 0..64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = 129 + (state % 64) as usize;
        let bad = FactoryActionRequest {
            format: FACTORY_CONTROL_FORMAT.to_string(),
            operation: "pause".to_string(),
            action_id: "a".repeat(len),
            confirmation: None,
        };
        assert_code(bad.request_value().unwrap_err(), "InvalidRecord");
        assert_code(bad.request_digest().unwrap_err(), "InvalidRecord");
    }
}

// --- 21. cursor fails closed and enforces its byte bound --------------------

#[test]
fn cursor_fails_closed_and_enforces_byte_bound() {
    tick();
    let doc = parse_value(CURSOR_JSON);
    let cursor: FactoryQueueCursor =
        serde_json::from_value(doc["cursor"].clone()).expect("cursor parses");
    let encoded = cursor.encode().expect("cursor encodes");
    let revision = doc["bound"]["revision"].as_str().unwrap();
    let runtime = doc["bound"]["runtime_instance_id"].as_str().unwrap();

    // Verifiable cursor is accepted.
    check_factory_cursor(
        Some(&encoded),
        "agalma",
        "state-fixture-running",
        Some(revision),
        Some(runtime),
    )
    .unwrap()
    .expect("valid cursor returned");

    // An unverifiable cursor fails closed (no current revision / runtime).
    assert_code(
        check_factory_cursor(
            Some(&encoded),
            "agalma",
            "state-fixture-running",
            None,
            Some(runtime),
        )
        .unwrap_err(),
        "RevisionConflict",
    );
    assert_code(
        check_factory_cursor(
            Some(&encoded),
            "agalma",
            "state-fixture-running",
            Some(revision),
            None,
        )
        .unwrap_err(),
        "RevisionConflict",
    );
    // A wrong scope is still InvalidRecord (distinct from RevisionConflict).
    assert_code(
        check_factory_cursor(
            Some(&encoded),
            "other-repo",
            "state-fixture-running",
            Some(revision),
            Some(runtime),
        )
        .unwrap_err(),
        "InvalidRecord",
    );

    // A token beyond the byte bound is refused; at the bound it decodes.
    let pad = FACTORY_CURSOR_MAX_BYTES + 1 - encoded.len();
    let over = format!("{encoded}{}", " ".repeat(pad));
    assert_eq!(over.len(), FACTORY_CURSOR_MAX_BYTES + 1);
    assert_code(
        FactoryQueueCursor::decode(&over).unwrap_err(),
        "InvalidRecord",
    );
    let at = format!("{encoded}{}", " ".repeat(pad - 1));
    assert_eq!(at.len(), FACTORY_CURSOR_MAX_BYTES);
    assert_eq!(FactoryQueueCursor::decode(&at).unwrap(), cursor);
}

// --- 22. cursor encode rejects JSON-escaping expansion ----------------------

#[test]
fn cursor_encode_rejects_escaping_expansion() {
    tick();
    // U+0001 is one raw UTF-8 byte but canonical JSON escapes it as the six
    // bytes `\u0001`, so valid bounded strings can expand the encoded token
    // past the 4096-byte bound even though every field is within its own bound.
    let control = "\u{1}";
    let expanded = FactoryQueueCursor {
        format: FACTORY_QUEUE_CURSOR_FORMAT.to_string(),
        repository_id: control.repeat(256),
        state_id: control.repeat(256),
        runtime_instance_id: control.repeat(128),
        revision: control.repeat(128),
        after: control.repeat(256),
    };
    expanded
        .validate()
        .expect("every raw field is within its own byte bound");
    assert_code(expanded.encode().unwrap_err(), "InvalidRecord");

    // The same shape with smaller escaped expansion encodes within the bound.
    let small = FactoryQueueCursor {
        format: FACTORY_QUEUE_CURSOR_FORMAT.to_string(),
        repository_id: control.repeat(100),
        state_id: control.repeat(100),
        runtime_instance_id: control.repeat(100),
        revision: control.repeat(100),
        after: control.repeat(100),
    };
    let raw = small.encode().expect("small escaped cursor encodes");
    assert!(raw.len() <= FACTORY_CURSOR_MAX_BYTES);
    assert_eq!(FactoryQueueCursor::decode(&raw).unwrap(), small);

    // Property: for any escaping cursor, encode either yields a token within
    // the bound or is refused with InvalidRecord; it never exceeds the bound.
    // Scheme: xorshift64 seed 0x57454209 ^ 0xC0, 64 cases drawing a repeat
    // length for each field (clamped to that field's bound).
    let mut state = 0x5745_4209u64 ^ 0xC0;
    for _ in 0..64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let n = (state % 260) as usize;
        let cursor = FactoryQueueCursor {
            format: FACTORY_QUEUE_CURSOR_FORMAT.to_string(),
            repository_id: control.repeat(n.min(256)),
            state_id: control.repeat(n.min(256)),
            runtime_instance_id: control.repeat(n.min(128)),
            revision: control.repeat(n.min(128)),
            after: control.repeat(n.min(256)),
        };
        match cursor.encode() {
            Ok(raw) => assert!(raw.len() <= FACTORY_CURSOR_MAX_BYTES),
            Err(error) => assert_code(error, "InvalidRecord"),
        }
    }
}

// --- 23. permissions require both wire fields -------------------------------
#[test]
fn permissions_require_both_wire_fields() {
    tick();
    assert!(serde_json::from_value::<FactoryRuntimePermissions>(json!({})).is_err());
    assert!(
        serde_json::from_value::<FactoryRuntimePermissions>(json!({ "pause": false })).is_err()
    );
    assert!(
        serde_json::from_value::<FactoryRuntimePermissions>(json!({ "resume": false })).is_err()
    );
    let both = serde_json::from_value::<FactoryRuntimePermissions>(
        json!({ "pause": true, "resume": false }),
    )
    .expect("both fields decode");
    assert!(both.pause && !both.resume);

    // Programmatic default stays all-false.
    let default = FactoryRuntimePermissions::default();
    assert!(!default.pause && !default.resume);

    // A runtime omitting either permission field is malformed.
    let value = parse_value(PERMISSIONS_JSON);
    for path in ["permissions.pause", "permissions.resume"] {
        let mut candidate = value["allowed"].clone();
        remove_path(&mut candidate, path);
        assert!(
            serde_json::from_value::<FactoryRuntime>(candidate).is_err(),
            "{path} must be required-present"
        );
    }
}

// --- 24. exact format spelling rejects aliases ------------------------------

#[test]
fn exact_format_spelling_rejects_aliases() {
    tick();
    // Non-canonical version spellings are schema-invalid, not aliases of v1.
    for alias in [
        "FactoryRuntime/v01",
        "FactoryRuntime/V1",
        "FactoryRuntime/v1 ",
        "FactoryRuntime/v 1",
        "FactoryRuntime/v1x",
        "FactoryRuntime/v001",
    ] {
        let mut record = runtime_stub();
        record.format = alias.to_string();
        assert_code(record.validate().unwrap_err(), "UnsupportedSchema");
    }
    // A recognized-but-unsupported canonical version is UnsupportedVersion.
    let mut record = runtime_stub();
    record.format = "FactoryRuntime/v2".to_string();
    assert_code(record.validate().unwrap_err(), "UnsupportedVersion");

    for alias in [
        "FactoryControl/v01",
        "FactoryControl/v1.0",
        "FactoryControl/V1",
    ] {
        let value = json!({ "format": alias, "operation": "lookup", "action_id": "op-1" });
        assert_code(
            FactoryControlRequest::from_value(value).unwrap_err(),
            "UnsupportedSchema",
        );
    }

    for alias in [
        "FactoryQueueCursor/v01",
        "FactoryQueueCursor/v1.0",
        "FactoryQueueCursor/V1",
    ] {
        let mut value = parse_value(CURSOR_JSON)["cursor"].clone();
        value["format"] = json!(alias);
        let cursor: FactoryQueueCursor = serde_json::from_value(value).unwrap();
        assert_code(cursor.validate().unwrap_err(), "UnsupportedSchema");
    }
}

// --- 25. required-case counter ---------------------------------------------

#[test]
fn required_cases_executed_is_nonzero() {
    tick();
    assert!(CASES.load(Ordering::SeqCst) > 0);
}
