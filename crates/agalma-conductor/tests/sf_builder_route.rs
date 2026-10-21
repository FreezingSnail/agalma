//! SF.8 acceptance: builder-only model selection (`agalma-5fy.8`).
//!
//! An explicit builder selector (`--builder-model`/`AGALMA_BUILDER_MODEL`) selects
//! the **builder** route only. It outranks the legacy global `--model` for the
//! builder, leaves planner/reviewer/verifier selection independently resolved, is
//! pinned into the immutable `TaskPackage/v1`, and is preserved across restart
//! even when the environment changes. A scoped selection naming a paid model
//! cannot bypass the credential/paid gate: it is selected as requested or
//! refused, never silently substituted.
//!
//! Deterministic: no network, no paid session. The routing half records real
//! `route.model` decisions in a scratch `SqliteLedger`; the production half runs
//! the real `agalma work` binary in scripted mode against a scratch git+bd origin.
//! Scratch dirs are retained for review evidence (`ScratchDir::keep`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agalma_conductor::routing::{self, RoleModelOverride, RouteClass, RouteOrigin};
use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FORMAT, PROTECTED_INPUTS_FILE,
};
use agalma_conductor::task_package::{
    build_package, load_admitted_package, load_package, persist_package, AuthorizationInput,
    ContextInput, PackageInputs, RoleInput,
};
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, ContextKind, DirectiveRef, LedgerApi, OperationId, Role,
    TaskFamily, TaskId,
};
use agalma_ledger::SqliteLedger;
use agalma_taskqueue::TaskDefinition;
use agalma_testkit::ScratchDir;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

/// Test scratch retained on disk for review evidence (`ScratchDir::keep`).
fn scratch(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(name);
    dir.keep();
    dir
}

/// The reviewed Flash route this child targets (must not be a fabricated id).
const FLASH: &str = "opencode-go/deepseek-v4.1-flash";
/// A reviewed free model used for the scoped builder selector in offline tests.
const FREE_A: &str = "opencode/mimo-v2.6-flash-free";
/// A different reviewed free model used for the legacy global selector.
const FREE_B: &str = "opencode/big-pickle";

// ---------------------------------------------------------------------------
// Routing: role-scoped selection, isolation, precedence, eligibility, provenance
// ---------------------------------------------------------------------------

#[test]
fn builder_selector_outranks_global_for_builder_only() {
    let dir = scratch("sf-builder-scope");
    let overrides = RoleModelOverride::new(Some(FREE_A), Some("custom/legacy-global"));

    let builder = routing::resolve_builder_route(
        &dir,
        "task-iso",
        TaskFamily::Feature,
        false,
        false,
        overrides,
    )
    .expect("builder route");
    assert_eq!(builder.class, RouteClass::Builder);
    assert_eq!(
        builder.model, FREE_A,
        "scoped selector wins for the builder"
    );
    assert_eq!(builder.origin, RouteOrigin::Override);

    // The verifier role keeps the legacy global selector, independently.
    let verifier = routing::resolve_route(
        &dir,
        "task-iso",
        "verifier",
        TaskFamily::Feature,
        false,
        false,
        overrides.effective_global(),
    )
    .expect("verifier route");
    assert_eq!(verifier.class, RouteClass::Verifier);
    assert_eq!(
        verifier.model, "custom/legacy-global",
        "verifier is not affected by the builder selector"
    );
    assert_eq!(verifier.origin, RouteOrigin::Override);

    // The planner role is untouched by the builder selector: without a global
    // override it resolves its own static paid route.
    let planner = routing::resolve_route(
        &dir,
        "task-iso",
        "planner",
        TaskFamily::Feature,
        true,
        true,
        None,
    )
    .expect("planner route");
    assert_eq!(planner.class, RouteClass::Planner);
    assert_eq!(planner.model, "opencode-go/deepseek-v4-pro");
    assert_ne!(
        builder.model, planner.model,
        "builder and planner routes stay independently resolved"
    );
}

