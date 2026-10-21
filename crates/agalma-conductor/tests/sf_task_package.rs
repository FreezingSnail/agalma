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

use agalma_conductor::composition::BUILDER_PROMPT;
use agalma_conductor::harness_binding::{harness_handle, HarnessChoice, HarnessKind};
use agalma_conductor::phases::VERIFIER_PROMPT;
use agalma_conductor::task_package::{
    build_package, gather_admission_context, load_admitted_package, load_package, object_path,
    package_path, persist_package, read_artifact, verify_references, AuthorizationInput,
    ContextInput, PackageInputs, RoleInput,
};
use agalma_conductor::work::{binding_pin as real_binding_pin, verify_recovered_pins};
use agalma_contracts::digest::{failure_code, sha256_digest, FailureCode};
use agalma_contracts::harness::{API_VERSION, HARNESS_API};
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, CommitBatch, ContextKind, ContractError, DirectiveRef,
    ExecutionApi, ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord, ExecutionState,
    LedgerApi, OperationId, Role, StepOutcome, TaskId, TaskPackage,
};
use agalma_execution::executor::Preparation;
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;
use agalma_taskqueue::{BdTaskQueue, TaskDefinition, TaskDependency};
use agalma_testkit::ScratchDir;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

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
        run_ok(
            "git",
            &origin,
            &["config", "user.email", "agalma@localhost"],
        );
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
                b"builder prompt".to_vec(),
                "text/plain",
            ),
            RoleInput::new(
                Role::Verifier,
                "fixture.verifier",
                "fixture-model",
                binding_pin(),
                b"verifier prompt".to_vec(),
                "text/plain",
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
        oracle_bytes: b"oracle".to_vec(),
        oracle_media_type: "text/plain".to_string(),
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
        loaded.definition.description.text, definition.description,
        "original body bytes retained"
    );
    assert_eq!(loaded.scope.allowed_paths, vec!["crates/agalma-conductor"]);
    assert_eq!(loaded.max_attempts, 2);
    assert_eq!(loaded.roles.len(), 2);

    let reloaded_bytes = fs::read(&package_file).expect("re-read persisted package");
    assert_eq!(
        reloaded_bytes, original_bytes,
        "package bytes are immutable"
    );

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
    let built =
        build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");
    let persisted = persist_package(&scratch.state_dir, &built).expect("persist");

    // Overwrite the stored document with a wrong-but-well-formed digest.
    let path = package_path(&scratch.state_dir, &persisted.digest).expect("path");
    let text = fs::read_to_string(&path).expect("read");
    let mut value: Value = serde_json::from_str(&text).expect("json");
    value["package_digest"] = Value::String(format!("sha256:{}", "0".repeat(64)));
    fs::write(&path, serde_json::to_string(&value).expect("encode")).expect("write");

    let err = load_package(&scratch.state_dir, &persisted.digest).expect_err("corrupt refused");
    assert!(matches!(
        err,
        agalma_contracts::ContractError::KnownFailure(_)
    ));
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
    let built =
        build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");

    // Write a forward-versioned document directly (the build path only seals v1).
    let mut package: TaskPackage = built.package.clone();
    package.format = "TaskPackage/v99".to_string();
    let digest = package.package_digest().expect("recompute digest");
    let document =
        agalma_contracts::digest::canonical_json(&serde_json::to_value(&package).expect("encode"))
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
    let built =
        build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");
    let persisted = persist_package(&scratch.state_dir, &built).expect("persist");

    let expected_package = package_path(&scratch.state_dir, &persisted.digest).expect("pkg path");
    assert!(
        expected_package.exists(),
        "package stored at its digest path"
    );
    assert!(!scratch.state_dir.join("runs").join("packages").exists());

    for (digest, _) in &built.objects {
        let expected = object_path(&scratch.state_dir, digest).expect("obj path");
        assert!(expected.exists(), "object {} stored", digest);
    }
}

// ---------------------------------------------------------------------------
// Wave 2: production admission/recovery through the `agalma work` path
// ---------------------------------------------------------------------------

/// Repository instruction/shared-design bytes the production admission freezes.
const AGENTS_BYTES: &str = "# Agents\nRepository working rules (frozen at admission).\n";
const SHARED_DESIGN_BYTES: &str = "# Shared design revision 1\nFrozen shared-design bytes.\n";
/// Acceptance that the scripted build satisfies (it writes `42` into `src/lib.rs`).
const GREEN: &str = "  - sh -c 'grep -q 42 src/lib.rs'";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

/// Isolated XDG config base for a scratch fixture.
fn config_home(state_dir: &Path) -> PathBuf {
    state_dir.parent().expect("state parent").join("xdg-config")
}

