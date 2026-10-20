//! WEB.1 `ControlPlaneRead/v1` contract acceptance tests (`agalma-rwn.1`).
//!
//! Independent wire fixtures live in `fixtures/control-plane-read-v1/`; they are
//! hand-written, not generated from the Rust types. Assertions use documented
//! `FailureDetail/v1` codes rather than implementation internals. No network, no
//! `/tmp`, no temp files.
//!
//! Covered: exact large identities and money, unknown source state, valid pages,
//! cursor/version errors, bounded chunks, malformed envelopes, the documented
//! field selectors and six block value conventions, and a purity check.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use agalma_contracts::digest::{canonical_json, failure_code, FailureCode};
use agalma_contracts::plugin::{Durability, HealthState, ReportEnvelope, View, ViewKind};
use agalma_contracts::{
    check_cursor, resolve_field_selector, BindingPin, BrowserWire, Completeness, ComponentLifecycle,
    ComponentStatus, ComponentStatusOrigin, ComponentSummary, ContractError, ExactAmount, ExactU64,
    FieldResolution, Freshness, LifecycleSource, ListComponents, PackageArtifactRef,
    ReadArtifactChunk, ReadArtifactChunkRequest, ReadComponent, ReadCursor, ReadMetadata,
    ReadOverview, ReadPageRequest, ReportingState, SourceState, StatusDesired, UsageCost,
    ARTIFACT_CHUNK_MAX_BYTES, ARTIFACT_TOTAL_MAX_BYTES, CONTROL_PLANE_READ_CAPABILITY,
    CONTROL_PLANE_READ_FORMAT, READ_CURSOR_PREFIX, READ_PAGE_DEFAULT_LIMIT, READ_PAGE_MAX_BYTES,
    READ_PAGE_MAX_ENTRIES, READ_PAGE_MAX_LIMIT,
};

// --- fixtures ---------------------------------------------------------------

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/control-plane-read-v1/",
            $name
        ))
    };
}

const OVERVIEW_JSON: &str = fixture!("overview.json");
const COMPONENTS_PAGE_JSON: &str = fixture!("components-page.json");
const COMPONENT_PAGE_JSON: &str = fixture!("component-page.json");
const ARTIFACT_CHUNK_JSON: &str = fixture!("artifact-chunk.json");
const ARTIFACT_CHUNK_MULTI_JSON: &str = fixture!("artifact-chunk-multi.json");
const FIELD_SELECTORS_JSON: &str = fixture!("field-selectors.json");
const INVALID_RECORDS_JSON: &str = fixture!("invalid-records.json");
const WIRE_IDENTITIES_JSON: &str = fixture!("wire-identities.json");
const WIRE_IDENTITIES_GOLDEN_JSON: &str = fixture!("wire-identities-golden.json");

/// Count of executed required cases; the final test asserts it is nonzero.
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

