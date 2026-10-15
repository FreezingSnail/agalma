//! Shared test-only fixtures for the conductor integration tests.
//!
//! Repository-mode harness fixtures construct `PhaseDeps` directly with a fresh
//! scratch state dir. SF.1 (`agalma-5fy.1`) makes repository mode **require** the
//! recorded immutable `TaskPackage/v1`: with no package the strict render refuses
//! instead of falling back. These helpers seed a minimal *valid* recorded package
//! (with a frozen instruction object) into the scratch state dir so a harness
//! exercises the strict production path rather than a fallback. No package, no
//! green build — the fixture is the recorded input the strict path demands.

#![allow(dead_code)]

use std::path::Path;

use agalma_conductor::task_package::{object_path, object_relative_path, package_path};
use agalma_contracts::{
    sha256_digest, Authorization, AuthorizationOrigin, Budget, ContextItem, ContextKind, Definition,
    DirectiveRef, PackageArtifactRef, RepositoryRef, Scope, TaskPackage, TextObject, UnknownUsage,
    TASK_PACKAGE_FORMAT,
};

/// Marker text seeded as the package's frozen repository instruction.
pub const SEED_INSTRUCTION: &str = "harness fixture: frozen repository instruction\n";

/// Write `bytes` as a content-addressed object; return its reference.
fn write_object(state_dir: &Path, bytes: &[u8]) -> PackageArtifactRef {
    let digest = sha256_digest(bytes);
    let relative = object_relative_path(&digest).expect("object relative path");
    let full = object_path(state_dir, &digest).expect("object path");
    std::fs::create_dir_all(full.parent().expect("object parent")).expect("objects dir");
    std::fs::write(&full, bytes).expect("write object");
    PackageArtifactRef {
        path: relative,
        digest,
        bytes: bytes.len() as u64,
        media_type: "text/plain".to_string(),
    }
}

/// Seed a minimal valid recorded package for `exec:<task_id>:<generation>`.
///
/// The document and every referenced object are persisted under the scratch
/// state dir's frozen package store, so `resolve_recorded_package` resolves it
/// and the strict render produces the full frozen context.
pub fn seed_recorded_package(state_dir: &Path, task_id: &str, generation: u32) {
    let instruction = ContextItem {
        kind: ContextKind::Instruction,
        source: "AGENTS.md".to_string(),
        artifact: write_object(state_dir, SEED_INSTRUCTION.as_bytes()),
    };
    let receipt = write_object(state_dir, b"{}");
    let oracle = write_object(state_dir, b"harness-oracle");
    let package = TaskPackage {
        format: TASK_PACKAGE_FORMAT.to_string(),
        task_id: task_id.to_string(),
        execution_id: format!("exec:{task_id}:{generation}"),
        generation: generation as u64,
        lease_generation: 1,
        repository: RepositoryRef {
            identity: "harness-fixture".to_string(),
            base_sha: "a".repeat(40),
        },
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 0,
            digest: sha256_digest(b"directive/v0"),
        },
        authorization: Authorization {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"human-bootstrap/v1"),
            receipt,
        },
        definition: Definition {
            title: "Harness fixture task".to_string(),
            description: TextObject::new("fixture/description", "# Scope\nHarness fixture.\n"),
            design: TextObject::new("fixture/design", ""),
            notes: TextObject::new("fixture/notes", ""),
            machine_block: String::new(),
            acceptance: vec!["sh -c 'true'".to_string()],
        },
        context: vec![instruction],
        scope: Scope {
            allowed_paths: Vec::new(),
            protected_paths: Vec::new(),
            readable_artifacts: Vec::new(),
            oracle,
        },
        max_attempts: 2,
        budget: Budget {
            allowance_nusd: 0,
            unknown_usage: UnknownUsage::Deny,
            quota_policy_digest: sha256_digest(b"quota"),
        },
        roles: Vec::new(),
        recorded_at_unix_ms: 1_791_820_800_000,
        package_digest: String::new(),
    }
    .seal()
    .expect("seal harness package");

    let path = package_path(state_dir, &package.package_digest).expect("package path");
    std::fs::create_dir_all(path.parent().expect("package parent")).expect("package dir");
    std::fs::write(&path, serde_json::to_vec(&package).expect("serialize package"))
        .expect("write harness package");
}

/// Seed generations `1..=max_generation` for `task_id`.
pub fn seed_recorded_packages(state_dir: &Path, task_id: &str, max_generation: u32) {
    for generation in 1..=max_generation {
        seed_recorded_package(state_dir, task_id, generation);
    }
}
