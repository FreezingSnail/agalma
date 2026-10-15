//! SF.1 (`agalma-5fy.1`) behavioral tests: the repository builder prompt
//! consumes the recorded immutable `TaskPackage/v1` and renders its complete
//! frozen context; the fixture prompt stays on its own explicit path.
//!
//! These tests build independent task-package fixtures (markdown bodies, context
//! objects, scopes, acceptance) with real content-addressed objects under a
//! `ScratchDir`, then render the prompt and assert the recorded sections appear
//! verbatim. No live provider call, no `bd`, no network. A deterministic
//! generator drives the property test with a fixed seed and case budget.
//!
//! Focused command: `cargo test --offline -p agalma-conductor --test sf_builder_prompt`.

use std::path::Path;

use agalma_conductor::composition::{BUILDER_FIXTURE_PROMPT, BUILDER_PROMPT};
use agalma_conductor::phases::{render_builder_prompt, resolve_recorded_package, RepoMode};
use agalma_conductor::task_package::{object_path, object_relative_path, package_path};
use agalma_contracts::{
    sha256_digest, Authorization, AuthorizationOrigin, Budget, ContextItem, ContextKind, Definition,
    DirectiveRef, ExecutionId, PackageArtifactRef, RepositoryRef, Scope, TaskPackage, TextObject,
    UnknownUsage, TASK_PACKAGE_FORMAT,
};
use agalma_execution::ActivityError;
use agalma_testkit::ScratchDir;

// ---------------------------------------------------------------------------
// Independent package fixtures
// ---------------------------------------------------------------------------

/// A description-independent task-package specification.
#[derive(Clone)]
struct Spec {
    task_id: String,
    execution_id: String,
    title: String,
    description: String,
    design: String,
    notes: String,
    machine_block: String,
    acceptance: Vec<String>,
    agents_md: Option<String>,
    shared_design: Option<String>,
    parent_design: Option<String>,
    dependency_handoff: Option<String>,
    allowed: Vec<String>,
    protected: Vec<String>,
}

impl Spec {
    /// The multi-file feature fixture: a feature task whose scope names several
    /// source and test files.
    fn feature() -> Self {
        Spec {
            task_id: "sf.fixture.feature".to_string(),
            execution_id: "exec:sf.fixture.feature:1".to_string(),
            title: "Render the complete frozen builder context".to_string(),
            description: "# Scope\nImplement a bounded feature across the listed files.\n\n\
# Files\n- `crates/example/src/alpha.rs`\n- `crates/example/src/beta.rs`\n\
- `crates/example/tests/feature.rs`\n\n\
# Contract\nConsume the recorded package and preserve immutable rules.\n\n\
# Invariants\nDo not weaken the acceptance criteria.\n\n\
# Non-goals\nDo not add a second database.\n"
                .to_string(),
            design: "# Design\nThe feature adds `alpha()` and `beta()`.\n".to_string(),
            notes: "# Notes\nFollow the repository conventions.\n".to_string(),
            machine_block: "directive: directive/v0\ntarget: agalma\nref: main\n\
acceptance:\n  - cargo test --offline -p example\npriority: high\nfamily: feature\nmax_attempts: 2\n"
                .to_string(),
            acceptance: vec![
                "cargo test --offline -p example".to_string(),
                "sh -c 'true'".to_string(),
            ],
            agents_md: Some(
                "# Repository rules\n\nAGALMA-RULES-MARKER: never commit; the conductor \
                 integrates.\n"
                    .to_string(),
            ),
            shared_design: Some("SHARED-DESIGN-MARKER shared design body.\n".to_string()),
            parent_design: Some("PARENT-DESIGN-MARKER parent design body.\n".to_string()),
            dependency_handoff: Some("DEPENDENCY-HANDOFF-MARKER dependency body.\n".to_string()),
            allowed: vec![
                "crates/example/src/alpha.rs".to_string(),
                "crates/example/src/beta.rs".to_string(),
                "crates/example/tests/feature.rs".to_string(),
            ],
            protected: vec!["fixtures/protected/".to_string()],
        }
    }
}

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