#[test]
fn unset_builder_selector_preserves_legacy_selection() {
    // No scoped selector, no global override: the static free-first builder route.
    let dir = scratch("sf-builder-unset");
    let builder = routing::resolve_builder_route(
        &dir,
        "task-unset",
        TaskFamily::Fix,
        false,
        false,
        RoleModelOverride::default(),
    )
    .expect("static builder route");
    assert_eq!(builder.model, FREE_B);
    assert_eq!(builder.origin, RouteOrigin::Static);

    // No scoped selector, global override set: the legacy global selects the
    // builder unchanged (legacy compatibility).
    let dir2 = scratch("sf-builder-global");
    let legacy = routing::resolve_builder_route(
        &dir2,
        "task-legacy",
        TaskFamily::Fix,
        false,
        false,
        RoleModelOverride::new(None, Some(FLASH)),
    )
    .expect("legacy global builder route");
    assert_eq!(legacy.model, FLASH);
    assert_eq!(legacy.origin, RouteOrigin::Override);
}

#[test]
fn eligible_paid_builder_selection_selects_the_reviewed_flash_route() {
    let dir = scratch("sf-builder-flash");
    let builder = routing::resolve_builder_route(
        &dir,
        "task-flash",
        TaskFamily::Feature,
        true,
        true,
        RoleModelOverride::new(Some(FLASH), None),
    )
    .expect("eligible paid builder selection");
    assert_eq!(builder.model, FLASH);
    assert_eq!(builder.origin, RouteOrigin::Override);
    assert!(builder.is_paid());
    assert!(
        builder.spec.is_some(),
        "the reviewed Flash id is a real catalog row"
    );
}

#[test]
fn explicit_paid_builder_selection_without_credential_is_denied() {
    let dir = scratch("sf-builder-paid-denied");

    // Opt-in but no provider credential: refused, never silently downgraded.
    let err = routing::resolve_builder_route(
        &dir,
        "task-deny",
        TaskFamily::Feature,
        true,
        false,
        RoleModelOverride::new(Some(FLASH), None),
    )
    .expect_err("paid builder selection without a credential is denied");
    let msg = err.to_string();
    assert!(msg.contains(FLASH), "error names the model: {msg}");
    assert!(
        msg.contains("credential"),
        "error names the missing credential: {msg}"
    );

    // Without the paid opt-in the selection is also refused.
    let err = routing::resolve_builder_route(
        &dir,
        "task-deny2",
        TaskFamily::Feature,
        false,
        true,
        RoleModelOverride::new(Some(FLASH), None),
    )
    .expect_err("paid builder selection without opt-in is denied");
    assert!(
        err.to_string().contains("AGALMA_ALLOW_PAID"),
        "error names the disabled opt-in: {err}"
    );

    // A free scoped selection is never gated.
    let free = routing::resolve_builder_route(
        &dir,
        "task-free",
        TaskFamily::Feature,
        false,
        false,
        RoleModelOverride::new(Some(FREE_A), None),
    )
    .expect("free scoped selection is allowed");
    assert_eq!(free.model, FREE_A);
}

#[test]
fn route_decisions_record_role_and_origin_provenance() {
    let dir = scratch("sf-builder-provenance");
    let overrides = RoleModelOverride::new(Some(FREE_A), None);
    let builder = routing::resolve_builder_route(
        &dir,
        "task-prov",
        TaskFamily::Feature,
        false,
        false,
        overrides,
    )
    .expect("builder route");
    let verifier = routing::resolve_route(
        &dir,
        "task-prov",
        "verifier",
        TaskFamily::Feature,
        false,
        false,
        None,
    )
    .expect("verifier route");
    assert_eq!(builder.model, FREE_A);
    assert_eq!(verifier.model, FREE_B);

    // The builder selection is recorded as a `route.model` decision whose
    // context names the builder role and the override origin.
    let conn = rusqlite::Connection::open(dir.join("ledger.sqlite")).expect("open ledger");
    let builder_request: String = conn
        .query_row(
            "SELECT request_json FROM decisions \
             WHERE operation_id LIKE 'op:route:task-prov:builder:%'",
            [],
            |row| row.get(0),
        )
        .expect("builder route decision recorded");
    let builder_request: Value = serde_json::from_str(&builder_request).expect("builder json");
    assert_eq!(builder_request["context"]["role"], "builder");
    assert_eq!(builder_request["context"]["origin"], "override");
    assert_eq!(builder_request["context"]["class"], "builder");
    assert_eq!(builder_request["options"][0]["option_id"], FREE_A);

    // The verifier decision is a separate role with the static origin.
    let verifier_request: String = conn
        .query_row(
            "SELECT request_json FROM decisions \
             WHERE operation_id LIKE 'op:route:task-prov:verifier:%'",
            [],
            |row| row.get(0),
        )
        .expect("verifier route decision recorded");
    let verifier_request: Value = serde_json::from_str(&verifier_request).expect("verifier json");
    assert_eq!(verifier_request["context"]["role"], "verifier");
    assert_eq!(verifier_request["context"]["origin"], "static");
    assert_eq!(verifier_request["context"]["class"], "verifier");

    // The recorded decisions are durable and reusable on a second call.
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
    let op = OperationId::new("op:route:task-prov:verifier:free");
    match ledger.decision_outcome(&op).expect("read outcome") {
        Some(agalma_contracts::DecisionOutcome::Applied {
            chosen: Some(chosen),
            ..
        }) => assert_eq!(chosen, FREE_B),
        other => panic!("unexpected verifier outcome {other:?}"),
    }
}

