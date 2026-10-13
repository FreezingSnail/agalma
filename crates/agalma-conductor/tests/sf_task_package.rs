//! SF.2 wave-1 acceptance: immutable `TaskPackage/v1` build, persist, and load.
//!
//! Each test runs against its own scratch origin repository under
//! `target/test-runs/sf-pack-*`: a real `git` origin with a real isolated `bd`
//! store. No `/tmp`, no network. Scratch dirs auto-remove on success (kept on
//! failure for forensics).
//!
//! Wave 1 proves the package survives backlog edits and deletion of the source
//! issue, and that a corrupt digest / unsupported version refuses effects. It
//! does not exercise the `work.rs` admission hook or crash boundaries (wave 2).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agalma_contracts::digest::{failure_code, FailureCode};
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, DirectiveRef, PackageArtifactRef, Role, TaskId, TaskPackage,
};
use agalma_conductor::task_package::{
    build_package, load_package, object_path, package_path, persist_package, read_artifact,
    AuthorizationInput, ContextInput, PackageInputs, RoleInput,
};
use agalma_taskqueue::BdTaskQueue;
use agalma_testkit::ScratchDir;
use serde_json::Value;

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        stderr_of(&out)
    );
}

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

/// A scratch origin repo (git + isolated bd store).
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        require_bd();
        let dir = fresh(&format!("sf-pack-{name}"));
        let origin = dir.join("origin");
        fs::create_dir_all(origin.join("src")).expect("origin src");
        fs::write(
            origin.join("src").join("lib.rs"),
            "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
        )
        .expect("seed source");

        run_ok("git", &origin, &["init", "-q", "-b", "main"]);
        run_ok("git", &origin, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &origin, &["config", "user.name", "Agalma"]);
        run_ok("git", &origin, &["add", "-A"]);
        run_ok("git", &origin, &["commit", "-qm", "seed"]);

        let prefix = name.replace('-', "");
        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                &prefix,
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        Scratch {
            origin,
            state_dir: dir.join("state"),
            _dir: dir,
        }
    }

    fn bd(&self, args: &[&str]) -> Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.origin.join(".beads"))
            .args(args)
            .current_dir(&self.origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn bd_ok(&self, args: &[&str]) -> String {
        let out = self.bd(args);
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn create(&self, title: &str, description: &str) -> String {
        let stdout = self.bd_ok(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            description,
            "--json",
        ]);
        let value: Value = serde_json::from_str(&stdout).expect("bd create json");
        value["id"].as_str().expect("created id").to_string()
    }

    fn update(&self, id: &str, args: &[&str]) {
        let mut full = vec!["update", id];
        full.extend_from_slice(args);
        self.bd_ok(&full);
    }

    fn head_sha(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.origin)
            .output()
            .expect("git rev-parse");
        assert!(out.status.success(), "git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn queue(&self) -> BdTaskQueue {
        BdTaskQueue::new(&self.origin)
    }
}

fn binding_pin() -> BindingPin {
    BindingPin {
        binding_id: "fixture.harness".to_string(),
        api: "harness".to_string(),
        api_version: 1,
        implementation_version: "fixture-1".to_string(),
        generation: 1,
        config_digest: agalma_contracts::digest::sha256_digest(b"{}"),
    }
}