fn context_item(
    state_dir: &Path,
    kind: ContextKind,
    source: &str,
    text: &str,
) -> ContextItem {
    ContextItem {
        kind,
        source: source.to_string(),
        artifact: write_object(state_dir, text.as_bytes()),
    }
}

/// Build a sealed `TaskPackage/v1` from `spec`, writing every referenced object.
fn build_package(state_dir: &Path, spec: &Spec) -> TaskPackage {
    let mut context = Vec::new();
    if let Some(text) = &spec.agents_md {
        context.push(context_item(
            state_dir,
            ContextKind::Instruction,
            "AGENTS.md",
            text,
        ));
    }
    if let Some(text) = &spec.shared_design {
        context.push(context_item(
            state_dir,
            ContextKind::SharedDesign,
            "docs/self-mutation-contracts.md",
            text,
        ));
    }
    if let Some(text) = &spec.parent_design {
        context.push(context_item(
            state_dir,
            ContextKind::ParentDesign,
            "bd:fixture.parent/design",
            text,
        ));
    }
    if let Some(text) = &spec.dependency_handoff {
        context.push(context_item(
            state_dir,
            ContextKind::DependencyHandoff,
            "bd:fixture.dep/handoff",
            text,
        ));
    }
    let receipt = write_object(state_dir, b"{}");
    let oracle = write_object(state_dir, b"oracle-answer");

    TaskPackage {
        format: TASK_PACKAGE_FORMAT.to_string(),
        task_id: spec.task_id.clone(),
        execution_id: spec.execution_id.clone(),
        generation: 1,
        lease_generation: 1,
        repository: RepositoryRef {
            identity: "agalma".to_string(),
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
            title: spec.title.clone(),
            description: TextObject::new("bd:fixture/description", spec.description.clone()),
            design: TextObject::new("bd:fixture/design", spec.design.clone()),
            notes: TextObject::new("bd:fixture/notes", spec.notes.clone()),
            machine_block: spec.machine_block.clone(),
            acceptance: spec.acceptance.clone(),
        },
        context,
        scope: Scope {
            allowed_paths: spec.allowed.clone(),
            protected_paths: spec.protected.clone(),
            readable_artifacts: Vec::new(),
            oracle,
        },
        max_attempts: 2,
        budget: Budget {
            allowance_nusd: 2_000_000_000,
            unknown_usage: UnknownUsage::Deny,
            quota_policy_digest: sha256_digest(b"quota"),
        },
        roles: Vec::new(),
        recorded_at_unix_ms: 1_791_820_800_000,
        package_digest: String::new(),
    }
    .seal()
    .expect("seal package")
}

/// Persist a sealed package document into the frozen store.
fn persist_document(state_dir: &Path, package: &TaskPackage) {
    let path = package_path(state_dir, &package.package_digest).expect("package path");
    std::fs::create_dir_all(path.parent().expect("package parent")).expect("package dir");
    std::fs::write(&path, serde_json::to_vec(package).expect("serialize")).expect("write package");
}

fn repo_mode(title: &str, acceptance: &[&str]) -> RepoMode {
    RepoMode {
        origin: "/nonexistent/origin".into(),
        base_ref: "main".to_string(),
        task_title: title.to_string(),
        acceptance: acceptance.iter().map(|s| s.to_string()).collect(),
    }
}

fn render(
    state_dir: &Path,
    base: &str,
    repo: Option<&RepoMode>,
    package: Option<&TaskPackage>,
    root: &Path,
    attempt: u32,
) -> Result<String, ActivityError> {
    render_builder_prompt(state_dir, base, repo, package, root, attempt)
}

// ---------------------------------------------------------------------------
// Behavioral coverage
// ---------------------------------------------------------------------------