/// Deterministic property: for a fixed enumerated matrix of scoped builder
/// selectors and global selectors, the scoped selector changes nothing for a
/// non-builder role — every non-builder class resolves from the global selector
/// alone. Fixed case budget: 6 selectors x 2 globals x 4 roles = 48 cases.
#[test]
fn scoped_builder_selector_never_mutates_non_builder_roles() {
    let dir = scratch("sf-builder-prop");
    let scoped = [
        None,
        Some(""),
        Some("   "),
        Some(FREE_A),
        Some(FREE_B),
        Some(FLASH),
    ];
    let globals = [None, Some("custom/legacy-global")];
    let roles = ["verifier", "diagnosis", "planner", "decision"];
    let mut cases = 0usize;
    for (si, selector) in scoped.iter().enumerate() {
        for (gi, global) in globals.iter().enumerate() {
            let overrides = RoleModelOverride::new(*selector, *global);
            assert_eq!(
                overrides.effective_global(),
                *global,
                "scoped selector {selector:?} leaked into the global selector"
            );
            for role in roles {
                cases += 1;
                let scope = format!("prop-{si}-{gi}-{role}");
                let via_scoped = routing::resolve_route(
                    &dir,
                    &scope,
                    role,
                    TaskFamily::Feature,
                    true,
                    true,
                    overrides.effective_global(),
                )
                .expect("non-builder route via scoped overrides");
                let via_global = routing::resolve_route(
                    &dir,
                    &scope,
                    role,
                    TaskFamily::Feature,
                    true,
                    true,
                    *global,
                )
                .expect("non-builder route via the global selector");
                assert_eq!(via_scoped.model, via_global.model, "role {role}");
                assert_eq!(via_scoped.origin, via_global.origin, "role {role}");
            }
        }
    }
    assert_eq!(
        cases,
        6 * 2 * 4,
        "fixed deterministic case budget is exercised"
    );
}

fn pin_binding() -> BindingPin {
    BindingPin {
        binding_id: "fixture.harness".to_string(),
        api: "harness".to_string(),
        api_version: 1,
        implementation_version: "fixture-1".to_string(),
        generation: 1,
        config_digest: sha256_digest(b"{}"),
    }
}

fn pinned_definition() -> TaskDefinition {
    TaskDefinition {
        task_id: TaskId::new("fixture.pin"),
        title: "Role pin fixture".to_string(),
        description: "Body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\npriority: high\nfamily: feature\nmax_attempts: 2\n```\n".to_string(),
        design: Some("Design.\n".to_string()),
        notes: None,
        acceptance_criteria: None,
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    }
}

fn pinned_inputs(builder_model: &str, verifier_model: &str) -> PackageInputs {
    PackageInputs {
        execution_id: "exec:fixture.pin:1".to_string(),
        generation: 1,
        lease_generation: 1,
        repository_identity: "agalma-fixture".to_string(),
        base_sha: "1".repeat(40),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 1,
            digest: sha256_digest(b"directive/v0"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"policy"),
            receipt_source: "objects/authorization.json".to_string(),
            receipt_media_type: "application/json".to_string(),
            receipt_bytes: b"{}".to_vec(),
        },
        roles: vec![
            RoleInput::new(
                Role::Builder,
                "builder:feature",
                builder_model,
                pin_binding(),
                b"builder prompt".to_vec(),
                "text/plain",
            ),
            RoleInput::new(
                Role::Verifier,
                "verifier:feature",
                verifier_model,
                pin_binding(),
                b"verifier prompt".to_vec(),
                "text/plain",
            ),
        ],
        instructions: vec![ContextInput::new(
            ContextKind::Instruction,
            "AGENTS.md",
            "text/plain",
            b"repo rules".to_vec(),
        )],
        shared_design: Vec::new(),
        parent_design: Vec::new(),
        dependency_handoffs: Vec::new(),
        allowed_paths: Vec::new(),
        protected_paths: Vec::new(),
        readable_artifacts: Vec::new(),
        oracle_bytes: b"oracle".to_vec(),
        oracle_media_type: "text/plain".to_string(),
        max_attempts: 2,
        allowance_nusd: 2_000_000_000,
        quota_policy_digest: sha256_digest(b"quota"),
        recorded_at_unix_ms: 1_791_820_800_000,
    }
}

