//! `agalma-contracts` canonical hashing + failure-detail acceptance tests (SF.2).
//!
//! Golden vectors are independent inputs from
//! `docs/contracts/self-mutation-v1/canonical-vectors.json`; the invalid
//! `task-package` records come from `invalid-records.json`. No network, no
//! `/tmp`.

use agalma_contracts::digest::{
    canonical_digest, canonical_json, failure_code, is_sha256, sha256_digest, sha256_hex,
    validate_digest, FailureCode,
};
use agalma_contracts::{ContractError, TaskPackage};
use serde::Deserialize;
use serde_json::{json, Value};

const VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/contracts/self-mutation-v1/canonical-vectors.json"
));
const INVALID: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/contracts/self-mutation-v1/invalid-records.json"
));

#[derive(Debug, Deserialize)]
struct Vector {
    input: Value,
    canonical_utf8: String,
    digest: String,
}

#[derive(Debug, Deserialize)]
struct InvalidRecord {
    name: String,
    expected_code: String,
    record: Value,
}

#[test]
fn golden_vectors_encode_and_digest() {
    let vectors: Vec<Vector> = serde_json::from_str(VECTORS).expect("vectors parse");
    assert!(!vectors.is_empty(), "vectors are non-empty");
    for vector in vectors {
        let canonical = canonical_json(&vector.input)
            .unwrap_or_else(|e| panic!("{}: {e}", vector.canonical_utf8));
        assert_eq!(
            canonical, vector.canonical_utf8,
            "canonical-json/v1 mismatch for {}",
            vector.canonical_utf8
        );
        assert_eq!(
            canonical_digest(&vector.input).expect("digest"),
            vector.digest,
            "sha256 mismatch for {}",
            vector.canonical_utf8
        );
    }
}

#[test]
fn unsorted_keys_are_reordered_by_byte_order() {
    // Insertion order differs from byte order; the encoder must sort.
    let value = json!({ "z": 1, "a": "é", "nested": { "b": 2, "a": 1 } });
    let canonical = canonical_json(&value).expect("canonical");
    assert_eq!(canonical, "{\"a\":\"é\",\"nested\":{\"a\":1,\"b\":2},\"z\":1}");
}

#[test]
fn floating_point_is_rejected() {
    let err = canonical_json(&json!({ "x": 1.5 })).expect_err("float rejected");
    assert!(matches!(err, ContractError::KnownFailure(_)));
    assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));
}

#[test]
fn sha256_matches_known_vector_and_empty_hash() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_digest(b"abc"),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn digest_shape_is_validated() {
    assert!(is_sha256(&sha256_digest(b"x")));
    assert!(!is_sha256("sha256:ABCDEF"));
    assert!(!is_sha256("fnv1a64:0000000000000000"));
    assert!(!is_sha256("sha256:short"));
    let err = validate_digest("fnv1a64:0000000000000000", "artifact").expect_err("legacy refused");
    assert!(matches!(err, ContractError::KnownFailure(_)));
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));
}

#[test]
fn failure_codes_map_to_the_revision1_variants() {
    for (code, known) in [
        (FailureCode::InvalidRecord, true),
        (FailureCode::IntegrityMismatch, true),
        (FailureCode::LegacyInputsMissing, true),
        (FailureCode::UnsupportedVersion, false),
        (FailureCode::UnsupportedSchema, false),
    ] {
        let err = code.failure("subject", "message");
        assert_eq!(
            matches!(err, ContractError::KnownFailure(_)),
            known,
            "{} variant",
            code.as_str()
        );
        assert_eq!(failure_code(&err), Some(code), "{} round-trips", code.as_str());
        let payload = match &err {
            ContractError::KnownFailure(m)
            | ContractError::UnsupportedCapability(m)
            | ContractError::Conflict(m)
            | ContractError::UnknownOutcome(m) => m,
        };
        let detail: Value = serde_json::from_str(payload).expect("FailureDetail JSON");
        assert_eq!(detail["format"], "FailureDetail/v1");
        assert_eq!(detail["code"], code.as_str());
    }
}

#[test]
fn invalid_task_package_records_map_to_expected_codes() {
    let records: Vec<InvalidRecord> = serde_json::from_str(INVALID).expect("invalid records parse");
    let mut checked = 0;
    for record in records {
        if !record.name.starts_with("task-package:") {
            continue;
        }
        checked += 1;
        let package: TaskPackage =
            serde_json::from_value(record.record).expect("record matches TaskPackage/v1");
        let err = match package.verify() {
            Ok(()) => panic!("{} must fail verify", record.name),
            Err(err) => err,
        };
        let code = failure_code(&err)
            .unwrap_or_else(|| panic!("{}: structured detail expected", record.name));
        assert_eq!(
            code.as_str(),
            record.expected_code,
            "{} expected {}",
            record.name,
            record.expected_code
        );
    }
    assert_eq!(checked, 2, "both task-package invalid records are covered");
}