#[test]
fn repository_prompt_delivers_every_frozen_section() {
    let dir = ScratchDir::new("sf-builder-prompt-full");
    let state = dir.child("state");
    let spec = Spec::feature();
    let package = build_package(&state, &spec);
    let repo = repo_mode(&spec.title, &["cargo test --offline -p example"]);

    let text = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &state, 1)
        .expect("repository render");

    // Scope/Files/Contract/Invariants/Non-goals body sections.
    for heading in ["# Scope", "# Files", "# Contract", "# Invariants", "# Non-goals"] {
        assert!(text.contains(heading), "missing body section {heading:?}:\n{text}");
    }
    // Multi-file feature: every recorded path is delivered.
    for path in &spec.allowed {
        assert!(text.contains(path), "missing allowed path {path:?}:\n{text}");
    }
    assert!(text.contains("fixtures/protected/"), "missing protected path:\n{text}");
    // Design and notes.
    assert!(text.contains("The feature adds `alpha()` and `beta()`"), "{text}");
    assert!(text.contains("Follow the repository conventions"), "{text}");
    // Shared design, parent design, dependency handoff, and immutable rules.
    assert!(text.contains("SHARED-DESIGN-MARKER"), "{text}");
    assert!(text.contains("PARENT-DESIGN-MARKER"), "{text}");
    assert!(text.contains("DEPENDENCY-HANDOFF-MARKER"), "{text}");
    assert!(text.contains("AGALMA-RULES-MARKER"), "immutable rules:\n{text}");
    // Acceptance commands.
    assert!(text.contains("cargo test --offline -p example"), "{text}");
    assert!(text.contains("sh -c 'true'"), "{text}");
}

/// Retain the representative rendered repository prompt as a durable artifact
/// for the SF.9 successor smoke (`agalma-5fy.1` design). The accepted multi-file
/// feature fixture is rendered through the same production path and the raw
/// bytes are written under `target/test-runs/agalma-5fy.1/`. Override the
/// destination with `SF1_RENDERED_PROMPT_OUT`.
#[test]
fn retains_rendered_repository_prompt_artifact() {
    let dir = ScratchDir::new("sf-builder-prompt-artifact");
    let state = dir.child("state");
    let spec = Spec::feature();
    let package = build_package(&state, &spec);
    let repo = repo_mode(&spec.title, &["cargo test --offline -p example"]);
    let prompt = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &state, 1)
        .expect("repository render");

    let artifact = std::env::var_os("SF1_RENDERED_PROMPT_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../target/test-runs/agalma-5fy.1/final-verify-20261014/rendered-builder-prompt.md",
            )
        });
    std::fs::create_dir_all(artifact.parent().expect("artifact parent")).expect("artifact dir");
    std::fs::write(&artifact, prompt.as_bytes()).expect("write rendered prompt artifact");
    println!(
        "rendered-builder-prompt {} sha256:{}",
        artifact.display(),
        sha256_digest(prompt.as_bytes())
    );
    assert!(
        prompt.contains("# Frozen task context"),
        "representative prompt:\n{prompt}"
    );
}

#[test]
fn repository_prompt_never_tells_a_feature_builder_to_make_a_one_line_edit() {
    let dir = ScratchDir::new("sf-builder-prompt-no-one-line");
    let state = dir.child("state");
    let spec = Spec::feature();
    let package = build_package(&state, &spec);
    let repo = repo_mode(&spec.title, &["cargo test"]);

    let text = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &state, 1)
        .expect("repository render");
    assert!(!text.contains("one-line"), "repository prompt is fixture-shaped:\n{text}");
    assert!(
        !text.contains("deliberately broken function"),
        "repository prompt describes the fixture bug:\n{text}"
    );
    // The repository base itself is not the M0 fixture text either.
    assert!(!BUILDER_PROMPT.contains("one-line"), "repository base prompt is fixture-shaped");
}