/// Deterministic property: an admitted package's role pins round-trip through the
/// immutable store for every builder selector, so recovery reads the exact
/// admitted builder/verifier models and never a re-resolved one. Fixed case
/// budget: 4 (builder, verifier) pairs, including a paid id and an unknown id.
#[test]
fn frozen_role_pins_survive_reload_for_every_builder_selector() {
    let cases: [(&str, &str); 4] = [
        (FREE_A, FREE_B),
        (FREE_B, FREE_A),
        (FLASH, FREE_A),
        ("custom/builder-model", "custom/verifier-model"),
    ];
    for (i, (builder_model, verifier_model)) in cases.iter().enumerate() {
        let dir = scratch(&format!("sf-builder-pin-{i}"));
        let built = build_package(
            &pinned_definition(),
            pinned_inputs(*builder_model, *verifier_model),
        )
        .expect("build package");
        let persisted = persist_package(&dir, &built).expect("persist package");
        let loaded = load_package(&dir, &persisted.digest).expect("reload package");
        assert_eq!(
            role_model(&loaded, Role::Builder),
            *builder_model,
            "the builder pin is immutable across reload"
        );
        assert_eq!(
            role_model(&loaded, Role::Verifier),
            *verifier_model,
            "the verifier pin is immutable across reload"
        );
    }
}

// ---------------------------------------------------------------------------
// SF.6 operator protected inputs the production fixture seeds
// ---------------------------------------------------------------------------
const ENTRY_TASK: &str = "fixture.protected";
const ENTRY_BASE: &str = "1111111111111111111111111111111111111111";

/// A valid operator `ProtectedEntry/v1` rooted at a synthetic origin.
fn entry_with(task: &str, base: &str) -> ProtectedEntry {
    let marker = "protected marker\n";
    ProtectedEntry {
        task_id: task.to_string(),
        repository_root: "/agalma-entry".to_string(),
        base_sha: base.to_string(),
        allowed_paths: vec!["src".to_string(), "docs".to_string(), "AGENTS.md".to_string()],
        protected_paths: vec!["tests".to_string()],
        baseline: ProtectedBaseline {
            format: PROTECTED_BASELINE_FORMAT.to_string(),
            files: vec![BaselineFile {
                path: "protected/marker.txt".to_string(),
                mode: "100644".to_string(),
                digest: sha256_digest(marker.as_bytes()),
                bytes: marker.len() as u64,
                text: marker.to_string(),
            }],
            commands: vec!["sh -c 'true'".to_string()],
        },
    }
}

fn protected_definition(task: &str) -> TaskDefinition {
    TaskDefinition {
        task_id: TaskId::new(task),
        title: "Protected entry fixture".to_string(),
        description: "Body.\n".to_string(),
        design: Some("Design.\n".to_string()),
        notes: None,
        acceptance_criteria: Some("sh -c 'true'".to_string()),
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    }
}

/// Record a frozen package with the given scope/oracle and return its digest.
fn frozen_package(
    state_dir: &Path,
    package_task: &str,
    package_base: &str,
    allowed: Vec<String>,
    protected: Vec<String>,
    oracle_bytes: Vec<u8>,
) -> String {
    let inputs = PackageInputs {
        execution_id: format!("exec:{package_task}:1"),
        generation: 1,
        lease_generation: 1,
        repository_identity: "agalma-entry-fixture".to_string(),
        base_sha: package_base.to_string(),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 1,
            digest: sha256_digest(b"directive/v0"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"policy"),
            receipt_source: "objects/authorization.json".to_string(),
            receipt_media_type: "application/json".to_string(),
            receipt_bytes: b"{}".to_vec(),
        },
        roles: vec![RoleInput::new(
            Role::Builder,
            "builder:feature",
            FREE_A,
            pin_binding(),
            b"builder prompt".to_vec(),
            "text/plain",
        )],
        instructions: vec![ContextInput::new(
            ContextKind::Instruction,
            "AGENTS.md",
            "text/plain",
            b"repo rules".to_vec(),
        )],
        shared_design: Vec::new(),
        parent_design: Vec::new(),
        dependency_handoffs: Vec::new(),
        allowed_paths: allowed,
        protected_paths: protected,
        readable_artifacts: Vec::new(),
        oracle_bytes,
        oracle_media_type: "application/json".to_string(),
        max_attempts: 2,
        allowance_nusd: 0,
        quota_policy_digest: sha256_digest(b"quota"),
        recorded_at_unix_ms: 1_791_820_800_000,
    };
    let built = build_package(&protected_definition(package_task), inputs).expect("build package");
    persist_package(state_dir, &built).expect("persist package");
    built.package.package_digest
}