fn invalid_case(name: &str) -> InvalidRecord {
    let records: Vec<InvalidRecord> =
        serde_json::from_str(INVALID_RECORDS_JSON).expect("invalid-records.json parses");
    records
        .into_iter()
        .find(|record| record.name == name)
        .unwrap_or_else(|| panic!("no invalid record named {name}"))
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

fn digest_stub() -> String {
    format!("sha256:{}", "a".repeat(64))
}

fn metadata_stub() -> ReadMetadata {
    ReadMetadata {
        format: CONTROL_PLANE_READ_FORMAT.to_string(),
        collected_at_unix_ms: 0,
        registry_revision: None,
        report_watermark: None,
        sources: Vec::new(),
    }
}

/// A canonical binding pin for the browser-wire property tests.
fn binding_stub(generation: u64) -> BindingPin {
    BindingPin {
        binding_id: "fixture.wire".to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "fixture-wire-1".to_string(),
        generation,
        config_digest: digest_stub(),
    }
}

/// A canonical report envelope for the browser-wire property tests.
fn envelope_stub(generation: u64, sequence: u64) -> ReportEnvelope {
    ReportEnvelope {
        format: "ReportEnvelope/v1".to_string(),
        plugin_id: "example.wire-reporter".to_string(),
        binding: binding_stub(generation),
        schema_namespace: "example.wire.status".to_string(),
        schema_version: 1,
        sequence,
        reported_at_unix_ms: 1_791_820_800_000,
        observed_at_unix_ms: 1_791_820_800_001,
        execution_id: Some("exec:fixture:1".to_string()),
        durability: Durability::Durable,
        payload: json!({ "ready": true }),
        payload_digest: digest_stub(),
    }
}

fn assert_code(error: ContractError, expected: &str) {
    assert_eq!(
        failure_code(&error).map(FailureCode::as_str),
        Some(expected),
        "error was: {error}"
    );
}

// --- 1. golden overview -----------------------------------------------------

#[test]
fn golden_overview_round_trips_and_gates() {
    tick();
    let overview: ReadOverview = roundtrip(OVERVIEW_JSON);
    overview
        .metadata
        .version_gate()
        .expect("ControlPlaneRead/v1 gates");
    assert_eq!(overview.metadata.format, CONTROL_PLANE_READ_FORMAT);

    // Exact large identities survive as decimal strings.
    let generation = &overview.executions[0].generation;
    assert_eq!(generation.as_str(), "18446744073709551615");
    assert_eq!(generation.as_u64().unwrap(), u64::MAX);
    assert_eq!(
        overview.executions[0].revision.as_u64().unwrap(),
        9_007_199_254_740_993,
        "revision beyond f64 precision is exact"
    );

    // Known usage is exact text; unknown cost is null, never zero.
    let economics: &UsageCost = &overview.economics;
    assert_eq!(
        economics.usage.as_ref().unwrap().as_str(),
        "123456789012345.678901"
    );
    assert!(economics.allowance.is_some());
    assert_eq!(economics.cost, None);

    // Stopped factory and unknown source state are visible and distinct.
    assert!(overview.factory.stopped);
    assert_eq!(overview.factory.reason.as_deref(), Some("operator pause"));
    assert!(overview
        .metadata
        .sources
        .iter()
        .any(|source| source.state == SourceState::Unknown));
    assert_eq!(
        overview
            .metadata
            .sources
            .iter()
            .filter(|source| source.state == SourceState::Available)
            .count(),
        1
    );
}

// --- 2. golden components page ---------------------------------------------

#[test]
fn golden_components_page_is_bounded_and_exact() {
    tick();
    let page: ListComponents = roundtrip(COMPONENTS_PAGE_JSON);
    page.metadata.version_gate().expect("v1 gates");
    page.check_bounds().expect("page is within bounds");
    assert_eq!(page.entries.len(), 2);
    assert!(page.entries.len() <= READ_PAGE_MAX_ENTRIES);

    let reporter = &page.entries[0];
    assert_eq!(reporter.generation.as_str(), "9007199254740993");
    assert_eq!(reporter.health, HealthState::Ready);
    assert_eq!(reporter.freshness, Freshness::Fresh);
    assert!(reporter.available);
    assert_eq!(reporter.views[0].kind, ViewKind::Status);

    // An installed-but-unavailable component is still listed with its identity.
    let stopped = &page.entries[1];
    assert!(!stopped.available);
    assert_eq!(stopped.health, HealthState::Unavailable);
    assert!(!stopped.plugin_id.is_empty());

    // A successful-but-empty source is distinct from a missing one.
    assert!(page
        .metadata
        .sources
        .iter()
        .any(|source| source.state == SourceState::Empty));

    // The opaque cursor round-trips, re-encodes stably, and binds to its scope.
    let raw = page.next_cursor.clone().expect("continuation cursor");
    assert!(raw.starts_with(READ_CURSOR_PREFIX));
    let cursor = ReadCursor::decode(&raw).expect("cursor decodes");
    assert_eq!(cursor.collection, "components");
    assert_eq!(
        cursor.encode().unwrap(),
        raw,
        "encode(decode(token)) == token"
    );
    cursor
        .check_binding(&cursor.collection, &cursor.filter_digest)
        .expect("cursor binds to its own scope");
    cursor
        .check_revision(&cursor.revision)
        .expect("cursor revision is current");
}

// --- 3. golden component page ----------------------------------------------

#[test]
fn golden_component_page_retains_envelope_identity() {
    tick();
    let page: ReadComponent = roundtrip(COMPONENT_PAGE_JSON);
    page.metadata.version_gate().expect("v1 gates");
    page.check_bounds().expect("page is within bounds");
    assert_eq!(page.descriptors.len(), 2);
    assert!(page.entries.len() >= 1);

    for envelope in &page.entries {
        envelope.version_gate().expect("envelope v1 gates");
        envelope.validate().expect("envelope identity is valid");
        assert_eq!(
            envelope.binding, page.binding,
            "entries retain the component binding identity"
        );
    }
    assert_eq!(page.entries[0].sequence, 1);
    assert_eq!(page.entries[1].sequence, 2);
    assert!(page.next_cursor.is_none());
}

// --- 4. golden artifact chunk ----------------------------------------------

#[test]
fn golden_artifact_chunk_is_bounded_and_integral() {
    tick();
    let chunk: ReadArtifactChunk = roundtrip(ARTIFACT_CHUNK_JSON);
    chunk.metadata.version_gate().expect("v1 gates");
    chunk.check_bounds().expect("chunk is within bounds");
    assert_eq!(chunk.data_len().unwrap(), 4);
    assert!(chunk.data_len().unwrap() <= ARTIFACT_CHUNK_MAX_BYTES);
    assert!(chunk.reference.bytes <= ARTIFACT_TOTAL_MAX_BYTES);
    chunk
        .reference
        .verify(&[0xde, 0xad, 0xbe, 0xef])
        .expect("reference digest matches its bytes");
    assert_eq!(chunk.offset.as_u64().unwrap(), 0);
    assert!(chunk.next_offset.is_none());
}

#[test]
fn golden_multi_chunk_continuation_is_exact() {
    tick();
    let chunk: ReadArtifactChunk = roundtrip(ARTIFACT_CHUNK_MULTI_JSON);
    chunk.metadata.version_gate().expect("v1 gates");
    chunk
        .check_bounds()
        .expect("a continuing chunk is within bounds");
    assert_eq!(chunk.data_len().unwrap(), 2);
    assert_eq!(chunk.reference.bytes, 4);
    assert_eq!(
        chunk.next_offset.as_ref().unwrap().as_u64().unwrap(),
        2,
        "next_offset exactly continues the artifact"
    );
    assert!(chunk.data_len().unwrap() < chunk.reference.bytes);
}

// --- 5. page limit ----------------------------------------------------------

#[test]
fn page_limit_defaults_and_bounds() {
    tick();
    let mut checked = 0usize;
    for limit in 0u32..=200 {
        let request = ReadPageRequest {
            limit: Some(limit),
            cursor: None,
        };
        if (1..=READ_PAGE_MAX_LIMIT).contains(&limit) {
            assert_eq!(request.effective_limit().unwrap(), limit);
        } else {
            assert_code(request.effective_limit().unwrap_err(), "InvalidRecord");
        }
        checked += 1;
    }
    assert!(checked > 0, "the limit sweep executed");
    let default = ReadPageRequest {
        limit: None,
        cursor: None,
    };
    assert_eq!(default.effective_limit().unwrap(), READ_PAGE_DEFAULT_LIMIT);
    assert_eq!(READ_PAGE_MAX_LIMIT, 100);
}

// --- 6. cursor round trip and stale revision --------------------------------

#[test]
fn cursor_round_trips_and_rejects_stale_revision() {
    tick();
    let filter = digest_stub();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut checked = 0usize;
    for index in 0..64u64 {
        // Deterministic xorshift64 so the property is reproducible without RNG.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let revision = ExactU64::from(seed);
        let cursor = ReadCursor {
            collection: "components".to_string(),
            filter_digest: filter.clone(),
            revision: revision.clone(),
            after: format!("entry-{index}"),
        };
        let raw = cursor.encode().unwrap();
        assert!(raw.starts_with(READ_CURSOR_PREFIX));
        assert_eq!(
            ReadCursor::decode(&raw).unwrap(),
            cursor,
            "cursor round trips"
        );

        let continued = check_cursor(Some(&raw), "components", &filter, &revision)
            .unwrap()
            .expect("a valid cursor is returned");
        assert_eq!(continued, cursor);

        let newer = ExactU64::from(seed ^ 1);
        assert_code(
            check_cursor(Some(&raw), "components", &filter, &newer).unwrap_err(),
            "RevisionConflict",
        );
        assert_code(
            check_cursor(Some(&raw), "overview", &filter, &revision).unwrap_err(),
            "InvalidRecord",
        );
        checked += 1;
    }
    assert!(checked >= 32, "the cursor sweep executed");
    assert!(
        check_cursor(None, "components", &filter, &ExactU64::from(0))
            .unwrap()
            .is_none()
    );
    assert_code(ReadCursor::decode("garbage").unwrap_err(), "InvalidRecord");

    // A malformed revision is refused at decode (InvalidRecord), never accepted
    // and later misclassified as a stale-revision RevisionConflict.
    for bad in [
        "not-a-number",
        "",
        "-1",
        "1.5",
        "0x10",
        " 1",
        "1 ",
        "18446744073709551616",
    ] {
        let token = format!(
            "cp1:{{\"after\":\"x\",\"collection\":\"components\",\"filter_digest\":\"{filter}\",\"revision\":\"{bad}\"}}"
        );
        assert_code(ReadCursor::decode(&token).unwrap_err(), "InvalidRecord");
    }
}

// --- 7. exact u64 identity --------------------------------------------------

#[test]
fn exact_u64_round_trips_large_values() {
    tick();
    let samples = [
        0u64,
        1,
        255,
        65_535,
        (1u64 << 32) + 1,
        1u64 << 53,
        (1u64 << 53) + 1,
        1u64 << 63,
        u64::MAX - 1,
        u64::MAX,
    ];
    for value in samples {
        let exact = ExactU64::from(value);
        assert_eq!(exact.as_u64().unwrap(), value);
        assert_eq!(exact.as_str(), value.to_string());
        assert_eq!(ExactU64::parse(exact.as_str()).unwrap(), exact);
        assert_eq!(
            serde_json::to_string(&exact).unwrap(),
            format!("\"{value}\"")
        );
    }
    assert_code(ExactU64::parse("12x").unwrap_err(), "InvalidRecord");
    assert_code(ExactU64::parse("-1").unwrap_err(), "InvalidRecord");
    assert_code(ExactU64::parse("").unwrap_err(), "InvalidRecord");
    assert_code(ExactU64::parse(" 1").unwrap_err(), "InvalidRecord");
    assert_code(
        ExactU64::parse("18446744073709551616").unwrap_err(),
        "InvalidRecord",
    );

    // Deserialization validates too: a malformed decimal string cannot enter
    // through a nested record such as a cursor revision.
    for bad in [
        "\"12x\"",
        "\"\"",
        "\"-1\"",
        "\"1.5\"",
        "\"18446744073709551616\"",
    ] {
        assert!(
            serde_json::from_str::<ExactU64>(bad).is_err(),
            "ExactU64 must refuse {bad}"
        );
    }
    assert_eq!(
        serde_json::from_str::<ExactU64>("\"18446744073709551615\"").unwrap(),
        ExactU64::from(u64::MAX)
    );
}

// --- 8. exact amount grammar ------------------------------------------------

#[test]
fn exact_amount_grammar_is_exact() {
    tick();
    for raw in ["0", "5", "5.00", "123456789012345.678901", "000123", "10.0"] {
        let amount = ExactAmount::parse(raw).unwrap();
        assert_eq!(
            amount.as_str(),
            raw,
            "text is preserved exactly, never rounded"
        );
    }
    for raw in [
        "", ".", ".5", "1.", "1.2.3", "-1", "+1", "1e3", "1 000", "abc", "1,5",
    ] {
        assert_code(ExactAmount::parse(raw).unwrap_err(), "InvalidRecord");
    }
}

// --- 9. artifact chunk boundary --------------------------------------------

#[test]
fn artifact_chunk_bounds_accept_limit_and_reject_over() {
    tick();
    let reference = |bytes: u64| PackageArtifactRef {
        path: "objects/x.bin".to_string(),
        digest: digest_stub(),
        bytes,
        media_type: "application/octet-stream".to_string(),
    };
    let chunk = |data_hex: String, reference: PackageArtifactRef, offset: u64| ReadArtifactChunk {
        metadata: metadata_stub(),
        reference,
        offset: ExactU64::from(offset),
        data_hex,
        next_offset: None,
    };

    let at_cap = "ab".repeat(ARTIFACT_CHUNK_MAX_BYTES as usize);
    let over_cap = "ab".repeat(ARTIFACT_CHUNK_MAX_BYTES as usize + 1);
    // A chunk exactly at the cap that also reaches the artifact end is final.
    let total = ARTIFACT_CHUNK_MAX_BYTES;

    chunk(at_cap, reference(total), 0)
        .check_bounds()
        .expect("exactly the chunk cap is allowed");
    assert_code(
        chunk(over_cap, reference(total + 1), 0)
            .check_bounds()
            .unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        chunk("ab".to_string(), reference(0), 0)
            .check_bounds()
            .unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        chunk(String::new(), reference(ARTIFACT_TOTAL_MAX_BYTES + 1), 0)
            .check_bounds()
            .unwrap_err(),
        "InvalidRecord",
    );

    let oversize_request = ReadArtifactChunkRequest {
        plugin_id: "example.reporter".to_string(),
        digest: digest_stub(),
        offset: None,
        max_bytes: Some(ARTIFACT_CHUNK_MAX_BYTES + 1),
    };
    assert_code(oversize_request.check().unwrap_err(), "InvalidRecord");
    let default_request = ReadArtifactChunkRequest {
        plugin_id: "example.reporter".to_string(),
        digest: digest_stub(),
        offset: None,
        max_bytes: None,
    };
    assert_eq!(
        default_request.effective_max_bytes().unwrap(),
        ARTIFACT_CHUNK_MAX_BYTES
    );
}

#[test]
fn artifact_chunk_next_offset_is_exact() {
    tick();
    let reference = |bytes: u64| PackageArtifactRef {
        path: "objects/x.bin".to_string(),
        digest: digest_stub(),
        bytes,
        media_type: "application/octet-stream".to_string(),
    };
    let chunk = |data_hex: &str, bytes: u64, offset: u64, next: Option<u64>| ReadArtifactChunk {
        metadata: metadata_stub(),
        reference: reference(bytes),
        offset: ExactU64::from(offset),
        data_hex: data_hex.to_string(),
        next_offset: next.map(ExactU64::from),
    };

    // Bytes remain (end=1 < total=2): next_offset must be exactly the end.
    chunk("ab", 2, 0, Some(1))
        .check_bounds()
        .expect("a continuing chunk with next_offset == end is allowed");
    assert_code(
        chunk("ab", 2, 0, None).check_bounds().unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        chunk("ab", 2, 0, Some(0)).check_bounds().unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        chunk("ab", 2, 0, Some(2)).check_bounds().unwrap_err(),
        "InvalidRecord",
    );

    // Artifact end (end=total): next_offset must be null.
    chunk("ab", 1, 0, None)
        .check_bounds()
        .expect("a final chunk with next_offset null is allowed");
    assert_code(
        chunk("ab", 1, 0, Some(1)).check_bounds().unwrap_err(),
        "InvalidRecord",
    );

    // Empty artifact is a legal single null-terminated chunk.
    chunk("", 0, 0, None)
        .check_bounds()
        .expect("an empty artifact is a single null-terminated chunk");
    assert_code(
        chunk("", 0, 0, Some(0)).check_bounds().unwrap_err(),
        "InvalidRecord",
    );

    // A zero-length chunk before the artifact end makes no progress and is
    // refused whatever continuation it claims; it cannot be repeated forever.
    assert_code(
        chunk("", 2, 0, Some(0)).check_bounds().unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        chunk("", 2, 0, Some(1)).check_bounds().unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        chunk("", 2, 0, None).check_bounds().unwrap_err(),
        "InvalidRecord",
    );
    // A zero-length chunk exactly at the end is a legal empty final chunk.
    chunk("", 2, 2, None)
        .check_bounds()
        .expect("an empty final chunk at the artifact end is allowed");
    assert_code(
        chunk("", 2, 2, Some(2)).check_bounds().unwrap_err(),
        "InvalidRecord",
    );

    // Generated sweep for the continuation-offset invariant.
    //
    // Scheme: xorshift64 seeded with CONTINUATION_SEED. For each of
    // CONTINUATION_BUDGET cases draw three values independently — total in
    // 0..=64, offset in 0..=total, len in 0..=(total-offset) — and assert the
    // contract for end = offset + len:
    //   * len == 0 and end < total: refused (no progress);
    //   * end == total: next_offset must be null (a non-null value is refused);
    //   * end  < total (and len > 0): next_offset must equal `end`; null
    //     (premature), a backward `end - 1`, a one-byte gap `end + 1`, or a
    //     jump to `total` is refused.
    // Budget: 256 cases. Reproducible; no RNG dependency.
    const CONTINUATION_SEED: u64 = 0xA5A5_5A5A_1234_5678;
    const CONTINUATION_BUDGET: usize = 256;

    let mut state = CONTINUATION_SEED;
    let mut next_rand = move |bound: u64| -> u64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    let mut checked = 0usize;
    let mut zero_progress_cases = 0usize;
    for case in 0..CONTINUATION_BUDGET {
        let total = next_rand(65);
        let offset = next_rand(total + 1);
        let len = next_rand(total - offset + 1);
        let hex = "ab".repeat(len as usize);
        let end = offset + len;
        let before_end = end < total;

        if len == 0 && before_end {
            // A zero-length non-final chunk makes no progress and is refused
            // whatever continuation it claims (including its own offset, which
            // would otherwise repeat forever).
            zero_progress_cases += 1;
            for next in [None, Some(offset), Some(end), Some(end + 1)] {
                assert_code(
                    chunk(&hex, total, offset, next).check_bounds().unwrap_err(),
                    "InvalidRecord",
                );
            }
        } else {
            // (a) The exact continuation is accepted; at the end the required
            //     null is accepted.
            let valid = before_end.then_some(end);
            chunk(&hex, total, offset, valid)
                .check_bounds()
                .unwrap_or_else(|e| {
                    panic!(
                        "case {case}: total={total} offset={offset} len={len} valid rejected: {e}"
                    )
                });

            // (b/c) null is accepted exactly at the artifact end and refused as
            //       a premature truncation before it.
            let null_chunk = chunk(&hex, total, offset, None);
            if before_end {
                assert_code(null_chunk.check_bounds().unwrap_err(), "InvalidRecord");
            } else {
                null_chunk
                    .check_bounds()
                    .unwrap_or_else(|e| panic!("case {case}: null at exact end rejected: {e}"));
            }

            // (d) A non-null value that is not exactly `end` is refused:
            //     one-byte overshoot, one-byte backward step, jump to `total`.
            assert_code(
                chunk(&hex, total, offset, Some(end + 1))
                    .check_bounds()
                    .unwrap_err(),
                "InvalidRecord",
            );
            if end > 0 {
                assert_code(
                    chunk(&hex, total, offset, Some(end - 1))
                        .check_bounds()
                        .unwrap_err(),
                    "InvalidRecord",
                );
            }
            if before_end {
                assert_code(
                    chunk(&hex, total, offset, Some(total))
                        .check_bounds()
                        .unwrap_err(),
                    "InvalidRecord",
                );
            }

            // (e) A non-null next_offset at the exact artifact end is refused.
            if !before_end {
                assert_code(
                    chunk(&hex, total, offset, Some(end))
                        .check_bounds()
                        .unwrap_err(),
                    "InvalidRecord",
                );
            }
        }
        checked += 1;
    }
    assert_eq!(
        checked, CONTINUATION_BUDGET,
        "the full generated budget executed"
    );
    assert!(
        zero_progress_cases > 0,
        "the sweep exercised zero-length non-final chunks"
    );
}

// --- 10. documented field selectors ----------------------------------------

#[test]
fn documented_field_selectors_resolve() {
    tick();
    let doc: Value = serde_json::from_str(FIELD_SELECTORS_JSON).unwrap();
    let selectors = doc["selectors"].as_array().expect("selectors array");
    assert!(!selectors.is_empty());
    let mut cases = 0usize;
    for case in selectors {
        let selector = case["selector"].as_str().unwrap();
        let payload = &case["payload"];
        let expect = case["expect"].as_str().unwrap();
        let resolved = resolve_field_selector(payload, selector);
        match expect {
            "value" => assert!(
                matches!(resolved, FieldResolution::Value(_)),
                "{selector} should resolve a value"
            ),
            "missing" => assert!(
                matches!(resolved, FieldResolution::Missing),
                "{selector} should be missing"
            ),
            other => panic!("unexpected expectation {other}"),
        }
        cases += 1;
    }
    assert!(cases >= 5, "the selector fixture exercises several forms");

    // Escapes resolve to the literal key, not a nested path.
    let payload = json!({ "a/b": "slash", "a~b": "tilde" });
    match resolve_field_selector(&payload, "/a~1b") {
        FieldResolution::Value(value) => assert_eq!(value, &json!("slash")),
        FieldResolution::Missing => panic!("escaped slash should resolve"),
    }
    match resolve_field_selector(&payload, "/a~0b") {
        FieldResolution::Value(value) => assert_eq!(value, &json!("tilde")),
        FieldResolution::Missing => panic!("escaped tilde should resolve"),
    }
}

// --- 11. six block value conventions ---------------------------------------

#[test]
fn six_block_value_conventions_documented() {
    tick();
    let doc: Value = serde_json::from_str(FIELD_SELECTORS_JSON).unwrap();
    let blocks = doc["blocks"].as_array().expect("blocks array");
    assert_eq!(blocks.len(), 6, "all six supported blocks are documented");

    let mut kinds: Vec<ViewKind> = Vec::new();
    for block in blocks {
        let kind_text = block["kind"].as_str().unwrap();
        let view: View = serde_json::from_value(block["view"].clone()).expect("descriptor parses");
        assert_eq!(
            format!("{:?}", view.kind),
            kind_text,
            "descriptor kind matches"
        );
        for field in &view.fields {
            assert!(
                matches!(
                    resolve_field_selector(&block["payload"], field),
                    FieldResolution::Value(_)
                ),
                "block {kind_text} field {field} resolves against its payload"
            );
        }
        if !kinds.contains(&view.kind) {
            kinds.push(view.kind);
        }
    }
    assert_eq!(kinds.len(), 6, "six distinct block kinds are covered");
    for kind in [
        ViewKind::Status,
        ViewKind::Metrics,
        ViewKind::Table,
        ViewKind::Timeline,
        ViewKind::Alert,
        ViewKind::ArtifactLink,
    ] {
        assert!(
            kinds.contains(&kind),
            "the frozen ViewKind {kind:?} is covered"
        );
    }
}

// --- 12. malformed records --------------------------------------------------

#[test]
fn malformed_records_report_documented_codes() {
    tick();

    let case = invalid_case("report-envelope:format");
    let envelope: ReportEnvelope = serde_json::from_value(case.record).unwrap();
    assert_code(envelope.version_gate().unwrap_err(), &case.expected_code);

    let case = invalid_case("report-envelope:payload_digest");
    let envelope: ReportEnvelope = serde_json::from_value(case.record).unwrap();
    assert_code(
        envelope.verify_payload_digest().unwrap_err(),
        &case.expected_code,
    );

    let case = invalid_case("read-metadata:format-family");
    let metadata: ReadMetadata = serde_json::from_value(case.record).unwrap();
    assert_code(metadata.version_gate().unwrap_err(), &case.expected_code);

    let case = invalid_case("read-metadata:format-version");
    let metadata: ReadMetadata = serde_json::from_value(case.record).unwrap();
    assert_code(metadata.version_gate().unwrap_err(), &case.expected_code);

    let case = invalid_case("read-page-request:limit");
    let request: ReadPageRequest = serde_json::from_value(case.record).unwrap();
    assert_code(request.effective_limit().unwrap_err(), &case.expected_code);

    let case = invalid_case("read-page-request:cursor");
    let raw = case.record["cursor"].as_str().unwrap();
    assert_code(ReadCursor::decode(raw).unwrap_err(), &case.expected_code);

    let case = invalid_case("read-cursor:revision");
    let raw = case.record["cursor"].as_str().unwrap().to_string();
    let current = ExactU64::parse(case.record["current_revision"].as_str().unwrap()).unwrap();
    let decoded = ReadCursor::decode(&raw).unwrap();
    assert_code(
        check_cursor(
            Some(&raw),
            &decoded.collection,
            &decoded.filter_digest,
            &current,
        )
        .unwrap_err(),
        &case.expected_code,
    );

    let case = invalid_case("read-artifact-chunk-request:max_bytes");
    let request: ReadArtifactChunkRequest = serde_json::from_value(case.record).unwrap();
    assert_code(request.check().unwrap_err(), &case.expected_code);

    let case = invalid_case("read-artifact-chunk:offset");
    let chunk: ReadArtifactChunk = serde_json::from_value(case.record).unwrap();
    assert_code(chunk.check_bounds().unwrap_err(), &case.expected_code);

    let case = invalid_case("read-artifact-chunk:total");
    let chunk: ReadArtifactChunk = serde_json::from_value(case.record).unwrap();
    assert_code(chunk.check_bounds().unwrap_err(), &case.expected_code);

    let case = invalid_case("exact-u64:identity");
    assert_code(
        ExactU64::parse(case.record["value"].as_str().unwrap()).unwrap_err(),
        &case.expected_code,
    );

    let case = invalid_case("exact-amount:money");
    assert_code(
        ExactAmount::parse(case.record["value"].as_str().unwrap()).unwrap_err(),
        &case.expected_code,
    );

    // Finding 1: a non-decimal revision is refused at decode, not misclassified
    // as a stale revision.
    let case = invalid_case("read-cursor:revision-format");
    let raw = case.record["cursor"].as_str().unwrap();
    assert_code(ReadCursor::decode(raw).unwrap_err(), &case.expected_code);

    // Finding 2: a null, skipped, or end-redundant next_offset is refused.
    for name in [
        "read-artifact-chunk:next-null",
        "read-artifact-chunk:next-skip",
        "read-artifact-chunk:next-at-end",
        "read-artifact-chunk:zero-progress",
    ] {
        let case = invalid_case(name);
        let chunk: ReadArtifactChunk = serde_json::from_value(case.record).unwrap();
        assert_code(chunk.check_bounds().unwrap_err(), &case.expected_code);
    }
}

// --- 13. contracts purity and read-only authority --------------------------

#[test]
fn contracts_are_free_of_io_and_vendor_handles() {
    tick();
    let source = include_str!("../src/control_plane.rs");
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
    ] {
        assert!(
            !source.contains(needle),
            "control_plane.rs must not contain {needle:?}"
        );
    }
    // Viewing grants no action authority: the read projection exposes no
    // mutation or ledger symbols.
    for needle in [
        "LedgerApi",
        "ActionIntent",
        "ComponentCommit",
        "MutationApi",
    ] {
        assert!(
            !source.contains(needle),
            "the read projection must not carry {needle:?}"
        );
    }
    assert_eq!(CONTROL_PLANE_READ_CAPABILITY, "control-plane.read/v1");
}