/// A scratch origin (git + isolated bd) with the repository instruction and
/// shared-design documents the production admission freezes.
struct Prod {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Prod {
    fn new(name: &str) -> Prod {
        require_bd();
        let dir = fresh(&format!("sf-prod-{name}"));
        let origin = dir.join("origin");
        fs::create_dir_all(origin.join("src")).expect("origin src");
        fs::create_dir_all(origin.join("docs")).expect("origin docs");
        fs::write(
            origin.join("src").join("lib.rs"),
            "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
        )
        .expect("seed source");
        fs::write(origin.join("AGENTS.md"), AGENTS_BYTES).expect("agents");
        fs::write(
            origin.join("docs").join("self-mutation-contracts.md"),
            SHARED_DESIGN_BYTES,
        )
        .expect("shared design");

        run_ok("git", &origin, &["init", "-q", "-b", "main"]);
        run_ok(
            "git",
            &origin,
            &["config", "user.email", "agalma@localhost"],
        );
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

        Prod {
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

    /// Create a task with a fenced directive block plus design/notes/acceptance.
    fn create_task(&self, title: &str) -> String {
        let description = format!(
            "Task body with a fenced block.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n{GREEN}\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n"
        );
        let id = self.create(title, &description);
        self.update(
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
        self.protect(&id);
        id
    }

    /// Write/merge the operator `ProtectedInputs/v1` entry for `task_id` into
    /// the isolated XDG config (SF.6). Admission requires it.
    fn protect(&self, task_id: &str) {
        let root = fs::canonicalize(&self.origin).expect("canonical origin");
        let marker = "ok\n";
        let entry = json!({
            "task_id": task_id,
            "repository_root": root.to_string_lossy(),
            "base_sha": self.head(),
            "allowed_paths": ["src", "docs", "AGENTS.md"],
            "protected_paths": ["tests"],
            "baseline": {
                "format": "ProtectedBaseline/v1",
                "files": [{
                    "path": "protected/marker.txt",
                    "mode": "100644",
                    "digest": sha256_digest(marker.as_bytes()),
                    "bytes": marker.len() as u64,
                    "text": marker,
                }],
                "commands": ["sh -c 'true'"],
            },
        });
        let path = config_home(&self.state_dir)
            .join("agalma")
            .join("protected-inputs.json");
        let mut entries: Vec<Value> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("entries").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        entries.retain(|e| e.get("task_id").and_then(Value::as_str) != Some(task_id));
        entries.push(entry);
        fs::create_dir_all(path.parent().expect("manifest parent")).expect("config dir");
        let doc = json!({ "format": "ProtectedInputs/v1", "entries": entries });
        fs::write(&path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write manifest");
    }

    fn comment(&self, id: &str, text: &str) {
        self.bd_ok(&["comment", id, text]);
    }

    /// Overwrite a working-tree file under the origin (not committed).
    fn write_file(&self, rel: &str, text: &str) {
        let path = self.origin.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&path, text).expect("write file");
    }

    /// Create a directory under the origin (used for non-file source fixtures).
    fn mkdir(&self, rel: &str) {
        fs::create_dir_all(self.origin.join(rel)).expect("create dir");
    }

    fn add_dep(&self, id: &str, dep: &str) {
        self.bd_ok(&["dep", "add", id, dep]);
    }

    fn show(&self, id: &str) -> Value {
        let out = self.bd(&["show", id, "--json", "--include-comments"]);
        assert!(out.status.success(), "bd show: {}", stderr_of(&out));
        let value: Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("bd show json");
        value
            .as_array()
            .expect("show array")
            .first()
            .expect("one issue")
            .clone()
    }

    fn head(&self) -> String {
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

    /// Run `agalma work --once` against the scratch origin in scripted mode.
    ///
    /// In-process transports keep the focused test self-contained (no supervised
    /// server binaries) and free of concurrent-build races; admission/recovery
    /// behavior is independent of the transport.
    fn work(&self, extra: &[(&str, &str)]) -> Output {
        let mut command = Command::new(bin());
        command.args([
            "work",
            "--repo",
            self.origin.to_str().expect("origin utf8"),
            "--state-dir",
            self.state_dir.to_str().expect("state utf8"),
            "--once",
        ]);
        command
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .env("AGALMA_LEDGER_TRANSPORT", "in-process")
            .env("AGALMA_WORKSPACE_TRANSPORT", "in-process")
            .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
            .env("XDG_CONFIG_HOME", config_home(&self.state_dir))
            .env("GIT_TERMINAL_PROMPT", "0");
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("run agalma work")
    }

    /// The `task.package_admitted` payload for `execution`, if recorded.
    fn admitted(&self, execution: &str) -> Option<Value> {
        let conn =
            rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger");
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM execution_events \
                 WHERE execution_id=?1 AND kind='task.package_admitted' \
                 ORDER BY rowid DESC LIMIT 1",
                [execution],
                |row| row.get(0),
            )
            .optional()
            .expect("query admission event");
        payload.map(|text| serde_json::from_str(&text).expect("admission payload json"))
    }

    /// `(phase, state)` from the durable execution row.
    fn execution_phase(&self, execution: &str) -> (String, String) {
        let conn =
            rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger");
        conn.query_row(
            "SELECT phase, state FROM executions WHERE execution_id=?1",
            [execution],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("execution row")
    }

    /// Unconsumed dispatch intents for `execution`.
    fn pending_intents(&self, execution: &str) -> Vec<String> {
        let conn =
            rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger");
        let mut stmt = conn
            .prepare(
                "SELECT operation_id FROM dispatch_intents WHERE execution_id=?1 AND consumed=0",
            )
            .expect("prepare intents");
        let rows = stmt
            .query_map([execution], |row| row.get::<_, String>(0))
            .expect("query intents");
        rows.map(|row| row.expect("intent row")).collect()
    }

    /// Whether the execution row exists at all.
    fn has_execution(&self, execution: &str) -> bool {
        let conn =
            rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger");
        conn.query_row(
            "SELECT 1 FROM executions WHERE execution_id=?1",
            [execution],
            |_| Ok(()),
        )
        .optional()
        .expect("query execution")
        .is_some()
    }
}

fn assert_aborted(status: &std::process::ExitStatus) {
    assert!(!status.success(), "process should abort: {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT: {status:?}");
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Production admission freezes the full definition and its context; the frozen
/// bytes survive source edits and deletion, and the shared-design/dependency
/// refs stay readable.
#[test]
fn production_admission_freezes_definition_and_context() {
    let prod = Prod::new("admit");
    let prerequisite = prod.create("Prerequisite", "Prerequisite body.");
    prod.comment(
        &prerequisite,
        "Handoff: prerequisite landed; reuse the canonical digest helpers.",
    );
    let id = prod.create_task("Immutable context task");
    prod.add_dep(&id, &prerequisite);
    // A closed blocking dependency makes the dependent task ready to claim.
    prod.bd_ok(&["close", &prerequisite, "--reason", "prerequisite done"]);

    let out = prod.work(&[]);
    assert!(
        out.status.success(),
        "green production run: {}\n{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    let execution = format!("exec:{id}:1");
    let event = prod
        .admitted(&execution)
        .expect("admission reference recorded");
    let digest = event["package_digest"]
        .as_str()
        .expect("package digest")
        .to_string();

    let frozen = load_package(&prod.state_dir, &digest).expect("load frozen package");
    assert_eq!(frozen.definition.title, "Immutable context task");
    assert_eq!(frozen.definition.design.text, "Original design.\n");
    assert_eq!(frozen.definition.notes.text, "Original notes.\n");
    assert_eq!(
        frozen.definition.acceptance,
        vec!["sh -c 'grep -q 42 src/lib.rs'".to_string()]
    );
    assert_eq!(frozen.execution_id, execution);
    assert_eq!(frozen.roles.len(), 2);

    // The repository shared design and the dependency handoff are frozen and
    // readable from the store.
    let shared = frozen
        .context
        .iter()
        .find(|item| item.kind == ContextKind::SharedDesign)
        .expect("shared design frozen");
    let handoff = frozen
        .context
        .iter()
        .find(|item| item.kind == ContextKind::DependencyHandoff)
        .expect("dependency handoff frozen");
    assert_eq!(
        read_artifact(&prod.state_dir, &shared.artifact).expect("read shared design"),
        SHARED_DESIGN_BYTES.as_bytes()
    );
    let handoff_bytes = read_artifact(&prod.state_dir, &handoff.artifact).expect("read handoff");
    assert!(
        String::from_utf8_lossy(&handoff_bytes)
            .contains("Handoff: prerequisite landed; reuse the canonical digest helpers."),
        "dependency handoff captured: {}",
        String::from_utf8_lossy(&handoff_bytes)
    );

    // The source issue is edited and then removed: recovery still reads the
    // original bytes and the referenced context objects.
    prod.update(
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
    prod.bd_ok(&["delete", &id, "--force"]);
    assert!(
        prod.queue()
            .task_definition(&TaskId::new(id.as_str()))
            .is_err(),
        "source issue is gone"
    );

    let reloaded = load_package(&prod.state_dir, &digest).expect("reload frozen package");
    assert_eq!(reloaded.definition.title, "Immutable context task");
    assert_eq!(reloaded.definition.design.text, "Original design.\n");
    assert_eq!(
        read_artifact(&prod.state_dir, &shared.artifact).expect("re-read shared design"),
        SHARED_DESIGN_BYTES.as_bytes()
    );
}

/// A crash after the package objects are durable but before the reference commit
/// leaves no admission reference: recovery parks the execution and dispatches
/// nothing.
#[test]
fn crash_before_admission_parks_without_dispatch() {
    let prod = Prod::new("before");
    let id = prod.create_task("Pre-admission crash");
    let before = prod.head();

    let out = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "before_admit")]);
    assert_aborted(&out.status);

    let execution = format!("exec:{id}:1");
    // The durable pre-admission state is a genuinely non-dispatchable Pending
    // preparation: an execution row exists, but no dispatch intent has been
    // announced and no admission reference exists. Recovery parks it.
    assert!(
        prod.has_execution(&execution),
        "a preparation row exists before admission"
    );
    assert_eq!(
        prod.execution_phase(&execution),
        ("intake".to_string(), "pending".to_string()),
        "preparation is Pending, not dispatchable"
    );
    assert!(
        prod.pending_intents(&execution).is_empty(),
        "no dispatch intent precedes package admission"
    );
    assert!(
        prod.admitted(&execution).is_none(),
        "no admission reference without activation"
    );

    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "recovery run: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!((phase.as_str(), state.as_str()), ("parked", "parked"));
    assert_eq!(
        prod.head(),
        before,
        "no integration without durable admission inputs"
    );

    let issue = prod.show(&id);
    let labels: Vec<&str> = issue["labels"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(labels.contains(&"parked"), "issue parked: {issue}");
}

/// A crash after the reference commit recovers from the frozen package and
/// completes the execution even when the source issue is removed.
#[test]
fn crash_after_admission_recovers_from_frozen_package() {
    let prod = Prod::new("after");
    let id = prod.create_task("Post-admission crash");
    let before = prod.head();

    let out = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&out.status);

    let execution = format!("exec:{id}:1");
    let event = prod
        .admitted(&execution)
        .expect("admission committed before the crash");
    let digest = event["package_digest"]
        .as_str()
        .expect("package digest")
        .to_string();

    // Remove the source issue: recovery must not refresh from current bd.
    prod.bd_ok(&["delete", &id, "--force"]);

    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "recovery from frozen package: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!((phase.as_str(), state.as_str()), ("done", "completed"));
    assert_ne!(
        prod.head(),
        before,
        "recovered execution integrated the candidate"
    );

    let frozen = load_package(&prod.state_dir, &digest).expect("frozen package intact");
    assert_eq!(frozen.definition.title, "Post-admission crash");
}

/// A corrupt frozen package parks the recovered execution with no effects.
#[test]
fn corrupt_frozen_package_parks_without_effects() {
    let prod = Prod::new("corrupt");
    let id = prod.create_task("Corrupt package crash");
    let before = prod.head();

    let out = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&out.status);

    let execution = format!("exec:{id}:1");
    let event = prod.admitted(&execution).expect("admission committed");
    let path = prod
        .state_dir
        .join(event["package_path"].as_str().expect("package path"));

    // Overwrite the stored document with a wrong-but-well-formed digest.
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("json");
    value["package_digest"] = Value::String(format!("sha256:{}", "0".repeat(64)));
    fs::write(&path, serde_json::to_string(&value).expect("encode")).expect("write");

    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "recovery tolerates the park: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!((phase.as_str(), state.as_str()), ("parked", "parked"));
    assert_eq!(prod.head(), before, "corrupt package produces no merge");
}

// ---------------------------------------------------------------------------
// Wave 2 failure evidence (Sol review findings 2, 3, 4)
// ---------------------------------------------------------------------------

/// A self-consistent rewrite placed under the requested store path is still
/// refused: the loaded document must match the requested digest, not merely its
/// own.
#[test]
fn self_consistent_wrong_document_under_requested_path_is_refused() {
    let scratch = Scratch::new("wrongdoc");
    let (id, _) = seed_issue(&scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let built =
        build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");
    let persisted = persist_package(&scratch.state_dir, &built).expect("persist");

    let mut other = built.package.clone();
    other.definition.title = "Self-consistent rewrite".to_string();
    let other = other.seal().expect("seal");
    other.verify().expect("rewrite is internally consistent");
    assert_ne!(other.package_digest, persisted.digest);
    let document =
        agalma_contracts::digest::canonical_json(&serde_json::to_value(&other).expect("encode"))
            .expect("canonical");
    fs::write(
        package_path(&scratch.state_dir, &persisted.digest).expect("path"),
        document.as_bytes(),
    )
    .expect("write rewrite");

    let err =
        load_package(&scratch.state_dir, &persisted.digest).expect_err("wrong document refused");
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));
}

fn built_and_persisted(scratch: &Scratch) -> agalma_conductor::task_package::BuiltPackage {
    let (id, _) = seed_issue(scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let built =
        build_package(&definition, inputs(scratch, &definition, b"d", b"h")).expect("build");
    persist_package(&scratch.state_dir, &built).expect("persist");
    built
}

/// A missing referenced object parks before any recovered dispatch.
#[test]
fn missing_referenced_object_parks() {
    let scratch = Scratch::new("missing-obj");
    let built = built_and_persisted(&scratch);
    let shared = built
        .package
        .context
        .iter()
        .find(|item| item.kind == ContextKind::SharedDesign)
        .expect("shared design context")
        .artifact
        .clone();
    fs::remove_file(object_path(&scratch.state_dir, &shared.digest).expect("obj path"))
        .expect("remove object");

    let err =
        verify_references(&scratch.state_dir, &built.package).expect_err("missing object parks");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
}

/// A corrupt referenced object (bytes do not match its digest) parks.
#[test]
fn corrupt_referenced_object_parks() {
    let scratch = Scratch::new("corrupt-obj");
    let built = built_and_persisted(&scratch);
    let shared = built
        .package
        .context
        .iter()
        .find(|item| item.kind == ContextKind::SharedDesign)
        .expect("shared design context")
        .artifact
        .clone();
    fs::write(
        object_path(&scratch.state_dir, &shared.digest).expect("obj path"),
        b"tampered bytes",
    )
    .expect("tamper object");

    let err =
        verify_references(&scratch.state_dir, &built.package).expect_err("corrupt object parks");
    assert_eq!(failure_code(&err), Some(FailureCode::IntegrityMismatch));
}

/// A missing role-prompt object parks (prompts are materialized, not just
/// referenced).
#[test]
fn missing_prompt_object_parks() {
    let scratch = Scratch::new("missing-prompt");
    let built = built_and_persisted(&scratch);
    let builder = built
        .package
        .roles
        .iter()
        .find(|pin| pin.role == Role::Builder)
        .expect("builder role pin")
        .prompt
        .clone();
    fs::remove_file(object_path(&scratch.state_dir, &builder.digest).expect("obj path"))
        .expect("remove prompt object");

    let err =
        verify_references(&scratch.state_dir, &built.package).expect_err("missing prompt parks");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
}

/// A declared parent design that cannot be captured refuses admission, even
/// though the fenced directive block is present (the block must not mask a
/// missing required source).
#[test]
fn admission_refuses_unreadable_declared_parent() {
    let scratch = Scratch::new("required-parent");
    let queue = scratch.queue();
    let mut definition = required_source_definition();
    definition.parent = Some("missing-epic".to_string());

    let err = gather_admission_context(&scratch.origin, &queue, &definition)
        .expect_err("missing parent refuses admission");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
    assert!(
        !scratch.state_dir.join("packages").exists(),
        "no package written when admission refuses"
    );
}

/// A declared dependency handoff that cannot be captured refuses admission.
#[test]
fn admission_refuses_unreadable_declared_dependency() {
    let scratch = Scratch::new("required-dep");
    let queue = scratch.queue();
    let mut definition = required_source_definition();
    definition.dependencies = vec![TaskDependency {
        id: "missing-dep".to_string(),
        kind: Some("blocks".to_string()),
    }];

    let err = gather_admission_context(&scratch.origin, &queue, &definition)
        .expect_err("missing dependency refuses admission");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
    assert!(!scratch.state_dir.join("packages").exists());
}

fn required_source_definition() -> TaskDefinition {
    TaskDefinition {
        task_id: TaskId::new("fixture.required"),
        title: "Required source".to_string(),
        description: "Body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\npriority: high\nfamily: feature\nmax_attempts: 2\n```\n".to_string(),
        design: Some("Design.\n".to_string()),
        notes: None,
        acceptance_criteria: None,
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    }
}

/// A changed harness selector refuses recovered dispatch rather than silently
/// substituting the current binding.
#[test]
fn recovery_refuses_on_changed_harness_selector() {
    let prod = Prod::new("harness-pin");
    let id = prod.create_task("Harness pin crash");
    let before = prod.head();

    let out = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&out.status);
    let execution = format!("exec:{id}:1");
    assert!(prod.admitted(&execution).is_some(), "admission committed");

    let recovery = prod.work(&[("AGALMA_HARNESS", "reference")]);
    assert!(
        recovery.status.success(),
        "recovery run: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!((phase.as_str(), state.as_str()), ("parked", "parked"));
    assert_eq!(
        prod.head(),
        before,
        "no dispatch when the frozen harness pin is incompatible"
    );
}

/// A mutated builder prompt file cannot substitute a different prompt: recovery
/// rewrites it from the frozen package bytes and completes.
#[test]
fn recovery_pins_frozen_builder_prompt() {
    let prod = Prod::new("prompt-pin");
    let id = prod.create_task("Prompt pin crash");

    let out = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&out.status);

    let prompt_path = prod.state_dir.join("prompts").join("builder.md");
    // Plant a mutated prompt file before recovery: the frozen bytes must
    // replace it, not the reverse.
    fs::create_dir_all(prompt_path.parent().expect("prompts parent")).expect("prompts dir");
    fs::write(&prompt_path, "MUTATED builder prompt").expect("plant mutated prompt");

    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "recovery run: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let execution = format!("exec:{id}:1");
    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!((phase.as_str(), state.as_str()), ("done", "completed"));
    assert_eq!(
        fs::read_to_string(&prompt_path).expect("prompt restored"),
        BUILDER_PROMPT,
        "frozen prompt bytes restored over the mutation"
    );
}

// ---------------------------------------------------------------------------
// Next-wave: bounded executor preparation/activation seam (finding 1)
// ---------------------------------------------------------------------------

fn admission_event(execution: &ExecutionId, digest: &str) -> ExecutionEvent {
    ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: "task.package_admitted".to_string(),
        payload: json!({ "package_digest": digest }),
        recorded_at_unix_ms: 0,
    }
}

fn activation_executor(dir: &Path) -> Executor<SqliteLedger> {
    Executor::new(
        SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger"),
        ExecutorConfig {
            max_attempts: 2,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    )
}

/// `prepare` commits a non-dispatchable Pending preparation with no intent;
/// `activate` atomically adds the Running transition, the admission event, and
/// one intent; duplicate activation adds no second intent and refuses changed
/// admission inputs.
#[test]
fn activation_is_atomic_duplicate_safe_and_rejects_changed_inputs() {
    let dir = ScratchDir::new("sf-activation");
    let mut executor = activation_executor(&dir);
    let prep = executor
        .prepare(&TaskId::new("activation"))
        .expect("prepare");

    let status = executor.status(&prep.execution_id).expect("status");
    assert_eq!(status.state, ExecutionState::Pending);
    assert!(
        status.pending_operations.is_empty(),
        "no dispatch intent before admission"
    );

    let admission = admission_event(&prep.execution_id, &format!("sha256:{}", "a".repeat(64)));
    let operation = executor
        .activate(&prep, admission.clone())
        .expect("activate");
    assert_eq!(operation, OperationId::derive(&prep.execution_id, "intake"));

    let status = executor.status(&prep.execution_id).expect("status");
    assert_eq!(status.state, ExecutionState::Running);
    assert_eq!(
        status.pending_operations,
        vec![operation.clone()],
        "activation announces exactly one intent"
    );

    // Duplicate activation: no second intent.
    let again = executor
        .activate(&prep, admission.clone())
        .expect("duplicate activation is idempotent");
    assert_eq!(again, operation);
    assert_eq!(
        executor
            .status(&prep.execution_id)
            .expect("status")
            .pending_operations
            .len(),
        1,
        "duplicate activation announces no second intent"
    );

    // Changed admission inputs for the same event kind are refused.
    let changed = admission_event(&prep.execution_id, &format!("sha256:{}", "b".repeat(64)));
    let err = executor
        .activate(&prep, changed)
        .expect_err("changed admission inputs refused");
    assert!(
        matches!(err, ContractError::Conflict(_)),
        "changed admission refused with Conflict: {err:?}"
    );
}

/// A Pending preparation never dispatches or advances; recovery agrees with the
/// Pending projection.
#[test]
fn pending_preparation_never_dispatches() {
    let dir = ScratchDir::new("sf-pending");
    let mut executor = activation_executor(&dir);
    let prep = executor.prepare(&TaskId::new("pending")).expect("prepare");

    executor
        .recover()
        .expect("recovery agrees with the Pending projection");

    let step = executor.step(&prep.execution_id).expect("step");
    assert!(
        matches!(step, StepOutcome::Parked { .. }),
        "Pending preparation does not dispatch: {step:?}"
    );
    assert!(
        executor
            .status(&prep.execution_id)
            .expect("status")
            .pending_operations
            .is_empty(),
        "no intent was announced by step"
    );
}

// ---------------------------------------------------------------------------
// Next-wave: real role pins and required referenced instructions
// ---------------------------------------------------------------------------

/// Role pins carry the real prepared generation and full binding fields, not a
/// synthetic generation-1 placeholder.
#[test]
fn non_first_generation_role_pins_use_real_generation() {
    let prod = Prod::new("gen-pin");
    let id = prod.create_task("Generation pin");

    let first = prod.work(&[]);
    assert!(first.status.success(), "first run: {}", stderr_of(&first));
    prod.bd_ok(&["reopen", &id]);
    // A re-run is a NEW generation whose resolved base is the post-merge origin
    // SHA. Fail-closed base matching is preserved: the operator supplies a fresh
    // entry matched to the new origin SHA before the next admission.
    prod.protect(&id);
    let second = prod.work(&[]);
    assert!(
        second.status.success(),
        "second run: {}",
        stderr_of(&second)
    );

    let execution = format!("exec:{id}:2");
    let event = prod.admitted(&execution).expect("generation-2 admission");
    let digest = event["package_digest"]
        .as_str()
        .expect("digest")
        .to_string();
    let frozen = load_package(&prod.state_dir, &digest).expect("load generation-2 package");
    assert_eq!(frozen.generation, 2);

    for pin in &frozen.roles {
        assert_eq!(
            pin.harness.generation, 2,
            "role pin uses the real prepared generation: {:?}",
            pin.role
        );
        assert!(
            pin.harness.binding_id.ends_with(":2"),
            "binding id carries the generation: {}",
            pin.harness.binding_id
        );
        assert_eq!(pin.harness.api, HARNESS_API);
        assert_eq!(pin.harness.api_version, u64::from(API_VERSION));
        assert!(
            !pin.harness.implementation_version.is_empty(),
            "implementation version recorded"
        );
    }
}

/// Every required pin field is validated before recovered effects; an
/// incompatible API, API version, generation, implementation version, or config
/// digest is refused.
#[test]
fn recovered_pins_validate_every_required_field() {
    let scratch = Scratch::new("pin-fields");
    let (id, _) = seed_issue(&scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let kind = HarnessKind::OpenCode;
    let mut inputs = inputs(&scratch, &definition, b"d", b"h");
    inputs.roles = vec![
        RoleInput::new(
            Role::Builder,
            "builder:fix",
            "fixture-model",
            real_binding_pin(kind, 3, "fixture-model").expect("builder pin"),
            b"builder prompt".to_vec(),
            "text/plain",
        ),
        RoleInput::new(
            Role::Verifier,
            "verifier:fix",
            "fixture-model",
            real_binding_pin(kind, 3, "fixture-model").expect("verifier pin"),
            VERIFIER_PROMPT.as_bytes().to_vec(),
            "text/plain",
        ),
    ];
    let built = build_package(&definition, inputs).expect("build");
    persist_package(&scratch.state_dir, &built).expect("persist");
    let harness = harness_handle(HarnessChoice::OpenCode);

    verify_recovered_pins(&scratch.state_dir, &built.package, &harness)
        .expect("real pins validate");

    let mut tampered = built.package.clone();
    tampered.roles[0].harness.api = "NotHarnessApi".to_string();
    assert!(
        verify_recovered_pins(&scratch.state_dir, &tampered, &harness).is_err(),
        "api"
    );

    let mut tampered = built.package.clone();
    tampered.roles[0].harness.api_version = 99;
    assert!(
        verify_recovered_pins(&scratch.state_dir, &tampered, &harness).is_err(),
        "api_version"
    );

    let mut tampered = built.package.clone();
    tampered.roles[0].harness.generation = 99;
    assert!(
        verify_recovered_pins(&scratch.state_dir, &tampered, &harness).is_err(),
        "generation"
    );

    let mut tampered = built.package.clone();
    tampered.roles[0].harness.implementation_version = "tampered".to_string();
    assert!(
        verify_recovered_pins(&scratch.state_dir, &tampered, &harness).is_err(),
        "implementation_version"
    );

    let mut tampered = built.package.clone();
    tampered.roles[0].harness.config_digest = format!("sha256:{}", "0".repeat(64));
    assert!(
        verify_recovered_pins(&scratch.state_dir, &tampered, &harness).is_err(),
        "config_digest"
    );
}

/// A missing instruction document referenced by AGENTS.md refuses admission
/// rather than admitting a package without its governing instructions.
#[test]
fn admission_refuses_missing_referenced_instruction() {
    let scratch = Scratch::new("ref-missing");
    fs::write(
        scratch.origin.join("AGENTS.md"),
        "# Agents\nSee docs/governance-missing.md for the governing rules.\n",
    )
    .expect("write agents");
    let queue = scratch.queue();
    let err = gather_admission_context(&scratch.origin, &queue, &required_source_definition())
        .expect_err("missing referenced instruction refuses admission");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
}

/// A non-file (metadata) failure for a referenced instruction refuses admission.
#[test]
fn admission_refuses_nonfile_referenced_instruction() {
    let scratch = Scratch::new("ref-nonfile");
    fs::write(
        scratch.origin.join("AGENTS.md"),
        "# Agents\nSee docs/governance.md for the governing rules.\n",
    )
    .expect("write agents");
    fs::create_dir_all(scratch.origin.join("docs").join("governance.md")).expect("dir as file");
    let queue = scratch.queue();
    let err = gather_admission_context(&scratch.origin, &queue, &required_source_definition())
        .expect_err("non-file referenced instruction refuses admission");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
}

/// Production path: a missing referenced instruction refuses admission with no
/// handler effects and no dispatch intent.
#[test]
fn production_refuses_missing_referenced_instruction_without_effects() {
    let prod = Prod::new("prod-ref-missing");
    prod.write_file(
        "AGENTS.md",
        "# Agents\nSee docs/governance-missing.md for the governing rules.\n",
    );
    let id = prod.create_task("Missing referenced instruction");
    let before = prod.head();

    let out = prod.work(&[]);
    assert!(
        !out.status.success(),
        "admission must refuse a missing required instruction: {}",
        stdout_of(&out)
    );

    let execution = format!("exec:{id}:1");
    assert!(
        prod.admitted(&execution).is_none(),
        "no admission on refusal"
    );
    assert!(
        prod.pending_intents(&execution).is_empty(),
        "no dispatch intent on refusal"
    );
    assert_eq!(
        prod.execution_phase(&execution),
        ("intake".to_string(), "pending".to_string()),
        "only a non-dispatchable preparation exists"
    );
    assert_eq!(prod.head(), before, "no handler effects on refusal");
}

/// Production path: a non-file referenced instruction refuses admission with no
/// handler effects.
#[test]
fn production_refuses_nonfile_referenced_instruction_without_effects() {
    let prod = Prod::new("prod-ref-nonfile");
    prod.write_file(
        "AGENTS.md",
        "# Agents\nSee docs/governance.md for the governing rules.\n",
    );
    prod.mkdir("docs/governance.md");
    let id = prod.create_task("Non-file referenced instruction");
    let before = prod.head();

    let out = prod.work(&[]);
    assert!(
        !out.status.success(),
        "admission must refuse a non-file required instruction: {}",
        stdout_of(&out)
    );
    let execution = format!("exec:{id}:1");
    assert!(
        prod.admitted(&execution).is_none(),
        "no admission on refusal"
    );
    assert!(
        prod.pending_intents(&execution).is_empty(),
        "no dispatch intent"
    );
    assert_eq!(prod.head(), before, "no handler effects on refusal");
}

/// Production recovery refuses dispatch when a referenced immutable object is
/// missing: the execution parks with no merge.
#[test]
fn production_recovery_refuses_missing_referenced_object() {
    let prod = Prod::new("recov-obj");
    let id = prod.create_task("Recovery object refusal");
    let before = prod.head();

    let out = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&out.status);

    let execution = format!("exec:{id}:1");
    let event = prod.admitted(&execution).expect("admission committed");
    let digest = event["package_digest"]
        .as_str()
        .expect("digest")
        .to_string();
    let frozen = load_package(&prod.state_dir, &digest).expect("load frozen package");
    let shared = frozen
        .context
        .iter()
        .find(|item| item.kind == ContextKind::SharedDesign)
        .expect("shared design context")
        .artifact
        .clone();
    fs::remove_file(object_path(&prod.state_dir, &shared.digest).expect("obj path"))
        .expect("remove referenced object");

    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "recovery run: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!((phase.as_str(), state.as_str()), ("parked", "parked"));
    assert_eq!(
        prod.head(),
        before,
        "missing referenced object produces no merge"
    );
}

// ---------------------------------------------------------------------------
// Next repair: activate replay/lease refusal (Sol review)
// ---------------------------------------------------------------------------

/// A durable snapshot used to prove a refused activation commits nothing.
fn activation_snapshot(
    executor: &Executor<SqliteLedger>,
    execution: &ExecutionId,
) -> (ExecutionState, usize, usize) {
    let status = executor.status(execution).expect("status");
    let events = executor.ledger().events(execution).expect("events").len();
    (status.state, status.pending_operations.len(), events)
}

/// A changed event kind or a preparation with no recorded activation/admission
/// evidence is refused without committing a new intent, event, or transition.
#[test]
fn activation_rejects_changed_kind_and_missing_evidence() {
    // Changed event kind after a successful activation.
    let dir = ScratchDir::new("sf-activate-kind");
    let mut executor = activation_executor(&dir);
    let prep = executor.prepare(&TaskId::new("kind")).expect("prepare");
    let admission = admission_event(&prep.execution_id, &format!("sha256:{}", "a".repeat(64)));
    executor
        .activate(&prep, admission.clone())
        .expect("activate");

    let before = activation_snapshot(&executor, &prep.execution_id);
    let changed_kind = ExecutionEvent {
        kind: "other.event".to_string(),
        ..admission.clone()
    };
    let err = executor
        .activate(&prep, changed_kind)
        .expect_err("changed event kind refused");
    assert!(matches!(err, ContractError::Conflict(_)), "{err:?}");
    assert_eq!(
        activation_snapshot(&executor, &prep.execution_id),
        before,
        "changed-kind refusal commits nothing"
    );

    // Replaying an existing non-admission event (the preparation event) with its
    // exact payload must also refuse: replay matches the admission identity
    // recorded at activation, not any existing event.
    let prepared_event = executor
        .ledger()
        .events(&prep.execution_id)
        .expect("events")
        .into_iter()
        .find(|event| event.kind == "execution_prepared")
        .expect("prepared event");
    let err = executor
        .activate(&prep, prepared_event)
        .expect_err("replaying the preparation event refused");
    assert!(matches!(err, ContractError::Conflict(_)), "{err:?}");
    assert_eq!(
        activation_snapshot(&executor, &prep.execution_id),
        before,
        "existing-event replay commits nothing"
    );

    // A parked preparation that was never activated/admitted is refused.
    let dir2 = ScratchDir::new("sf-activate-parked");
    let mut executor2 = activation_executor(&dir2);
    let prep2 = executor2.prepare(&TaskId::new("parked")).expect("prepare");
    {
        let record = executor2
            .ledger()
            .execution(&prep2.execution_id)
            .expect("row")
            .expect("prepared row");
        let parked = ExecutionRecord {
            phase: ExecutionPhase::Parked,
            state: ExecutionState::Parked,
            ..record
        };
        executor2
            .ledger_mut()
            .commit(CommitBatch {
                expected_revision: Some(parked.revision),
                state: Some(parked),
                events: vec![],
                receipts: vec![],
                intents: vec![],
            })
            .expect("park preparation");
    }
    let before2 = activation_snapshot(&executor2, &prep2.execution_id);
    let err = executor2
        .activate(
            &prep2,
            admission_event(&prep2.execution_id, &format!("sha256:{}", "c".repeat(64))),
        )
        .expect_err("parked preparation refused");
    assert!(matches!(err, ContractError::Conflict(_)), "{err:?}");
    assert_eq!(
        activation_snapshot(&executor2, &prep2.execution_id),
        before2,
        "missing-evidence refusal commits nothing"
    );
}

/// A forged preparation lease or a lease superseded by another preparation is
/// refused before any commit.
#[test]
fn activation_rejects_forged_and_superseded_leases() {
    // Forged: the supplied preparation lease is not the current task lease.
    let dir = ScratchDir::new("sf-forged-lease");
    let mut executor = activation_executor(&dir);
    let prep = executor.prepare(&TaskId::new("forged")).expect("prepare");
    let forged = Preparation {
        execution_id: prep.execution_id.clone(),
        generation: prep.generation,
        lease_generation: prep.lease_generation + 7,
    };
    let before = activation_snapshot(&executor, &prep.execution_id);
    let err = executor
        .activate(
            &forged,
            admission_event(&prep.execution_id, &format!("sha256:{}", "d".repeat(64))),
        )
        .expect_err("forged lease refused");
    assert_eq!(failure_code(&err), Some(FailureCode::StaleGeneration));
    assert_eq!(
        activation_snapshot(&executor, &prep.execution_id),
        before,
        "forged-lease refusal commits nothing"
    );

    // Superseded: a second preparation advances the task lease, so the first
    // preparation can no longer activate.
    let dir2 = ScratchDir::new("sf-superseded-lease");
    let mut executor2 = activation_executor(&dir2);
    let task = TaskId::new("superseded");
    let prep1 = executor2.prepare(&task).expect("prepare 1");
    let _prep2 = executor2.prepare(&task).expect("prepare 2");
    let before2 = activation_snapshot(&executor2, &prep1.execution_id);
    let err = executor2
        .activate(
            &prep1,
            admission_event(&prep1.execution_id, &format!("sha256:{}", "e".repeat(64))),
        )
        .expect_err("superseded lease refused");
    assert_eq!(failure_code(&err), Some(FailureCode::StaleGeneration));
    assert_eq!(
        activation_snapshot(&executor2, &prep1.execution_id),
        before2,
        "superseded-lease refusal commits nothing"
    );

    // Coherent forgery: a preparation whose recorded lease was 1 is forged to the
    // advanced task lease (2) after another preparation advances the task lease.
    // The historical recorded-lease check must still refuse it.
    let dir3 = ScratchDir::new("sf-coherent-forge");
    let mut executor3 = activation_executor(&dir3);
    let task3 = TaskId::new("coherent");
    let prep3 = executor3.prepare(&task3).expect("prepare 1");
    let _advance = executor3.prepare(&task3).expect("prepare 2");
    let forged = Preparation {
        execution_id: prep3.execution_id.clone(),
        generation: prep3.generation,
        lease_generation: prep3.lease_generation + 1,
    };
    assert_eq!(
        executor3
            .ledger()
            .lease_generation(&task3)
            .expect("task lease"),
        forged.lease_generation,
        "the forged lease equals the current task lease"
    );
    let before3 = activation_snapshot(&executor3, &prep3.execution_id);
    let err = executor3
        .activate(
            &forged,
            admission_event(&prep3.execution_id, &format!("sha256:{}", "f".repeat(64))),
        )
        .expect_err("coherent forgery refused");
    assert_eq!(failure_code(&err), Some(FailureCode::StaleGeneration));
    assert_eq!(
        activation_snapshot(&executor3, &prep3.execution_id),
        before3,
        "coherent-forgery refusal commits nothing"
    );
}

// ---------------------------------------------------------------------------
// Review/commit pass: held-out oracle scope (SF.2 final correction)
// ---------------------------------------------------------------------------

/// The held-out oracle digest is never auto-granted as a builder-readable
/// artifact, the oracle object stays durable and reachable via its explicit
/// reference, and a caller-supplied grant of the same oracle is refused.
#[test]
fn oracle_is_not_builder_readable_and_grant_is_refused() {
    let scratch = Scratch::new("oracle-scope");
    let (id, _) = seed_issue(&scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let built =
        build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");
    let oracle_digest = built.package.scope.oracle.digest.clone();

    // Exclusion: the oracle digest is not a builder-readable artifact.
    assert!(
        !built
            .package
            .scope
            .readable_artifacts
            .iter()
            .any(|digest| digest == &oracle_digest),
        "oracle digest must not be builder-readable: {:?}",
        built.package.scope.readable_artifacts
    );

    // Durable object + explicit reference: the oracle stays materialized and
    // readable only through `scope.oracle`.
    persist_package(&scratch.state_dir, &built).expect("persist");
    let oracle_path = object_path(&scratch.state_dir, &oracle_digest).expect("oracle path");
    assert!(
        oracle_path.exists(),
        "oracle object durable: {}",
        oracle_path.display()
    );
    assert_eq!(
        read_artifact(&scratch.state_dir, &built.package.scope.oracle).expect("read oracle"),
        b"oracle"
    );

    // Refusal: a caller-supplied grant of the same oracle is refused.
    let mut grant = inputs(&scratch, &definition, b"d", b"h");
    grant.readable_artifacts = vec![oracle_digest];
    let err = build_package(&definition, grant).expect_err("oracle grant refused");
    assert_eq!(failure_code(&err), Some(FailureCode::ScopeDenied));
}

/// A stored admitted package that grants its own held-out oracle is refused at
/// the load-admitted seam (before any recovered effect) and its frozen
/// bytes/digest are preserved (no silent rewrite).
#[test]
fn stored_package_granting_its_oracle_is_refused() {
    let scratch = Scratch::new("oracle-stored");
    let (id, _) = seed_issue(&scratch);
    let queue = scratch.queue();
    let definition = queue
        .task_definition(&TaskId::new(id.as_str()))
        .expect("definition");
    let built =
        build_package(&definition, inputs(&scratch, &definition, b"d", b"h")).expect("build");
    let oracle_digest = built.package.scope.oracle.digest.clone();

    // Forge a stored package that grants its own held-out oracle, sealed under
    // its own digest.
    let mut unsafe_built = built.clone();
    unsafe_built
        .package
        .scope
        .readable_artifacts
        .push(oracle_digest);
    unsafe_built.package.scope.readable_artifacts.sort();
    unsafe_built.package.scope.readable_artifacts.dedup();
    unsafe_built.package = unsafe_built.package.clone().seal().expect("seal");
    let unsafe_digest = unsafe_built.package.package_digest.clone();
    let persisted = persist_package(&scratch.state_dir, &unsafe_built).expect("persist unsafe");
    let path = scratch.state_dir.join(&persisted.path);
    let stored_bytes = fs::read(&path).expect("read stored");

    let err = load_admitted_package(&scratch.state_dir, &unsafe_digest)
        .expect_err("stored oracle grant refused");
    assert_eq!(failure_code(&err), Some(FailureCode::ScopeDenied));

    // Frozen bytes/digest preserved: the refusal never rewrites the package.
    assert_eq!(fs::read(&path).expect("re-read stored"), stored_bytes);
    assert_eq!(unsafe_digest, persisted.digest);
}