#[test]
fn retry_prompt_includes_prior_failure_and_diagnosis_by_file_and_inline() {
    let dir = ScratchDir::new("sf-builder-prompt-retry");
    let state = dir.child("state");
    let run = dir.child("run");
    std::fs::create_dir_all(run.join("artifacts")).expect("artifacts dir");
    std::fs::write(
        run.join("artifacts").join("failure@1.txt"),
        "acceptance command 0 failed: MAGIC_VALUE not found",
    )
    .expect("failure artifact");
    std::fs::write(
        run.join("artifacts").join("diagnosis@1.md"),
        "## cause\nthe alpha function returns 0\n",
    )
    .expect("diagnosis artifact");

    let spec = Spec::feature();
    let package = build_package(&state, &spec);
    let repo = repo_mode(&spec.title, &["cargo test"]);

    let text = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &run, 2)
        .expect("retry render");
    assert!(text.contains("failure@1.txt"), "failure path reference:\n{text}");
    assert!(text.contains("diagnosis@1.md"), "diagnosis path reference:\n{text}");
    assert!(text.contains("MAGIC_VALUE"), "inline failure text:\n{text}");
    assert!(text.contains("the alpha function returns 0"), "inline diagnosis:\n{text}");
}

#[test]
fn missing_required_context_is_a_known_failure_not_an_m0_fallback() {
    let dir = ScratchDir::new("sf-builder-prompt-missing");
    let state = dir.child("state");
    let repo = repo_mode("Missing context", &["cargo test"]);

    // No frozen repository instruction object.
    let mut spec = Spec::feature();
    spec.agents_md = None;
    let package = build_package(&state, &spec);
    let err = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &state, 1)
        .expect_err("no instruction context must refuse");
    assert!(
        matches!(err, ActivityError::KnownFailure(_)),
        "expected KnownFailure, got {err:?}"
    );

    // An empty title is also required context.
    let mut spec = Spec::feature();
    spec.title = "   ".to_string();
    let package = build_package(&state, &spec);
    let err = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &state, 1)
        .expect_err("blank title must refuse");
    assert!(matches!(err, ActivityError::KnownFailure(_)), "got {err:?}");
}

#[test]
fn an_unreadable_required_object_is_a_known_failure() {
    let dir = ScratchDir::new("sf-builder-prompt-corrupt-object");
    let state = dir.child("state");
    let spec = Spec::feature();
    let package = build_package(&state, &spec);
    // Corrupt the frozen instruction object's bytes (digest no longer matches).
    let instruction = package
        .context
        .iter()
        .find(|item| item.kind == ContextKind::Instruction)
        .expect("instruction context");
    let object = object_path(&state, &instruction.artifact.digest).expect("object path");
    std::fs::write(&object, b"tampered bytes").expect("corrupt object");

    let repo = repo_mode(&spec.title, &["cargo test"]);
    let err = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &state, 1)
        .expect_err("unreadable required object must refuse");
    assert!(matches!(err, ActivityError::KnownFailure(_)), "got {err:?}");
}

#[test]
fn fixture_mode_returns_the_base_prompt_unchanged() {
    let dir = ScratchDir::new("sf-builder-prompt-fixture");
    let state = dir.child("state");
    let spec = Spec::feature();
    let package = build_package(&state, &spec);

    // `repo == None` is the explicit fixture path: the fixture base is returned
    // unchanged, regardless of an attached package.
    let text = render(
        &state,
        BUILDER_FIXTURE_PROMPT,
        None,
        Some(&package),
        &state,
        1,
    )
    .expect("fixture render");
    assert_eq!(text, BUILDER_FIXTURE_PROMPT);
    assert!(text.contains("deliberately broken function"), "M0 fixture text intact");
    assert!(text.contains("one-line change"), "M0 fixture instruction intact");
}