fn inputs(
    scratch: &Scratch,
    definition: &agalma_taskqueue::TaskDefinition,
    shared_design: &[u8],
    handoff: &[u8],
) -> PackageInputs {
    PackageInputs {
        execution_id: format!("exec:{}:1", definition.task_id.as_str()),
        generation: 1,
        lease_generation: 1,
        repository_identity: "agalma-fixture".to_string(),
        base_sha: scratch.head_sha(),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 1,
            digest: agalma_contracts::digest::sha256_digest(b"directive/v0"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: agalma_contracts::digest::sha256_digest(b"policy"),
            receipt_source: "objects/authorization.json".to_string(),
            receipt_media_type: "application/json".to_string(),
            receipt_bytes: br#"{"authorized":true}"#.to_vec(),
        },
        roles: vec![
            RoleInput::new(
                Role::Builder,
                "opencode-go/deepseek-v4.1-flash",
                "deepseek-v4.1-flash",
                binding_pin(),
                PackageArtifactRef::new("objects/builder-prompt.txt", b"builder prompt", "text/plain"),
            ),
            RoleInput::new(
                Role::Verifier,
                "fixture.verifier",
                "fixture-model",
                binding_pin(),
                PackageArtifactRef::new("objects/verifier-prompt.txt", b"verifier prompt", "text/plain"),
            ),
        ],
        instructions: vec![ContextInput::new(
            agalma_contracts::ContextKind::Instruction,
            "AGENTS.md",
            "text/plain",
            b"repo rules\n".to_vec(),
        )],
        shared_design: vec![ContextInput::new(
            agalma_contracts::ContextKind::SharedDesign,
            "docs/self-mutation-contracts.md",
            "text/plain",
            shared_design.to_vec(),
        )],
        parent_design: vec![ContextInput::new(
            agalma_contracts::ContextKind::ParentDesign,
            "bd:epic",
            "text/plain",
            b"parent epic design\n".to_vec(),
        )],
        dependency_handoffs: vec![ContextInput::new(
            agalma_contracts::ContextKind::DependencyHandoff,
            "bd:dependency",
            "text/plain",
            handoff.to_vec(),
        )],
        allowed_paths: vec!["crates/agalma-conductor".to_string()],
        protected_paths: vec!["fixtures/protected/".to_string()],
        readable_artifacts: Vec::new(),
        oracle: PackageArtifactRef::new("objects/oracle.txt", b"oracle", "text/plain"),
        max_attempts: 2,
        allowance_nusd: 2_000_000_000,
        quota_policy_digest: agalma_contracts::digest::sha256_digest(b"quota"),
        recorded_at_unix_ms: 1_791_820_800_000,
    }
}

fn seed_issue(scratch: &Scratch) -> (String, String) {
    // A prerequisite issue and the target, wired with a real `blocks` edge.
    let prerequisite = scratch.create("Prerequisite", "Seed prerequisite body.");
    let description = "Task body with fenced block.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\npriority: high\nfamily: feature\nmax_attempts: 2\n```\n";
    let id = scratch.create("Immutable context task", description);
    scratch.update(
        &id,
        &[
            "--design",
            "Original design.\n",
            "--notes",
            "Original notes.\n",
            "--acceptance",
            "Original acceptance text.",
        ],
    );
    scratch.bd_ok(&["dep", "add", &id, &prerequisite]);
    (id, prerequisite)
}

#[test]
fn frozen_package_survives_edits_and_source_deletion() {
    let scratch = Scratch::new("immutable");
    let (id, prerequisite) = seed_issue(&scratch);
    let queue = scratch.queue();
    let task_id = TaskId::new(id.as_str());

    let definition = queue.task_definition(&task_id).expect("definition");
    assert_eq!(definition.title, "Immutable context task");
    assert_eq!(definition.design.as_deref(), Some("Original design.\n"));
    assert_eq!(definition.notes.as_deref(), Some("Original notes.\n"));
    assert_eq!(
        definition.acceptance_criteria.as_deref(),
        Some("Original acceptance text.")
    );
    assert!(
        definition
            .dependencies
            .iter()
            .any(|dep| dep.id == prerequisite && dep.kind.as_deref() == Some("blocks")),
        "nested show-shape dependency parsed: {:?}",
        definition.dependencies
    );

    // The shared-design and dependency-handoff bytes are captured into the store.
    let shared_design = b"# Shared design revision 1\nFrozen bytes.\n".to_vec();
    let handoff = format!("handoff from {prerequisite}\n").into_bytes();
    let built = build_package(
        &definition,
        inputs(&scratch, &definition, &shared_design, &handoff),
    )
    .expect("build package");
    built.package.verify().expect("built package verifies");
    let persisted = persist_package(&scratch.state_dir, &built).expect("persist");

    let package_file = scratch.state_dir.join(&persisted.path);
    let original_bytes = fs::read(&package_file).expect("read persisted package");

    // Locate the shared-design and dependency-handoff context refs.
    let shared_ref = built
        .package
        .context
        .iter()
        .find(|c| c.kind == agalma_contracts::ContextKind::SharedDesign)
        .expect("shared design context")
        .artifact
        .clone();
    let handoff_ref = built
        .package
        .context
        .iter()
        .find(|c| c.kind == agalma_contracts::ContextKind::DependencyHandoff)
        .expect("dependency handoff context")
        .artifact
        .clone();
    assert!(
        shared_ref.path.starts_with("packages/objects/sha256/"),
        "context object is content-addressed: {}",
        shared_ref.path
    );

    // Admission happened; now the backlog changes and the source issue vanishes.
    scratch.update(
        &id,
        &[
            "--title",
            "Mutated title",
            "--design",
            "MUTATED design.",
            "--notes",
            "MUTATED notes.",
            "--acceptance",
            "MUTATED acceptance.",
            "--description",
            "MUTATED body without any fence.",
        ],
    );
    scratch.bd_ok(&["delete", &id, "--force"]);
    assert!(
        queue.task_definition(&task_id).is_err(),
        "source issue is gone"
    );

    // Recovery loads the frozen package: byte-identical, original fields intact.
    let loaded = load_package(&scratch.state_dir, &persisted.digest).expect("load frozen");
    assert_eq!(loaded.definition.title, "Immutable context task");
    assert_eq!(loaded.definition.design.text, "Original design.\n");
    assert_eq!(loaded.definition.notes.text, "Original notes.\n");
    assert_eq!(
        loaded.definition.description.text,
        definition.description,
        "original body bytes retained"
    );
    assert_eq!(loaded.scope.allowed_paths, vec!["crates/agalma-conductor"]);
    assert_eq!(loaded.max_attempts, 2);
    assert_eq!(loaded.roles.len(), 2);

    let reloaded_bytes = fs::read(&package_file).expect("re-read persisted package");
    assert_eq!(reloaded_bytes, original_bytes, "package bytes are immutable");

    // The referenced shared-design/handoff objects are readable and intact.
    assert_eq!(
        read_artifact(&scratch.state_dir, &shared_ref).expect("read shared design"),
        shared_design
    );
    assert_eq!(
        read_artifact(&scratch.state_dir, &handoff_ref).expect("read handoff"),
        handoff
    );
}