/// Deterministic property (fixed enumerated budget: 6 package/oracle
/// agreement cases + 3 manifest cases = 9 assertions; no randomness) over the
/// SF.6 contract the production fixture relies on: a frozen package whose oracle
/// is the selected complete `ProtectedEntry/v1` loads only when the entry and
/// package scope agree; every identity-, base-, scope-, or oracle-malformed
/// variant refuses. Replay:
/// `cargo test --offline --locked -p agalma-conductor --test sf_builder_route`.
#[test]
fn frozen_protected_entry_agreement_property() {
    let dir = scratch("sf-builder-entry");
    let entry = entry_with(ENTRY_TASK, ENTRY_BASE);
    let allowed = entry.allowed_paths.clone();
    let protected = entry.protected_paths.clone();

    let mut cases: Vec<(&str, String, String, Vec<String>, Vec<String>, Vec<u8>, bool)> = Vec::new();
    cases.push((
        "valid",
        ENTRY_TASK.to_string(),
        ENTRY_BASE.to_string(),
        allowed.clone(),
        protected.clone(),
        entry.freeze().expect("freeze"),
        true,
    ));
    cases.push((
        "task_mismatch",
        ENTRY_TASK.to_string(),
        ENTRY_BASE.to_string(),
        allowed.clone(),
        protected.clone(),
        entry_with("other.task", ENTRY_BASE).freeze().expect("freeze"),
        false,
    ));
    cases.push((
        "base_mismatch",
        ENTRY_TASK.to_string(),
        ENTRY_BASE.to_string(),
        allowed.clone(),
        protected.clone(),
        entry_with(ENTRY_TASK, "2222222222222222222222222222222222222222")
            .freeze()
            .expect("freeze"),
        false,
    ));
    cases.push((
        "allowed_disagree",
        ENTRY_TASK.to_string(),
        ENTRY_BASE.to_string(),
        vec!["different".to_string()],
        protected.clone(),
        entry.freeze().expect("freeze"),
        false,
    ));
    cases.push((
        "protected_disagree",
        ENTRY_TASK.to_string(),
        ENTRY_BASE.to_string(),
        allowed.clone(),
        vec!["different".to_string()],
        entry.freeze().expect("freeze"),
        false,
    ));
    cases.push((
        "opaque_oracle",
        ENTRY_TASK.to_string(),
        ENTRY_BASE.to_string(),
        allowed.clone(),
        protected.clone(),
        b"harness-oracle".to_vec(),
        false,
    ));

    let mut executed = 0usize;
    for (label, pkg_task, pkg_base, allow, prot, oracle, expect_ok) in &cases {
        executed += 1;
        let case_dir = dir.child(&format!("case-{executed}"));
        let digest = frozen_package(
            &case_dir,
            pkg_task,
            pkg_base,
            allow.clone(),
            prot.clone(),
            oracle.clone(),
        );
        let result = load_admitted_package(&case_dir, &digest);
        assert_eq!(
            result.is_ok(),
            *expect_ok,
            "case {label:?}: recorded-package validity must be {expect_ok} ({result:?})"
        );
    }
    assert_eq!(executed, 6, "fixed deterministic case budget is exercised");

    // The operator manifest the fixture writes parses, selects its frozen
    // identity, and an absent manifest refuses.
    let doc = json!({
        "format": PROTECTED_INPUTS_FORMAT,
        "entries": [entry.clone()],
    });
    let bytes = serde_json::to_vec(&doc).expect("json");
    let parsed = ProtectedInputs::parse(&bytes).expect("manifest parses");
    assert!(
        parsed.select(ENTRY_TASK, "/agalma-entry", ENTRY_BASE).is_ok(),
        "the frozen identity selects"
    );
    assert!(
        parsed.select(ENTRY_TASK, "/other", ENTRY_BASE).is_err(),
        "a repository identity mismatch refuses"
    );
    assert!(
        ProtectedInputs::load(&dir).is_err(),
        "an absent operator manifest refuses repository work"
    );
}