// --- 14. browser wire golden identity fixture ------------------------------

#[test]
fn browser_wire_identity_fixture_projects_and_round_trips() {
    tick();
    let source: ReadComponent = serde_json::from_str(WIRE_IDENTITIES_JSON).expect("source parses");
    let golden: Value = serde_json::from_str(WIRE_IDENTITIES_GOLDEN_JSON).expect("golden parses");

    // The projection rewrites exactly the nested canonical identities; nothing
    // else changes.
    let wire = source.to_wire_value().expect("projects");
    assert_eq!(
        wire, golden,
        "projection matches the hand-written golden wire"
    );

    // Exact parse back into the canonical record.
    let reparsed = ReadComponent::from_wire_value(&golden).expect("golden parses back");
    assert_eq!(reparsed, source, "wire -> canonical is exact");

    // JSON text round-trips too.
    let text = source.to_wire_json().expect("to_wire_json");
    let from_text = ReadComponent::from_wire_str(&text).expect("from_wire_str");
    assert_eq!(
        from_text, source,
        "to_wire_json -> from_wire_str round-trips"
    );
}

// --- 15. browser wire identity path assertions -----------------------------

#[test]
fn browser_wire_identity_paths_are_exact_strings() {
    tick();
    let source: ReadComponent = serde_json::from_str(WIRE_IDENTITIES_JSON).expect("source parses");
    let wire = source.to_wire_value().expect("projects");

    // Nested canonical identities are decimal strings, equal to the exact text.
    assert_eq!(wire["binding"]["generation"], json!("9007199254740993"));
    assert_eq!(
        wire["entries"][0]["binding"]["generation"],
        json!("18446744073709551615")
    );
    assert_eq!(wire["entries"][0]["sequence"], json!("9007199254740993"));
    assert_eq!(
        wire["entries"][0]["reported_at_unix_ms"],
        json!("9223372036854775807")
    );
    assert_eq!(
        wire["entries"][0]["observed_at_unix_ms"],
        json!("9223372036854775807")
    );

    // The exact string identity is preserved verbatim.
    assert_eq!(
        wire["entries"][0]["execution_id"],
        json!("exec:fixture:9007199254740993")
    );

    // Non-identity u64s stay JSON numbers.
    assert_eq!(wire["binding"]["api_version"], json!(1));
    assert_eq!(wire["entries"][0]["schema_version"], json!(1));

    // Plugin-private payload numbers stay numbers (never stringified).
    assert!(
        wire["entries"][0]["payload"]["count"].is_number(),
        "a payload count above 2^53 stays a JSON number"
    );
    assert_eq!(
        wire["entries"][0]["payload"]["count"],
        json!(9007199254740993u64)
    );
    assert!(
        wire["entries"][0]["payload"]["ratio"].is_number(),
        "a payload float stays a JSON number"
    );
    assert_eq!(wire["entries"][0]["payload"]["ratio"], json!(1.5));
    assert_eq!(wire["entries"][0]["payload"]["state"], json!("ready"));

    // The second envelope's small identities are strings too.
    assert_eq!(wire["entries"][1]["binding"]["generation"], json!("2"));
    assert_eq!(wire["entries"][1]["sequence"], json!("2"));
}