/// Repository mode always requires the recorded package. Absence is a known
/// failure, with no title/acceptance or M0-base fallback, whether or not the
/// package store already exists.
#[test]
fn repository_mode_without_a_recorded_package_is_a_known_failure() {
    let repo = repo_mode("Strict missing package", &["cargo test"]);

    // No store at all.
    let bare = ScratchDir::new("sf-builder-prompt-no-store");
    let bare_state = bare.child("state");
    let err = render(&bare_state, BUILDER_PROMPT, Some(&repo), None, &bare_state, 1)
        .expect_err("repository mode without a package must refuse");
    assert!(matches!(err, ActivityError::KnownFailure(_)), "got {err:?}");

    // A store that has been used but holds no package for another execution.
    let dir = ScratchDir::new("sf-builder-prompt-store-no-match");
    let state = dir.child("state");
    let spec = Spec::feature();
    let package = build_package(&state, &spec);
    persist_document(&state, &package);
    let err = render(&state, BUILDER_PROMPT, Some(&repo), None, &state, 1)
        .expect_err("repository mode without a package must refuse");
    assert!(matches!(err, ActivityError::KnownFailure(_)), "got {err:?}");
}

#[test]
fn fixture_and_repository_prompts_are_distinct() {
    assert_ne!(
        BUILDER_FIXTURE_PROMPT, BUILDER_PROMPT,
        "fixture and repository prompts must be separate texts"
    );
    assert!(BUILDER_FIXTURE_PROMPT.contains("one-line change"));
    assert!(!BUILDER_PROMPT.contains("one-line"));
    // Both are substantive.
    assert!(BUILDER_PROMPT.trim().len() > 400, "repository prompt too short");
}

// ---------------------------------------------------------------------------
// Recorded-package resolution (frozen store, never a live backlog read)
// ---------------------------------------------------------------------------

#[test]
fn resolves_the_recorded_package_by_execution_from_the_frozen_store() {
    let dir = ScratchDir::new("sf-builder-prompt-resolve");
    let state = dir.child("state");
    let spec = Spec::feature();
    let package = build_package(&state, &spec);
    persist_document(&state, &package);

    let execution = ExecutionId::new(spec.execution_id.clone());
    let resolved = resolve_recorded_package(&state, &execution)
        .expect("resolve")
        .expect("recorded package present");
    // The frozen document is returned byte-for-byte: same digest and definition.
    assert_eq!(resolved.package_digest, package.package_digest);
    assert_eq!(resolved.definition, package.definition);
    assert_eq!(resolved.scope, package.scope);

    // An execution with no recorded package resolves to `None` (never a guess).
    let other = ExecutionId::new("exec:sf.fixture.absent:1");
    assert!(
        resolve_recorded_package(&state, &other)
            .expect("resolve absent")
            .is_none()
    );
}