// ---------------------------------------------------------------------------
// Production: immutable pin through `agalma work`, and restart preservation
// ---------------------------------------------------------------------------
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

/// Isolated XDG config base for a scratch fixture.
fn config_home(state_dir: &Path) -> PathBuf {
    state_dir.parent().expect("state parent").join("xdg-config")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
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

fn assert_aborted(status: &std::process::ExitStatus) {
    assert!(!status.success(), "process should abort: {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT: {status:?}");
    }
}

/// Repository instruction/shared-design bytes the production admission freezes.
const AGENTS_BYTES: &str = "# Agents\nRepository working rules (frozen at admission).\n";
const SHARED_DESIGN_BYTES: &str = "# Shared design revision 1\nFrozen shared-design bytes.\n";
/// Acceptance the scripted build satisfies (it writes `42` into `src/lib.rs`).
const GREEN: &str = "  - sh -c 'grep -q 42 src/lib.rs'";

/// A scratch origin (git + isolated bd) with the repository documents production
/// admission freezes.
struct Prod {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Prod {
    fn new(name: &str) -> Prod {
        require_bd();
        let dir = scratch(&format!("sf-route-{name}"));
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
        // SF.6: production admission requires the operator `ProtectedInputs/v1`
        // entry for the task; missing/mismatched inputs stop repository work.
        self.protect(&id);
        id
    }

    fn head(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.origin)
            .output()
            .expect("git rev-parse");
        assert!(out.status.success(), "git rev-parse HEAD");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Write/merge the operator `ProtectedInputs/v1` entry for `task_id` into
    /// the isolated XDG config (SF.6), mirroring production
    /// admission: canonical repository root, resolved base SHA, a nonempty
    /// allow/protect scope, and a `ProtectedBaseline/v1` with one file + command.
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
                "format": PROTECTED_BASELINE_FORMAT,
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
            .join(PROTECTED_INPUTS_FILE);
        let mut entries: Vec<Value> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("entries").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        entries.retain(|e| e.get("task_id").and_then(Value::as_str) != Some(task_id));
        entries.push(entry);
        fs::create_dir_all(path.parent().expect("manifest parent")).expect("config dir");
        let doc = json!({ "format": PROTECTED_INPUTS_FORMAT, "entries": entries });
        let bytes = serde_json::to_vec_pretty(&doc).expect("json");
        // Validate through the production contract before persisting.
        ProtectedInputs::parse(&bytes).expect("seeded operator manifest must be valid");
        fs::write(&path, bytes).expect("write manifest");
    }