// --- 16. list components wire coverage -------------------------------------

#[test]
fn list_components_wire_rewrites_binding_identity() {
    tick();
    let source: ListComponents =
        serde_json::from_str(COMPONENTS_PAGE_JSON).expect("components page parses");
    let wire = source.to_wire_value().expect("projects");

    // The fixture carries binding.generation = 9007199254740993 as a JSON number;
    // the wire projects it as the exact decimal string.
    assert!(
        wire["entries"][0]["binding"]["generation"].is_string(),
        "binding generation is a string on the wire"
    );
    assert_eq!(
        wire["entries"][0]["binding"]["generation"],
        json!("9007199254740993")
    );
    // Untouched fields remain in place.
    assert_eq!(
        wire["entries"][0]["plugin_id"],
        json!("example.unfamiliar-reporter")
    );
    assert_eq!(wire["entries"][0]["generation"], json!("9007199254740993"));
    assert_eq!(wire["entries"][0]["binding"]["api_version"], json!(1));
    assert_eq!(
        wire["entries"][1]["binding"]["generation"],
        json!("2"),
        "small identities are strings too"
    );

    // Round-trip through the trait equals the source record.
    let from_text =
        ListComponents::from_wire_str(&source.to_wire_json().unwrap()).expect("reparses");
    assert_eq!(from_text, source, "list components wire round-trips");
}