#[test]
fn corrupt_package_digest_is_refused() {
    let scratch = Scratch::new("corrupt");
    let (id, _) = seed_issue(&scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let built = build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");
    let persisted = persist_package(&scratch.state_dir, &built).expect("persist");

    // Overwrite the stored document with a wrong-but-well-formed digest.
    let path = package_path(&scratch.state_dir, &persisted.digest).expect("path");
    let text = fs::read_to_string(&path).expect("read");
    let mut value: Value = serde_json::from_str(&text).expect("json");
    value["package_digest"] =
        Value::String(format!("sha256:{}", "0".repeat(64)));
    fs::write(&path, serde_json::to_string(&value).expect("encode")).expect("write");

    let err = load_package(&scratch.state_dir, &persisted.digest).expect_err("corrupt refused");
    assert!(matches!(err, agalma_contracts::ContractError::KnownFailure(_)));
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));
}

#[test]
fn unsupported_package_version_is_refused() {
    let scratch = Scratch::new("version");
    let (id, _) = seed_issue(&scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let built = build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");

    // Write a forward-versioned document directly (the build path only seals v1).
    let mut package: TaskPackage = built.package.clone();
    package.format = "TaskPackage/v99".to_string();
    let digest = package.package_digest().expect("recompute digest");
    let document = agalma_contracts::digest::canonical_json(
        &serde_json::to_value(&package).expect("encode"),
    )
    .expect("canonical");
    let path = package_path(&scratch.state_dir, &digest).expect("path");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, document.as_bytes()).expect("write v99");

    let err = load_package(&scratch.state_dir, &digest).expect_err("v99 refused");
    assert!(matches!(
        err,
        agalma_contracts::ContractError::UnsupportedCapability(_)
    ));
    assert_eq!(failure_code(&err), Some(FailureCode::UnsupportedVersion));
}

#[test]
fn missing_package_parks_as_legacy_inputs_missing() {
    let scratch = Scratch::new("legacy");
    let missing = format!("sha256:{}", "a".repeat(64));
    let err = load_package(&scratch.state_dir, &missing).expect_err("missing parks");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));

    let err = agalma_conductor::task_package::load_frozen_or_legacy(
        &scratch.state_dir,
        None,
        "exec:legacy:1",
    )
    .expect_err("no reference parks");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
}

/// `object_path` and `package_path` agree with what `persist_package` wrote.
#[test]
fn store_layout_matches_advertised_paths() {
    let scratch = Scratch::new("layout");
    let (id, _) = seed_issue(&scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let built = build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");
    let persisted = persist_package(&scratch.state_dir, &built).expect("persist");

    let expected_package = package_path(&scratch.state_dir, &persisted.digest).expect("pkg path");
    assert!(expected_package.exists(), "package stored at its digest path");
    assert!(!scratch.state_dir.join("runs").join("packages").exists());

    for (digest, _) in &built.objects {
        let expected = object_path(&scratch.state_dir, digest).expect("obj path");
        assert!(expected.exists(), "object {} stored", digest);
    }
}