    /// Run `agalma work --once` against the scratch origin in scripted mode.
    ///
    /// The route-affecting env vars are cleared first so the runner's environment
    /// cannot leak into a route decision; `extra` then sets the intended ones.
    fn work(&self, flags: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut command = Command::new(bin());
        command.args([
            "work",
            "--repo",
            self.origin.to_str().expect("origin utf8"),
            "--state-dir",
            self.state_dir.to_str().expect("state utf8"),
            "--once",
        ]);
        command.args(flags);
        command
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .env("AGALMA_LEDGER_TRANSPORT", "in-process")
            .env("AGALMA_WORKSPACE_TRANSPORT", "in-process")
            .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
            .env("XDG_CONFIG_HOME", config_home(&self.state_dir))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("AGALMA_ALLOW_PAID")
            .env_remove("AGALMA_MODEL")
            .env_remove("AGALMA_BUILDER_MODEL");
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

    fn frozen(&self, digest: &str) -> agalma_contracts::TaskPackage {
        load_package(&self.state_dir, digest).expect("load frozen package")
    }
}

fn role_model(package: &agalma_contracts::TaskPackage, role: Role) -> String {
    package
        .roles
        .iter()
        .find(|pin| pin.role == role)
        .unwrap_or_else(|| panic!("frozen package has no {role:?} role pin"))
        .model
        .clone()
}

/// The scoped builder selector is pinned into the immutable package and the
/// verifier keeps its independently resolved (legacy global) route.
#[test]
fn production_pins_scoped_builder_and_independent_verifier() {
    let prod = Prod::new("pin");
    let id = prod.create_task("Builder route pin");

    let out = prod.work(&["--builder-model", FREE_A, "--model", FREE_B], &[]);
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
    let frozen = prod.frozen(&digest);

    assert_eq!(
        role_model(&frozen, Role::Builder),
        FREE_A,
        "the scoped builder selector is pinned in the immutable package"
    );
    assert_eq!(
        role_model(&frozen, Role::Verifier),
        FREE_B,
        "the verifier keeps the legacy global route, not the builder selector"
    );
    assert_ne!(
        role_model(&frozen, Role::Builder),
        role_model(&frozen, Role::Verifier),
        "builder and verifier selections are independent"
    );
}

/// A restart after the environment changes keeps the frozen role pins: recovery
/// reuses the admitted package and never re-resolves the builder selector.
#[test]
fn restart_after_environment_change_preserves_role_pins() {
    let prod = Prod::new("restart");
    let id = prod.create_task("Builder pin restart");

    // Admit with the scoped selector, then crash before any phase handler runs.
    let out = prod.work(
        &["--builder-model", FREE_A],
        &[("AGALMA_PACKAGE_CRASH_AT", "after_admit")],
    );
    assert_aborted(&out.status);

    let execution = format!("exec:{id}:1");
    let event = prod.admitted(&execution).expect("admission committed");
    let digest = event["package_digest"]
        .as_str()
        .expect("package digest")
        .to_string();
    assert_eq!(
        role_model(&prod.frozen(&digest), Role::Builder),
        FREE_A,
        "the admitted package pins the scoped builder model"
    );

    // The environment changes before recovery: a different builder selector and a
    // different legacy global selector are both supplied.
    let recovery = prod.work(
        &["--builder-model", FREE_B],
        &[
            ("AGALMA_BUILDER_MODEL", FREE_B),
            ("AGALMA_MODEL", "custom/other"),
        ],
    );
    assert!(
        recovery.status.success(),
        "recovery run: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!(
        (phase.as_str(), state.as_str()),
        ("done", "completed"),
        "the recovered execution completes on its frozen pins"
    );
    assert_eq!(
        role_model(&prod.frozen(&digest), Role::Builder),
        FREE_A,
        "recovery preserves the frozen builder pin despite the changed environment"
    );
}

/// An ineligible paid builder selection is refused before claiming any task, so a
/// requested paid model is never silently substituted.
#[test]
fn work_refuses_ineligible_paid_builder_selector() {
    let prod = Prod::new("deny");
    let id = prod.create_task("Denied builder");

    let out = prod.work(&["--builder-model", FLASH], &[]);
    assert!(
        !out.status.success(),
        "an ineligible paid builder selection is refused: {}",
        stdout_of(&out)
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("builder model selector refused"),
        "refusal is explicit: {stderr}"
    );
    assert!(stderr.contains(FLASH), "refusal names the model: {stderr}");
    assert!(
        !prod.has_execution(&format!("exec:{id}:1")),
        "the run refuses before claiming/dispatching the task"
    );
}

// ---------------------------------------------------------------------------
// Deterministic generation over the protected-input contract (fixed seed)
// ---------------------------------------------------------------------------

/// SplitMix64 generator (same algorithm as `agalma-testkit`; deterministic,
/// no runtime randomness). Replay: fixed seeds below, fixed case budgets.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Independent property: for generated `ProtectedEntry` mutations, a case is
/// valid **iff** its mutation class is the valid class; valid cases round-trip
/// `freeze`/`parse` identically and every malformed / missing-identity /
/// bad-path / baseline-boundary case refuses. Expected validity derives from the
/// class, not the implementation. Deterministic seed `0x5F8B_0000_0012`, fixed
/// budget 256; every class is asserted generated.
#[test]
fn generated_protected_entry_mutations_round_trip_or_refuse() {
    const SEED: u64 = 0x5F8B_0000_0012;
    const BUDGET: u64 = 256;
    let mut rng = Rng::new(SEED);
    let mut class_hits = [0u64; 8];
    for case in 0..BUDGET {
        let mut entry = entry_with(ENTRY_TASK, ENTRY_BASE);
        let class = rng.below(class_hits.len() as u64) as usize;
        class_hits[class] += 1;
        let expected_valid = match class {
            0 => true,
            1 => {
                match rng.below(3) {
                    0 => entry.task_id.clear(),
                    1 => entry.repository_root.clear(),
                    _ => entry.base_sha.clear(),
                }
                false
            }
            2 => {
                let bad = ["/abs", "..", "a/../b", "a//b", "."][rng.below(5) as usize].to_string();
                if rng.next_u64() & 1 == 0 {
                    entry.allowed_paths = vec![bad];
                } else {
                    entry.protected_paths = vec![bad];
                }
                false
            }
            3 => {
                entry.baseline.format = "ProtectedBaseline/v0".to_string();
                false
            }
            4 => {
                entry.baseline.files.clear();
                false
            }
            5 => {
                entry.baseline.commands.clear();
                false
            }
            6 => {
                if rng.next_u64() & 1 == 0 {
                    entry.baseline.files[0].digest = sha256_digest(b"other");
                } else {
                    entry.baseline.files[0].mode = "120000".to_string();
                }
                false
            }
            _ => {
                entry.baseline.files.push(entry.baseline.files[0].clone());
                false
            }
        };
        let bytes = entry.freeze().expect("freeze");
        let parsed = ProtectedEntry::parse(&bytes);
        assert_eq!(
            parsed.is_ok(),
            expected_valid,
            "case {case}: class {class} validity must be {expected_valid}"
        );
        if expected_valid {
            assert_eq!(
                parsed.expect("valid entry parses"),
                entry,
                "case {case}: a valid entry round-trips identically"
            );
        }
    }
    let total: u64 = class_hits.iter().sum();
    assert_eq!(total, BUDGET, "fixed generated case budget is exercised");
    for (class, hits) in class_hits.iter().enumerate() {
        assert!(*hits > 0, "mutation class {class} was generated");
    }
}

/// Independent property over generated `ProtectedInputs/v1` manifest
/// mutations: a valid manifest parses and selects the frozen identity; a
/// duplicate-entry, wrong-format, empty, unknown-field, or identity-mismatched
/// manifest refuses. Deterministic seed `0x5F8B_0000_0013`, fixed budget 128;
/// every class is asserted generated.
#[test]
fn generated_protected_manifest_mutations_select_or_refuse() {
    const SEED: u64 = 0x5F8B_0000_0013;
    const BUDGET: u64 = 128;
    let base = entry_with(ENTRY_TASK, ENTRY_BASE);
    let mut rng = Rng::new(SEED);
    let mut class_hits = [0u64; 6];
    for case in 0..BUDGET {
        let class = rng.below(class_hits.len() as u64) as usize;
        class_hits[class] += 1;
        let expect_parse_ok = class == 0 || class == 5;
        let refuse_selection = class == 5;
        let doc = match class {
            0 => json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap()],
            }),
            1 => json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap(), serde_json::to_value(&base).unwrap()],
            }),
            2 => json!({
                "format": "ProtectedInputs/v2",
                "entries": [serde_json::to_value(&base).unwrap()],
            }),
            3 => json!({ "format": PROTECTED_INPUTS_FORMAT, "entries": [] }),
            4 => json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap()],
                "extra": 1,
            }),
            _ => json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap()],
            }),
        };
        let bytes = serde_json::to_vec(&doc).expect("json");
        match ProtectedInputs::parse(&bytes) {
            Ok(parsed) => {
                assert!(expect_parse_ok, "case {case}: class {class} must refuse parsing");
                if refuse_selection {
                    assert!(
                        parsed.select("missing.task", "/agalma-entry", ENTRY_BASE).is_err(),
                        "case {case}: a missing task entry refuses selection"
                    );
                    assert!(
                        parsed.select(ENTRY_TASK, "/other", ENTRY_BASE).is_err(),
                        "case {case}: a repository identity mismatch refuses selection"
                    );
                } else {
                    assert!(
                        parsed.select(ENTRY_TASK, "/agalma-entry", ENTRY_BASE).is_ok(),
                        "case {case}: the frozen identity selects"
                    );
                }
            }
            Err(_) => assert!(!expect_parse_ok, "case {case}: class {class} must parse"),
        }
    }
    let total: u64 = class_hits.iter().sum();
    assert_eq!(total, BUDGET, "fixed generated case budget is exercised");
    for (class, hits) in class_hits.iter().enumerate() {
        assert!(*hits > 0, "manifest mutation class {class} was generated");
    }
}