// --- 17. overview wire coverage --------------------------------------------

#[test]
fn overview_wire_matches_generic_and_round_trips() {
    tick();
    let source: ReadOverview = serde_json::from_str(OVERVIEW_JSON).expect("overview parses");
    let wire = source.to_wire_value().expect("projects");

    // The overview surface has no nested canonical identity, so the projection
    // is the faithful generic value.
    assert_eq!(wire, serde_json::to_value(&source).unwrap());

    // Its ExactU64 fields stay exact decimal strings.
    assert_eq!(
        wire["executions"][0]["generation"],
        json!("18446744073709551615")
    );
    assert_eq!(wire["executions"][0]["revision"], json!("9007199254740993"));

    let from_text = ReadOverview::from_wire_str(&source.to_wire_json().unwrap()).expect("reparses");
    assert_eq!(from_text, source, "overview wire round-trips");
}

// --- 18. artifact chunk wire round trip ------------------------------------

#[test]
fn artifact_chunk_wire_round_trips() {
    tick();
    let source: ReadArtifactChunk =
        serde_json::from_str(ARTIFACT_CHUNK_JSON).expect("chunk parses");
    let wire = source.to_wire_value().expect("projects");
    // PackageArtifactRef.bytes is not identity-bearing: it stays a number.
    assert!(wire["reference"]["bytes"].is_number());
    let from_text =
        ReadArtifactChunk::from_wire_str(&source.to_wire_json().unwrap()).expect("reparses");
    assert_eq!(from_text, source, "artifact chunk wire round-trips");
}

