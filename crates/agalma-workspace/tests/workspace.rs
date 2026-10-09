//! Integration tests for the workspace service (`WorkspaceApi`).
//!
//! Every test runs against its own repository under `target/test-runs/`, using
//! the real `fixtures/target-template`. No `/tmp`, no network.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_contracts::{ContractError, ExecutionId, WorkspaceApi};
use agalma_workspace::Workspace;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/agalma-workspace
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate must live under <root>/crates/")
        .to_path_buf()
}

fn template_dir() -> PathBuf {
    workspace_root().join("fixtures").join("target-template")
}

/// Fresh run directory; removed and recreated so repeated runs are clean.
fn run_dir(name: &str) -> PathBuf {
    let dir = workspace_root().join("target").join("test-runs").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_opt(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn dir_checksum(dir: &Path) -> u64 {
    fn walk(dir: &Path, hasher: &mut impl Hasher) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        for path in entries {
            path.file_name()
                .expect("file name")
                .to_string_lossy()
                .hash(hasher);
            if path.is_dir() {
                walk(&path, hasher);
            } else {
                fs::read(&path).expect("read file").hash(hasher);
            }
        }
    }
    let mut hasher = DefaultHasher::new();
    walk(dir, &mut hasher);
    hasher.finish()
}

fn fix_template(checkout: &str) {
    let lib = Path::new(checkout).join("src").join("lib.rs");
    let fixed = fs::read_to_string(&lib)
        .expect("read lib.rs")
        .replace("41", "42");
    fs::write(&lib, fixed).expect("write lib.rs");
}

#[test]
fn prepare_creates_baseline_and_candidate() {
    let base = run_dir("workspace-prepare");
    let mut ws = Workspace::new(&base);
    let exec = ExecutionId::new("exec:fix-answer:1");

    let checkout = ws.prepare(template_dir().to_str().unwrap(), &exec).unwrap();
    let repo = Path::new(&checkout.path);

    assert!(repo.join(".git").is_dir());
    assert_eq!(checkout.candidate_branch, "candidate/exec-fix-answer-1");
    assert_eq!(
        git(repo, &["rev-parse", "HEAD"]),
        checkout.base_sha,
        "candidate starts at the baseline"
    );
    assert_eq!(
        git(repo, &["rev-parse", "refs/heads/main"]),
        checkout.base_sha
    );
    assert_eq!(
        git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        checkout.candidate_branch
    );
    // The fixture carries no git metadata; prepare created it.
    assert!(!template_dir().join(".git").exists());
}

#[test]
fn happy_path_integrate_receipt_tag_and_diff() {
    let base = run_dir("workspace-happy");
    let mut ws = Workspace::new(&base);
    let exec = ExecutionId::new("exec:fix-answer:1");

    let checkout = ws.prepare(template_dir().to_str().unwrap(), &exec).unwrap();
    let repo = Path::new(&checkout.path);

    assert_eq!(ws.diff(&checkout).unwrap(), "", "clean tree before edit");
    fix_template(&checkout.path);
    let diff = ws.diff(&checkout).unwrap();
    assert!(diff.contains("42"), "diff shows the edit: {diff}");

    let receipt = ws.integrate(&checkout, &checkout.base_sha).unwrap();
    assert_eq!(receipt.expected_main_sha, checkout.base_sha);
    assert_ne!(receipt.candidate_sha, checkout.base_sha);
    assert_eq!(receipt.result_sha, receipt.candidate_sha);

    assert_eq!(
        git(repo, &["rev-parse", "refs/heads/main"]),
        receipt.result_sha
    );
    assert_eq!(
        git(repo, &["rev-parse", "refs/tags/m0/exec-fix-answer-1"]),
        receipt.result_sha
    );
    // Two commits: baseline + candidate.
    assert_eq!(git(repo, &["rev-list", "--count", "refs/heads/main"]), "2");
}

#[test]
fn integrate_expected_sha_mismatch_is_conflict_and_leaves_main() {
    let base = run_dir("workspace-conflict");
    let mut ws = Workspace::new(&base);
    let exec = ExecutionId::new("exec:fix-answer:1");

    let checkout = ws.prepare(template_dir().to_str().unwrap(), &exec).unwrap();
    let repo = Path::new(&checkout.path);
    fix_template(&checkout.path);

    let err = ws
        .integrate(&checkout, "0000000000000000000000000000000000000000")
        .unwrap_err();
    assert!(
        matches!(err, ContractError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
    assert_eq!(
        git(repo, &["rev-parse", "refs/heads/main"]),
        checkout.base_sha,
        "main unchanged on mismatch"
    );
    assert!(
        git_opt(
            repo,
            &[
                "rev-parse",
                "-q",
                "--verify",
                "refs/tags/m0/exec-fix-answer-1"
            ]
        )
        .is_none(),
        "no tag on mismatch"
    );
}

#[test]
fn reconcile_after_integrate_reports_merged_without_duplicate() {
    let base = run_dir("workspace-reconcile");
    let mut ws = Workspace::new(&base);
    let exec = ExecutionId::new("exec:fix-answer:1");

    let checkout = ws.prepare(template_dir().to_str().unwrap(), &exec).unwrap();
    let repo = Path::new(&checkout.path);
    fix_template(&checkout.path);

    assert!(
        !ws.reconcile(&checkout).unwrap(),
        "not merged before integrate"
    );
    let receipt = ws.integrate(&checkout, &checkout.base_sha).unwrap();

    assert!(ws.reconcile(&checkout).unwrap(), "merged after integrate");
    assert!(ws.reconcile(&checkout).unwrap(), "idempotent reconcile");
    assert_eq!(git(repo, &["rev-list", "--count", "refs/heads/main"]), "2");
    assert_eq!(
        git(repo, &["rev-parse", "refs/heads/main"]),
        receipt.result_sha
    );
}

#[test]
fn revert_restores_previous_main() {
    let base = run_dir("workspace-revert");
    let mut ws = Workspace::new(&base);
    let exec = ExecutionId::new("exec:fix-answer:1");

    let checkout = ws.prepare(template_dir().to_str().unwrap(), &exec).unwrap();
    let repo = Path::new(&checkout.path);
    fix_template(&checkout.path);
    ws.integrate(&checkout, &checkout.base_sha).unwrap();
    assert_ne!(
        git(repo, &["rev-parse", "refs/heads/main"]),
        checkout.base_sha
    );

    ws.revert(&checkout.base_sha).unwrap();
    assert_eq!(
        git(repo, &["rev-parse", "refs/heads/main"]),
        checkout.base_sha
    );
}

#[test]
fn prepare_rejects_template_with_git_and_preserves_template() {
    let base = run_dir("workspace-reject-git");
    let template = base.join("template-with-git");
    fs::create_dir_all(template.join("src")).unwrap();
    fs::create_dir_all(template.join(".git")).unwrap();
    fs::write(
        template.join("src/lib.rs"),
        "pub fn answer() -> u32 { 41 }\n",
    )
    .unwrap();

    let mut ws = Workspace::new(&base);
    let exec = ExecutionId::new("exec:fix-answer:1");
    let err = ws.prepare(template.to_str().unwrap(), &exec).unwrap_err();
    assert!(
        matches!(err, ContractError::KnownFailure(_)),
        "expected KnownFailure, got {err:?}"
    );
}

#[test]
fn prepare_leaves_template_untouched() {
    let base = run_dir("workspace-untouched");
    let mut ws = Workspace::new(&base);
    let exec = ExecutionId::new("exec:fix-answer:1");

    let before = dir_checksum(&template_dir());
    ws.prepare(template_dir().to_str().unwrap(), &exec).unwrap();
    assert_eq!(before, dir_checksum(&template_dir()));
}
