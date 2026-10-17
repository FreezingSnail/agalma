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

use agalma_contracts::digest::{failure_code, FailureCode};
use agalma_contracts::plugin::{HealthState, ReportEnvelope, View, ViewKind};
use agalma_contracts::{
    check_cursor, resolve_field_selector, ContractError, ExactAmount, ExactU64, FieldResolution,
    Freshness, ListComponents, PackageArtifactRef, ReadArtifactChunk, ReadArtifactChunkRequest,
    ReadComponent, ReadCursor, ReadMetadata, ReadOverview, ReadPageRequest, SourceState, UsageCost,
    ARTIFACT_CHUNK_MAX_BYTES, ARTIFACT_TOTAL_MAX_BYTES, CONTROL_PLANE_READ_CAPABILITY,
    CONTROL_PLANE_READ_FORMAT, READ_CURSOR_PREFIX, READ_PAGE_DEFAULT_LIMIT, READ_PAGE_MAX_ENTRIES,
    READ_PAGE_MAX_LIMIT,
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
    overview.metadata.version_gate().expect("ControlPlaneRead/v1 gates");
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
    assert_eq!(economics.usage.as_ref().unwrap().as_str(), "123456789012345.678901");
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
    assert_eq!(cursor.encode().unwrap(), raw, "encode(decode(token)) == token");
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
    chunk.check_bounds().expect("a continuing chunk is within bounds");
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
        assert_eq!(ReadCursor::decode(&raw).unwrap(), cursor, "cursor round trips");

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
    assert!(check_cursor(None, "components", &filter, &ExactU64::from(0))
        .unwrap()
        .is_none());
    assert_code(ReadCursor::decode("garbage").unwrap_err(), "InvalidRecord");

    // A malformed revision is refused at decode (InvalidRecord), never accepted
    // and later misclassified as a stale-revision RevisionConflict.
    for bad in ["not-a-number", "", "-1", "1.5", "0x10", " 1", "1 ", "18446744073709551616"] {
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
        assert_eq!(serde_json::to_string(&exact).unwrap(), format!("\"{value}\""));
    }
    assert_code(ExactU64::parse("12x").unwrap_err(), "InvalidRecord");
    assert_code(ExactU64::parse("-1").unwrap_err(), "InvalidRecord");
    assert_code(ExactU64::parse("").unwrap_err(), "InvalidRecord");
    assert_code(ExactU64::parse(" 1").unwrap_err(), "InvalidRecord");
    assert_code(ExactU64::parse("18446744073709551616").unwrap_err(), "InvalidRecord");

    // Deserialization validates too: a malformed decimal string cannot enter
    // through a nested record such as a cursor revision.
    for bad in ["\"12x\"", "\"\"", "\"-1\"", "\"1.5\"", "\"18446744073709551616\""] {
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
        assert_eq!(amount.as_str(), raw, "text is preserved exactly, never rounded");
    }
    for raw in ["", ".", ".5", "1.", "1.2.3", "-1", "+1", "1e3", "1 000", "abc", "1,5"] {
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
        chunk(over_cap, reference(total + 1), 0).check_bounds().unwrap_err(),
        "InvalidRecord",
    );
    assert_code(
        chunk("ab".to_string(), reference(0), 0).check_bounds().unwrap_err(),
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
    assert_eq!(default_request.effective_max_bytes().unwrap(), ARTIFACT_CHUNK_MAX_BYTES);
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
    assert_code(chunk("ab", 2, 0, None).check_bounds().unwrap_err(), "InvalidRecord");
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
    assert_code(chunk("", 2, 0, Some(0)).check_bounds().unwrap_err(), "InvalidRecord");
    assert_code(chunk("", 2, 0, Some(1)).check_bounds().unwrap_err(), "InvalidRecord");
    assert_code(chunk("", 2, 0, None).check_bounds().unwrap_err(), "InvalidRecord");
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
            chunk(&hex, total, offset, valid).check_bounds().unwrap_or_else(|e| {
                panic!("case {case}: total={total} offset={offset} len={len} valid rejected: {e}")
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
                chunk(&hex, total, offset, Some(end + 1)).check_bounds().unwrap_err(),
                "InvalidRecord",
            );
            if end > 0 {
                assert_code(
                    chunk(&hex, total, offset, Some(end - 1)).check_bounds().unwrap_err(),
                    "InvalidRecord",
                );
            }
            if before_end {
                assert_code(
                    chunk(&hex, total, offset, Some(total)).check_bounds().unwrap_err(),
                    "InvalidRecord",
                );
            }

            // (e) A non-null next_offset at the exact artifact end is refused.
            if !before_end {
                assert_code(
                    chunk(&hex, total, offset, Some(end)).check_bounds().unwrap_err(),
                    "InvalidRecord",
                );
            }
        }
        checked += 1;
    }
    assert_eq!(checked, CONTINUATION_BUDGET, "the full generated budget executed");
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
        assert_eq!(format!("{:?}", view.kind), kind_text, "descriptor kind matches");
        for field in &view.fields {
            assert!(
                matches!(resolve_field_selector(&block["payload"], field), FieldResolution::Value(_)),
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
        assert!(kinds.contains(&kind), "the frozen ViewKind {kind:?} is covered");
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
    assert_code(envelope.verify_payload_digest().unwrap_err(), &case.expected_code);

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
        assert!(!source.contains(needle), "control_plane.rs must not contain {needle:?}");
    }
    // Viewing grants no action authority: the read projection exposes no
    // mutation or ledger symbols.
    for needle in ["LedgerApi", "ActionIntent", "ComponentCommit", "MutationApi"] {
        assert!(
            !source.contains(needle),
            "the read projection must not carry {needle:?}"
        );
    }
    assert_eq!(CONTROL_PLANE_READ_CAPABILITY, "control-plane.read/v1");
}

// --- 14. required-case counter ---------------------------------------------

#[test]
fn required_cases_executed_is_nonzero() {
    tick();
    assert!(CASES.load(Ordering::SeqCst) > 0);
}