// --- 19. browser wire identity property ------------------------------------

#[test]
fn browser_wire_property_round_trips_identity_u64() {
    tick();
    // Property (independent of the implementation): for every u64 `g` and `s`,
    // projecting a ReadComponent whose top-level binding generation is `g` and
    // whose single envelope has sequence `s` and binding generation `g` yields
    // the exact decimal text; parsing it back recovers `g`/`s`; and the whole
    // record round-trips.
    //
    // Generation scheme: xorshift64 seeded with WIRE_SEED; two values drawn per
    // case. Budget: WIRE_BUDGET = 256 generated cases, plus an exhaustive
    // boundary sample list. Reproducible; no RNG dependency.
    const WIRE_SEED: u64 = 0x0123_4567_89AB_CDEF;
    const WIRE_BUDGET: usize = 256;

    let component = |g: u64, s: u64| ReadComponent {
        metadata: metadata_stub(),
        binding: binding_stub(g),
        descriptors: Vec::new(),
        entries: vec![envelope_stub(g, s)],
        next_cursor: None,
        status: None,
    };

    let check = |g: u64, s: u64| {
        let record = component(g, s);
        let wire = record.to_wire_value().expect("projects");
        assert_eq!(
            wire["binding"]["generation"],
            Value::String(g.to_string()),
            "top-level generation is exact decimal text"
        );
        assert_eq!(
            wire["entries"][0]["binding"]["generation"],
            Value::String(g.to_string()),
            "envelope binding generation is exact decimal text"
        );
        assert_eq!(
            wire["entries"][0]["sequence"],
            Value::String(s.to_string()),
            "envelope sequence is exact decimal text"
        );
        assert_eq!(
            wire["binding"]["generation"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            g,
            "parsing the wire string recovers g"
        );
        assert_eq!(
            wire["entries"][0]["sequence"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            s,
            "parsing the wire string recovers s"
        );
        assert_eq!(
            ReadComponent::from_wire_value(&wire).unwrap(),
            record,
            "from_wire_value(to_wire_value(x)) == x"
        );
    };

    // Exhaustive boundary sample list.
    let boundaries = [
        0u64,
        1,
        255,
        65_535,
        (1u64 << 32) + 1,
        1u64 << 53,
        (1u64 << 53) + 1,
        1u64 << 63,
        u64::MAX - 1,
        u64::MAX,
    ];
    for &value in &boundaries {
        check(value, value);
    }
    // The spec-named pair cases, including a g != s cross.
    check(0, 1);
    check(1u64 << 53, (1u64 << 53) + 1);
    check(u64::MAX, 1);

    let mut state = WIRE_SEED;
    let mut next = move || -> u64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut executed = 0usize;
    for _ in 0..WIRE_BUDGET {
        let g = next();
        let s = next();
        check(g, s);
        executed += 1;
    }
    assert_eq!(executed, WIRE_BUDGET, "the full generated budget executed");
}

// --- 20. malformed wire identity refusal -----------------------------------

#[test]
fn browser_wire_refuses_malformed_identities() {
    tick();
    let source: ReadComponent = serde_json::from_str(WIRE_IDENTITIES_JSON).expect("source parses");
    for bad in ["12x", "", "-1", "1.5", "0x10", " 1", "18446744073709551616"] {
        let mut wire = source.to_wire_value().expect("projects");
        wire["binding"]["generation"] = json!(bad);
        assert_code(
            ReadComponent::from_wire_value(&wire).unwrap_err(),
            "InvalidRecord",
        );
    }
}

// --- 21. page bound measures the browser wire, not canonical-json -----------

#[test]
fn page_bound_measures_browser_wire_not_canonical() {
    tick();
    // Each pad character adds exactly one browser-wire byte (the plugin payload
    // is copied verbatim), so a page can be sized precisely to cross the byte
    // cap. Large identities force the decimal-string quotes the canonical form
    // omits, so the canonical length stays under the cap while the wire length
    // is one byte over: the old canonical measure would have wrongly passed.

    let component = |pad: usize| {
        let mut envelope = envelope_stub(u64::MAX, (1u64 << 53) + 1);
        envelope.reported_at_unix_ms = u64::MAX;
        envelope.observed_at_unix_ms = u64::MAX;
        envelope.payload = json!({ "blob": "a".repeat(pad) });
        ReadComponent {
            metadata: metadata_stub(),
            binding: binding_stub(u64::MAX),
            descriptors: Vec::new(),
            entries: vec![envelope],
            next_cursor: None,
            status: None,
        }
    };

    let base_len = component(0).to_wire_json().unwrap().len();
    let pad = READ_PAGE_MAX_BYTES as usize + 1 - base_len;
    let over = component(pad);
    let over_wire = over.to_wire_json().unwrap();
    assert_eq!(
        over_wire.len() as u64,
        READ_PAGE_MAX_BYTES + 1,
        "the over page's wire length crosses the cap by one byte"
    );
    let over_canonical = canonical_json(&serde_json::to_value(&over).unwrap())
        .expect("canonical-json accepts integer-only identities");
    assert!(
        over_canonical.len() as u64 <= READ_PAGE_MAX_BYTES,
        "the canonical measure would have wrongly passed the over page"
    );
    assert!(
        over_canonical.len() < over_wire.len(),
        "identity quoting makes the canonical form strictly shorter"
    );
    assert_code(over.check_bounds().unwrap_err(), "InvalidRecord");

    let under = component(pad - 1);
    assert!(under.to_wire_json().unwrap().len() as u64 <= READ_PAGE_MAX_BYTES);
    under
        .check_bounds()
        .expect("one byte under the cap passes the wire bound");

    // The generic `ReadPage` bound (ListComponents) measures the wire form too.
    let summary = |pad: usize| ComponentSummary {
        plugin_id: "example.unfamiliar-reporter".to_string(),
        name: "n".repeat(pad),
        binding: binding_stub(u64::MAX),
        generation: ExactU64::from(u64::MAX),
        health: HealthState::Ready,
        freshness: Freshness::Fresh,
        completeness: Completeness::Complete,
        available: true,
        views: Vec::new(),
        status: None,
    };
    let page = |pad: usize| ListComponents {
        metadata: metadata_stub(),
        entries: vec![summary(pad)],
        next_cursor: None,
    };

    let page_base_len = page(0).to_wire_json().unwrap().len();
    let page_pad = READ_PAGE_MAX_BYTES as usize + 1 - page_base_len;
    let page_over = page(page_pad);
    let page_over_wire = page_over.to_wire_json().unwrap();
    assert_eq!(page_over_wire.len() as u64, READ_PAGE_MAX_BYTES + 1);
    let page_over_canonical = canonical_json(&serde_json::to_value(&page_over).unwrap()).unwrap();
    assert!(
        page_over_canonical.len() as u64 <= READ_PAGE_MAX_BYTES,
        "the canonical measure would have wrongly passed the over page"
    );
    assert_code(page_over.check_bounds().unwrap_err(), "InvalidRecord");

    let page_under = page(page_pad - 1);
    assert!(page_under.to_wire_json().unwrap().len() as u64 <= READ_PAGE_MAX_BYTES);
    page_under
        .check_bounds()
        .expect("one byte under the cap passes the wire bound");
}

// --- 22. float payload pages are valid and measured on the browser wire ----

#[test]
fn float_payload_page_is_valid_and_measured_on_browser_wire() {
    tick();
    // A plugin payload may carry floats; canonical-json/v1 rejects them, so the
    // page bound must measure the float-compatible browser wire instead.
    let record = ReadComponent {
        metadata: metadata_stub(),
        binding: binding_stub(u64::MAX),
        descriptors: Vec::new(),
        entries: vec![{
            let mut envelope = envelope_stub(u64::MAX, (1u64 << 53) + 1);
            envelope.payload = json!({ "ratio": 1.5, "count": 9007199254740993u64 });
            envelope
        }],
        next_cursor: None,
        status: None,
    };

    record
        .check_bounds()
        .expect("a float-bearing payload page is within bounds");
    let text = record.to_wire_json().expect("to_wire_json permits floats");
    assert!(text.contains("\"ratio\":1.5"));

    let wire = record.to_wire_value().expect("projects");
    assert!(
        wire["entries"][0]["payload"]["ratio"].is_number(),
        "a payload float stays a JSON number on the wire"
    );
    assert_eq!(wire["entries"][0]["payload"]["ratio"], json!(1.5));
    assert!(
        wire["entries"][0]["payload"]["count"].is_number(),
        "a payload integer stays a JSON number"
    );
    assert_eq!(
        wire["entries"][0]["payload"]["count"],
        json!(9007199254740993u64)
    );
    assert!(wire["binding"]["generation"].is_string());
    assert!(wire["entries"][0]["sequence"].is_string());

    // canonical-json/v1 refuses the float, proving the bound cannot use it.
    assert!(
        canonical_json(&serde_json::to_value(&record).unwrap()).is_err(),
        "canonical-json/v1 rejects the payload float"
    );

    // Stronger case: a float-bearing page padded just over the cap is refused.
    let padded = |pad: usize| ReadComponent {
        metadata: metadata_stub(),
        binding: binding_stub(u64::MAX),
        descriptors: Vec::new(),
        entries: vec![{
            let mut envelope = envelope_stub(u64::MAX, (1u64 << 53) + 1);
            envelope.payload = json!({ "ratio": 1.5, "blob": "a".repeat(pad) });
            envelope
        }],
        next_cursor: None,
        status: None,
    };
    let base_len = padded(0).to_wire_json().unwrap().len();
    let pad = READ_PAGE_MAX_BYTES as usize + 1 - base_len;
    let over = padded(pad);
    assert_eq!(
        over.to_wire_json().unwrap().len() as u64,
        READ_PAGE_MAX_BYTES + 1
    );
    assert_code(over.check_bounds().unwrap_err(), "InvalidRecord");
}

// --- 23. status MVP contract (agalma-51dg.12) ------------------------------

fn status_stub() -> ComponentStatus {
    ComponentStatus {
        origin: ComponentStatusOrigin::Discovered,
        generation: Some(ExactU64::from(u64::MAX)),
        desired: Some(StatusDesired::Running),
        lifecycle: Some(ComponentLifecycle::Failed),
        lifecycle_source: LifecycleSource::LiveRegistry,
        lifecycle_observed_at_unix_ms: Some(ExactU64::from((1u64 << 53) + 1)),
        health: Some(HealthState::Ready),
        health_observed_at_unix_ms: Some(ExactU64::from(u64::MAX)),
        health_detail: Some("listener bound".to_string()),
        last_error: Some("process exited".to_string()),
        reporting: ReportingState::Stale,
        incomplete: true,
    }
}

#[test]
fn component_status_mvp_round_trips_optional_status_and_exact_identities() {
    tick();
    let status = status_stub();
    let summary = ComponentSummary {
        plugin_id: "example.status".to_string(),
        name: "Example Status".to_string(),
        binding: binding_stub(u64::MAX),
        generation: ExactU64::from(u64::MAX),
        health: HealthState::Ready,
        freshness: Freshness::Fresh,
        completeness: Completeness::Complete,
        available: true,
        views: Vec::new(),
        status: Some(status.clone()),
    };
    let wire = summary.to_wire_value().expect("summary projects");
    assert_eq!(wire["status"]["origin"], json!("discovered"));
    assert_eq!(wire["status"]["generation"], json!(u64::MAX.to_string()));
    assert_eq!(
        wire["status"]["lifecycle_observed_at_unix_ms"],
        json!(((1u64 << 53) + 1).to_string())
    );
    assert_eq!(
        wire["status"]["health_observed_at_unix_ms"],
        json!(u64::MAX.to_string())
    );
    assert_eq!(wire["status"]["lifecycle"], json!("failed"));
    assert_eq!(wire["status"]["reporting"], json!("stale"));
    let decoded = ComponentSummary::from_wire_value(&wire).expect("summary decodes");
    assert_eq!(decoded, summary);

    // A missing `status` (old response) decodes as unknown, never as healthy.
    let mut old = wire.clone();
    old.as_object_mut().unwrap().remove("status");
    let old_summary = ComponentSummary::from_wire_value(&old).expect("old summary decodes");
    assert_eq!(old_summary.status, None);

    // Explicit null is the same compatibility state.
    let mut null = wire.clone();
    null["status"] = Value::Null;
    assert_eq!(
        ComponentSummary::from_wire_value(&null).unwrap().status,
        None
    );

    let detail = ReadComponent {
        metadata: metadata_stub(),
        binding: binding_stub(u64::MAX),
        descriptors: Vec::new(),
        entries: Vec::new(),
        next_cursor: None,
        status: Some(status.clone()),
    };
    let detail_wire = detail.to_wire_value().expect("detail projects");
    assert_eq!(detail_wire["status"]["health"], json!("ready"));
    assert_eq!(
        ReadComponent::from_wire_value(&detail_wire).unwrap(),
        detail
    );
    let mut old_detail = detail_wire;
    old_detail.as_object_mut().unwrap().remove("status");
    assert_eq!(
        ReadComponent::from_wire_value(&old_detail).unwrap().status,
        None
    );
}

#[test]
fn component_status_mvp_closed_vocabularies_refuse_unknown_values() {
    tick();
    let status = status_stub();
    let summary = ComponentSummary {
        plugin_id: "example.status".to_string(),
        name: "Example Status".to_string(),
        binding: binding_stub(1),
        generation: ExactU64::from(1),
        health: HealthState::Ready,
        freshness: Freshness::Fresh,
        completeness: Completeness::Complete,
        available: true,
        views: Vec::new(),
        status: Some(status),
    };
    let base = summary.to_wire_value().expect("summary projects");

    for (field, value) in [
        ("origin", json!("builtin")),
        ("desired", json!("paused")),
        ("lifecycle", json!("healthy")),
        ("lifecycle_source", json!("durable")),
        ("health", json!("healthy")),
        ("reporting", json!("ok")),
    ] {
        let mut wire = base.clone();
        wire["status"][field] = value;
        let error = ComponentSummary::from_wire_value(&wire)
            .expect_err("unknown vocabulary value must be refused");
        assert!(matches!(error, ContractError::KnownFailure(_)), "{field}: {error}");
    }
    for (field, value) in [
        ("generation", json!(1.5)),
        ("generation", json!("9007199254740993.0")),
        ("lifecycle_observed_at_unix_ms", json!("-1")),
        ("health_observed_at_unix_ms", json!("18446744073709551616")),
        ("incomplete", json!("yes")),
    ] {
        let mut wire = base.clone();
        wire["status"][field] = value;
        ComponentSummary::from_wire_value(&wire)
            .expect_err("non-closed or non-exact status value must be refused");
    }
}

// --- 24. required-case counter ---------------------------------------------

#[test]
fn required_cases_executed_is_nonzero() {
    tick();
    assert!(CASES.load(Ordering::SeqCst) > 0);
}