#[test]
fn ambiguous_or_corrupt_recorded_packages_are_refused() {
    let dir = ScratchDir::new("sf-builder-prompt-ambiguous");
    let state = dir.child("state");
    let spec = Spec::feature();
    let first = build_package(&state, &spec);
    persist_document(&state, &first);

    // A second, distinct package for the same execution is ambiguous.
    let mut second_spec = Spec::feature();
    second_spec.title = "A different recorded definition".to_string();
    let second = build_package(&state, &second_spec);
    persist_document(&state, &second);

    let execution = ExecutionId::new(spec.execution_id.clone());
    let err = resolve_recorded_package(&state, &execution).expect_err("ambiguous");
    assert!(matches!(err, ActivityError::KnownFailure(_)), "got {err:?}");

    // Corrupt a referenced object: the stored package can no longer be verified.
    let clean = ScratchDir::new("sf-builder-prompt-corrupt-store");
    let clean_state = clean.child("state");
    let package = build_package(&clean_state, &spec);
    persist_document(&clean_state, &package);
    let context = package.context.first().expect("context").artifact.digest.clone();
    let object = object_path(&clean_state, &context).expect("object path");
    std::fs::write(&object, b"tampered").expect("corrupt");
    let err = resolve_recorded_package(&clean_state, &execution).expect_err("corrupt");
    assert!(matches!(err, ActivityError::KnownFailure(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Deterministic property test over generated package shapes
// ---------------------------------------------------------------------------

/// A small deterministic LCG so the generated cases are reproducible from a
/// fixed seed (no external RNG dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 16
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn yes(&mut self) -> bool {
        self.below(2) == 1
    }
}

#[test]
fn generated_packages_render_their_recorded_sections_or_refuse() {
    const SEED: u64 = 0x5F1_5EED;
    const CASES: u64 = 48;
    let dir = ScratchDir::new("sf-builder-prompt-property");
    let state = dir.child("state");
    let run = dir.child("run");
    std::fs::create_dir_all(run.join("artifacts")).expect("artifacts dir");

    let mut rng = Lcg(SEED);
    for case in 0..CASES {
        let file_count = 1 + rng.below(5) as usize;
        let allowed: Vec<String> = (0..file_count)
            .map(|n| format!("crates/gen{case}/src/file{n}.rs"))
            .collect();
        let has_instruction = rng.yes();
        let non_empty_title = rng.below(4) != 0;
        let marker = format!("CASE-{case}-INSTRUCTION-BODY");

        let spec = Spec {
            task_id: format!("sf.gen.{case}"),
            execution_id: format!("exec:sf.gen.{case}:1"),
            title: if non_empty_title {
                format!("Generated case {case}")
            } else {
                "   ".to_string()
            },
            description: format!("# Scope\nGenerated case {case}.\n\n# Files\n{}\n", allowed.join("\n")),
            design: if rng.yes() {
                format!("DESIGN-{case}")
            } else {
                String::new()
            },
            notes: if rng.yes() {
                format!("NOTES-{case}")
            } else {
                String::new()
            },
            machine_block: String::new(),
            acceptance: (0..1 + rng.below(3))
                .map(|n| format!("cmd-{case}-{n}"))
                .collect(),
            agents_md: has_instruction.then(|| marker.clone()),
            shared_design: rng.yes().then(|| format!("SHARED-{case}")),
            parent_design: rng.yes().then(|| format!("PARENT-{case}")),
            dependency_handoff: rng.yes().then(|| format!("DEP-{case}")),
            allowed: allowed.clone(),
            protected: vec![format!("fixtures/protected/{case}/")],
        };
        let package = build_package(&state, &spec);
        let repo = repo_mode(&spec.title, &["cargo test"]);
        let attempt = 1 + rng.below(2) as u32;

        let result = render(&state, BUILDER_PROMPT, Some(&repo), Some(&package), &run, attempt);

        // Invariant 4: repository mode NEVER renders without a recorded package,
        // regardless of the package's own completeness.
        let missing = render(&state, BUILDER_PROMPT, Some(&repo), None, &run, attempt);
        assert!(
            matches!(missing, Err(ActivityError::KnownFailure(_))),
            "case {case}: repository mode accepted a missing package"
        );

        if !has_instruction || !non_empty_title {
            // Required context is missing: an explicit refusal, never the M0 base.
            let err = result.expect_err("missing required context must refuse");
            assert!(matches!(err, ActivityError::KnownFailure(_)), "case {case}: {err:?}");
            continue;
        }
        let text = result.unwrap_or_else(|err| panic!("case {case}: {err:?}"));

        // Invariant 1: the frozen title is always delivered.
        assert!(text.contains(&spec.title), "case {case}: title missing");
        // Invariant 2: every recorded non-empty section text appears verbatim
        // (the renderer trims only trailing whitespace, so compare trimmed
        // needles).
        for needle in [
            spec.description.trim_end(),
            spec.design.trim_end(),
            spec.notes.trim_end(),
            marker.as_str(),
            spec.acceptance[0].as_str(),
        ]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        {
            assert!(text.contains(needle), "case {case}: missing {needle:?}");
        }
        // Invariant 3: every permitted path appears; the empty optional sections
        // still render their explicit marker without leaking fixture wording.
        for path in &spec.allowed {
            assert!(text.contains(path), "case {case}: missing allowed path {path:?}");
        }
        assert!(!text.contains("one-line"), "case {case}: fixture wording leaked");
    }
}
