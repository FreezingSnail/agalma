//! Protected fixture-pack integrity gate (B0.1 `agalma-9n4.2`).
//!
//! `fixtures/recovery-pack/MANIFEST.sha256` pins every pack file; this test
//! recomputes each digest and fails on any change, addition, or removal. A
//! tamper self-test copies the pack to `target/test-runs/` and proves
//! change/addition/removal are all detected, so the gate itself cannot silently
//! rot. Candidates cannot edit the tests that judge them.
//!
//! Run dirs live under `target/test-runs/` (never a system temp directory).

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Acceptance cases the pack must contain (ids = file stems in `cases/`).
const EXPECTED_CASES: [&str; 10] = [
    "transition-commit-before-dispatch",
    "duplicate-delivery-after-completion",
    "restart-after-decision-commit",
    "merge-before-completion",
    "stale-lease-worker",
    "restart-during-retry-delay",
    "directive-update-restart",
    "upgrade-with-pending-executions",
    "kernel-adapter-replacement",
    "rollback-restore-reconcile",
];

/// Scratch directory under `target/test-runs/`; returns a fresh empty dir.
///
/// WARNING: deletes files, but only the freshly-minted unique directory. The
/// base is confined to the workspace `target/` tree; it never touches a system
/// temp directory.
fn scratch_dir(name: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let dir = target
        .join("test-runs")
        .join(format!("pack-integrity-{name}-{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("reset scratch dir");
    }
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create copy dir");
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            fs::copy(&path, &target).expect("copy file");
        }
    }
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn protected_pack_matches_manifest() {
    let root = support::pack::pack_root();
    assert!(
        root.join(support::pack::MANIFEST_FILE).exists(),
        "manifest missing at {}",
        root.display()
    );
    if let Err(issues) = support::pack::verify_pack(&root) {
        panic!(
            "protected recovery pack changed — candidates must not edit it:\n{}",
            issues.join("\n")
        );
    }
}

#[test]
fn every_case_file_matches_schema() {
    let cases_dir = support::pack::pack_root().join("cases");
    let mut ids = BTreeSet::new();
    for entry in fs::read_dir(&cases_dir).expect("cases dir") {
        let path = entry.expect("case entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read case");
        let value: Value = serde_json::from_str(&text).expect("case json");
        for field in [
            "id",
            "description",
            "crash_point",
            "expected_observable",
            "fixture_refs",
        ] {
            assert!(
                value.get(field).is_some(),
                "{} missing required field `{field}`",
                path.display()
            );
        }
        let id = value["id"].as_str().expect("case id string").to_string();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("case stem")
            .to_string();
        assert_eq!(id, stem, "case id must match its file name");
        ids.insert(id);
    }

    let expected: BTreeSet<String> = EXPECTED_CASES.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        ids, expected,
        "case set drifted from the accepted pack; got {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Self-test: the gate detects tampering
// ---------------------------------------------------------------------------

#[test]
fn tamper_detection_self_test() {
    let original = support::pack::pack_root();

    // Pristine original verifies; the copy starts identical.
    assert!(
        support::pack::verify_pack(&original).is_ok(),
        "pristine pack must verify"
    );
    let copy = scratch_dir("tamper").join("recovery-pack");
    copy_dir(&original, &copy);
    assert!(
        support::pack::verify_pack(&copy).is_ok(),
        "an untouched copy must verify"
    );

    let victim = copy.join("cases/transition-commit-before-dispatch.json");

    // (1) change: one byte appended to a listed file is detected.
    let mut changed = fs::read_to_string(&victim).expect("read victim");
    changed.push('\n');
    fs::write(&victim, changed).expect("tamper");
    let issues = support::pack::verify_pack(&copy).expect_err("change must be detected");
    assert!(
        issues
            .iter()
            .any(|m| m.contains("transition-commit-before-dispatch.json")),
        "change not reported: {issues:?}"
    );
    fs::copy(
        original.join("cases/transition-commit-before-dispatch.json"),
        &victim,
    )
    .expect("restore victim");
    assert!(support::pack::verify_pack(&copy).is_ok());

    // (2) addition: an unlisted file is detected.
    let rogue = copy.join("cases/rogue-case.json");
    fs::write(&rogue, "{}").expect("add rogue");
    let issues = support::pack::verify_pack(&copy).expect_err("addition must be detected");
    assert!(
        issues
            .iter()
            .any(|m| m.contains("addition") && m.contains("rogue-case.json")),
        "addition not reported: {issues:?}"
    );
    fs::remove_file(&rogue).expect("remove rogue");
    assert!(support::pack::verify_pack(&copy).is_ok());

    // (3) removal: a listed file that vanished is detected.
    fs::remove_file(&victim).expect("remove victim");
    let issues = support::pack::verify_pack(&copy).expect_err("removal must be detected");
    assert!(
        issues.iter().any(|m| m.contains("missing file")),
        "removal not reported: {issues:?}"
    );

    // The real pack is never touched by the self-test.
    assert!(support::pack::verify_pack(&original).is_ok());
}
