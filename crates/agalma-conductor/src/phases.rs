//! M0.7/M0.8 phase activities: the real Git/Seatbelt/OpenCode effects behind
//! the executor's [`agalma_execution::Activity`] seam.
//!
//! ```text
//! intake -> checkout -> build -> verify -> integrate -> done | parked
//! ```
//!
//! - `checkout`: [`WorkspaceApi::prepare`] copies the fixture template, records
//!   the base SHA, and persists the [`Checkout`] to `<run>/checkout.json` so a
//!   restart can reconstruct it.
//! - `build`: one confined OpenCode session in the checkout with the builder
//!   prompt; the model edits the checkout. The launched worker's pid/pgid is
//!   recorded durably (see [`crate::workers`]) so a restart can terminate a
//!   survivor before redispatching.
//! - `verify`: a *separate* confined run; the exit code is the verdict, full
//!   output is captured under `<state>/runs/<exec>/artifacts/`. In repo mode
//!   (M1) the candidate working tree is committed on the candidate branch, a
//!   **pristine** verification checkout is cloned at that SHA under
//!   `<run>/verify-checkout/` (its own Git metadata), and each task acceptance command
//!   runs there as its own bounded launch (`accept@<attempt>-<n>.txt`).
//! - `integrate`: [`WorkspaceApi::integrate`] compare-and-updates `main` and
//!   tags `m0/<exec>`. Recovery detects an already-landed merge from Git state
//!   (`main == candidate`) and completes without re-merging.
//!
//! ## Recovery (M0.8)
//!
//! Each effect phase records its outcome to a durable completion marker
//! (`<run>/<phase>/<attempt>.done`). [`Activity::effect_present`] returns
//! [`EffectProbe::Present`] when that marker exists, so
//! [`Activity::reconcile`] can complete the operation without repeating it.
//! Otherwise a recorded-but-live worker is terminated through [`SandboxApi`]
//! with evidence before the operation is redispatched; a worker that died
//! without completing is an ambiguous effect and parks.
//!
//! ## Scripted mode
//!
//! `AGALMA_ACTIVITY_MODE=scripted` swaps the model turn and the verify command
//! for deterministic, file-backed stand-ins (used by the M0.8 crash/kill
//! matrix). The real checkout and integrate effects still run, so recovery
//! exercises real Git state.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use agalma_contracts::harness::{
    AttemptHandle, Completeness, CreateSessionRequest, Event, HarnessApi, Limits, OperationHandle,
    OperationState, RunTurnRequest, SessionHandle, StartAttemptRequest, Usage,
};
use agalma_contracts::ids::{ArtifactRef, AttemptId};
use agalma_contracts::sandbox::LaunchSpec;
use agalma_contracts::{
    Checkout, ContextItem, ContextKind, ContractError, DecisionApi, DecisionAttempt, DecisionKind,
    DecisionOption, DecisionPin, DecisionRequest, LedgerApi, OperationId, TaskPackage,
    WorkspaceApi, DECISION_API_VERSION,
};
use agalma_decision::{StaticDecider, RETRY_ESCALATE_BASELINE};
use agalma_execution::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, OperationContext,
};
use agalma_harness_opencode::{build_config, OpenCodeConfigInit};
use agalma_ipc::WorkspaceOps;
use agalma_ledger::SqliteLedger;
use agalma_sandbox::{SandboxOutput, SeatbeltSandbox};
use serde_json::json;

use crate::harness_binding::{self, HarnessHandle, HarnessKind};
use crate::harness_process::{self, HarnessTransport, LiveHarness, OpenCodeHarnessInit};
use crate::sandbox_process::SandboxSource;
use crate::scope_guard::{self, CandidatePin, DeniedChange, ScopeEvidence, ScopePolicy};
use crate::stop::StopControl;
use crate::workers::{self, Survivor};
use crate::workspace_process::WorkspaceHandle;

/// The verifier prompt shipped with the conductor. A fresh verifier session is
/// created from this text on a red verify; the per-attempt prompt appends the
/// concrete input/output artifact paths.
pub const VERIFIER_PROMPT: &str = include_str!("../prompts/verifier.md");

/// Environment variable selecting the activity implementation set.
pub const ACTIVITY_MODE_ENV: &str = "AGALMA_ACTIVITY_MODE";
/// Environment variable controlling the scripted build's in-process sleep (ms).
pub const SCRIPTED_SLEEP_ENV: &str = "AGALMA_SCRIPTED_BUILD_SLEEP_MS";

/// Which implementation set the composition root wires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityMode {
    /// Real OpenCode harness + confined `cargo test`.
    Live,
    /// Deterministic file-backed build/verify stand-ins (crash-matrix tests).
    Scripted,
}

impl ActivityMode {
    /// Read the mode from [`ACTIVITY_MODE_ENV`] (`scripted` selects scripted).
    pub fn from_env() -> Self {
        match std::env::var(ACTIVITY_MODE_ENV).ok().as_deref() {
            Some("scripted") => ActivityMode::Scripted,
            _ => ActivityMode::Live,
        }
    }
}

/// Mutable state shared between phase activities for one run.
#[derive(Default)]
pub struct RunState {
    /// The checkout prepared by the checkout phase.
    pub checkout: Option<Checkout>,
}

impl RunState {
    /// Return the checkout, reconstructing it from the durable artifact when the
    /// in-memory copy is missing (a restart).
    pub fn ensure_checkout(
        &mut self,
        state_dir: &Path,
        execution: &agalma_contracts::ExecutionId,
    ) -> Result<Checkout, ActivityError> {
        if let Some(checkout) = &self.checkout {
            return Ok(checkout.clone());
        }
        let path = checkout_artifact_path(state_dir, execution);
        let checkout: Checkout = workers::read_json(&path).ok_or_else(|| {
            ActivityError::KnownFailure(format!(
                "checkout artifact missing or unreadable: {}",
                path.display()
            ))
        })?;
        self.checkout = Some(checkout.clone());
        Ok(checkout)
    }
}

/// Immutable wiring the activities need.
#[derive(Clone)]
pub struct PhaseDeps {
    pub state_dir: PathBuf,
    pub fixture: PathBuf,
    /// Resolved builder model (H3 route), pinned for builder attempts.
    pub model: String,
    /// Resolved verifier/diagnosis model (H3 route), pinned for verifier turns.
    pub verifier_model: String,
    /// Route label for the builder model (`<class>:<family>`, H3).
    pub route: String,
    /// Route label for the verifier model (H3).
    pub verifier_route: String,
    pub role: String,
    /// Rendered, fail-closed sandbox profile.
    pub profile_path: PathBuf,
    /// Builder prompt file (path passed to the harness as `prompts_ref`).
    pub builder_prompt: PathBuf,
    /// Parent-owned provider proxy URL handed to the confined harness.
    pub proxy_url: Option<String>,
    /// Verifier-role proxy URL (`agalma-c31`). When the paid path pins a
    /// per-role gate, the verifier session runs through its own proxy so its
    /// spend is attributed to the verifier, not the builder. `None` keeps the
    /// builder/`proxy_url` for the verifier (historical behavior).
    pub verifier_proxy_url: Option<String>,
    /// Whether the paid provider proxy path is active (credential present +
    /// `AGALMA_ALLOW_PAID`). When true the confined harness config points the
    /// provider at the loopback proxy with a dummy key (`agalma-csw.2`).
    pub paid_proxy: bool,
    /// Read-only toolchain roots the confined worker/verify need (`~/.cargo`,
    /// `~/.rustup`).
    pub extra_ro_roots: Vec<PathBuf>,
    /// Toolchain environment for the confined worker (`RUSTUP_HOME`,
    /// `RUSTUP_TOOLCHAIN`, `PATH`).
    pub toolchain_env: BTreeMap<String, String>,
    /// Bound on one model turn.
    pub turn_timeout: Duration,
    /// Bound on the verify command.
    pub verify_timeout: Duration,
    /// Progress watchdog bound: a session with no token/state delta for this
    /// long is cancelled and classified `stalled` (H1).
    pub idle_timeout: Duration,
    /// Repo mode (M1): when set, checkout clones `origin` at `base_ref` and
    /// verify runs the task's acceptance commands instead of `cargo test`.
    pub repo: Option<RepoMode>,
    /// Harness binding selection (M1.7). Activities resolve the pinned binding
    /// for an operation, else the current selection, at attempt start.
    pub harness: HarnessHandle,
    /// Shared bound stop control (`agalma-5fy.5`): the same control the caller
    /// used for admission, reused by the live turn polls so a phase poll reads
    /// the selected ledger binding rather than a second database.
    pub stop: Rc<dyn StopControl>,
}

/// Repo-mode parameters carried per execution (M1 queue-driven conductor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMode {
    /// Origin repository cloned at checkout and integrated into at integrate.
    pub origin: PathBuf,
    /// Base ref the candidate branches from (`main`).
    pub base_ref: String,
    /// Task title, rendered into the per-attempt builder prompt.
    pub task_title: String,
    /// Acceptance commands run by Rust, each as its own launch.
    pub acceptance: Vec<String>,
}

/// Shared handles for one executor.
pub struct PhaseActivities {
    pub intake: Box<dyn Activity>,
    pub checkout: Box<dyn Activity>,
    pub build: Box<dyn Activity>,
    pub verify: Box<dyn Activity>,
    pub integrate: Box<dyn Activity>,
}

/// Build the five phase activities, selecting the implementation set from the
/// environment ([`ActivityMode::from_env`]).
pub fn activities(
    deps: PhaseDeps,
    workspace: Rc<RefCell<WorkspaceHandle>>,
    state: Rc<RefCell<RunState>>,
) -> PhaseActivities {
    activities_with_mode(deps, workspace, state, ActivityMode::from_env())
}

/// Build the five phase activities over shared state with an explicit mode.
pub fn activities_with_mode(
    deps: PhaseDeps,
    workspace: Rc<RefCell<WorkspaceHandle>>,
    state: Rc<RefCell<RunState>>,
    mode: ActivityMode,
) -> PhaseActivities {
    let sandbox = SandboxSource::new(deps.state_dir.clone());
    let build: Box<dyn Activity> = Box::new(BuildActivity {
        deps: deps.clone(),
        state: Rc::clone(&state),
        mode,
        sandbox: sandbox.clone(),
    });
    // Repo mode always runs the real acceptance runner (`VerifyActivity`); only
    // the model call is replaced in scripted mode. The `sandboxed` flag selects
    // Seatbelt (`Live`) vs. an in-process launch (`Scripted`, tests).
    let verify: Box<dyn Activity> = if deps.repo.is_some() {
        Box::new(VerifyActivity {
            deps: deps.clone(),
            workspace: Rc::clone(&workspace),
            state: Rc::clone(&state),
            sandboxed: mode == ActivityMode::Live,
            scripted: mode == ActivityMode::Scripted,
            sandbox: sandbox.clone(),
        })
    } else {
        match mode {
            ActivityMode::Live => Box::new(VerifyActivity {
                deps: deps.clone(),
                workspace: Rc::clone(&workspace),
                state: Rc::clone(&state),
                sandboxed: true,
                scripted: false,
                sandbox: sandbox.clone(),
            }),
            ActivityMode::Scripted => Box::new(ScriptedVerifyActivity {
                deps: deps.clone(),
                state: Rc::clone(&state),
            }),
        }
    };
    PhaseActivities {
        intake: Box::new(NoopActivity),
        checkout: Box::new(CheckoutActivity {
            deps: deps.clone(),
            workspace: Rc::clone(&workspace),
            state: Rc::clone(&state),
        }),
        build,
        verify,
        integrate: Box::new(IntegrateActivity {
            deps,
            workspace,
            state,
        }),
    }
}

/// `<state>/runs/<exec>` — the execution-scoped root shared by all phases.
///
/// Uses the same filesystem-safe name as [`agalma_workspace`]'s checkout so the
/// harness/verify `ATTEMPT` root contains the checkout, and so the path is free
/// of the `:` that breaks cargo's `DYLD_FALLBACK_LIBRARY_PATH`.
pub fn run_root(state_dir: &Path, execution: &agalma_contracts::ExecutionId) -> PathBuf {
    state_dir
        .join("runs")
        .join(agalma_workspace::execution_dir_name(execution))
}

/// `<run>/checkout.json` — the durable [`Checkout`] handle.
fn checkout_artifact_path(state_dir: &Path, execution: &agalma_contracts::ExecutionId) -> PathBuf {
    run_root(state_dir, execution).join("checkout.json")
}

/// `<run>/effects.log` — scripted-mode effect ledger (crash-matrix evidence).
pub fn effects_log_path(state_dir: &Path, execution: &agalma_contracts::ExecutionId) -> PathBuf {
    run_root(state_dir, execution).join("effects.log")
}

/// `<state>/scope-pins/<exec>.json` — the immutable verification pin written by
/// a green repo-mode verify and revalidated before integration (SF.6).
///
/// Deliberately **outside** `<state>/runs/<exec>` (the sandbox `ATTEMPT`,
/// which the child may write): a read-only `PROTECTED` allowance does not revoke
/// the parent writable grant over `ATTEMPT`, so the pin must live outside all
/// child writable roots.
pub fn scope_pin_path(state_dir: &Path, execution: &agalma_contracts::ExecutionId) -> PathBuf {
    state_dir.join("scope-pins").join(format!(
        "{}.json",
        agalma_workspace::execution_dir_name(execution)
    ))
}

/// `<state>/scope-evidence/<exec>@<attempt>.json` — scope/oracle evidence for
/// one verify attempt (outside `<state>/runs/<exec>`).
fn scope_evidence_path(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
    attempt: u32,
) -> PathBuf {
    state_dir.join("scope-evidence").join(format!(
        "{}@{attempt}.json",
        agalma_workspace::execution_dir_name(execution)
    ))
}

/// `<run>/build-runtime` — the builder child's narrow sandbox `ATTEMPT` root.
///
/// SF.6: parent markers/receipts live under `<run>/<phase>/`, outside this
/// narrow root, so a confined builder cannot overwrite a parent completion or
/// worker marker. The checkout the model edits is granted separately.
pub fn builder_runtime_dir(state_dir: &Path, execution: &agalma_contracts::ExecutionId) -> PathBuf {
    run_root(state_dir, execution).join("build-runtime")
}

/// `<run>/verify-runtime` — the verifier child's narrow sandbox `ATTEMPT` root
/// (shared by the acceptance run and the live diagnosis session).
pub fn verify_runtime_dir(state_dir: &Path, execution: &agalma_contracts::ExecutionId) -> PathBuf {
    run_root(state_dir, execution).join("verify-runtime")
}

/// Production builder child config (SF.6).
///
/// The sandbox `ATTEMPT` is the narrow `build-runtime`; the whole run root is
/// granted **read-only** via `protected_dir` so the builder can read its prompt
/// and the retry context without granting `<run>` writable; the checkout is the
/// only extra writable root. All supplied RO roots are preserved.
pub fn builder_config_init(
    deps: &PhaseDeps,
    checkout: &Checkout,
    execution: &agalma_contracts::ExecutionId,
) -> Result<OpenCodeConfigInit, ActivityError> {
    let root = run_root(&deps.state_dir, execution);
    std::fs::create_dir_all(&root)
        .map_err(|e| ActivityError::KnownFailure(format!("create run root: {e}")))?;
    let runtime = canonical_dir(&builder_runtime_dir(&deps.state_dir, execution))?;
    let protected = canonical_dir(&root)?;
    let sock = canonical_dir(&runtime.join("sock"))?;
    let mut env = deps.toolchain_env.clone();
    env.insert(
        "CARGO_HOME".to_string(),
        ensure_cargo_home(&runtime)?.to_string_lossy().into_owned(),
    );
    // Keep build artifacts out of the checkout so the candidate commit is the
    // source change, not a multi-megabyte `target/` tree.
    env.insert(
        "CARGO_TARGET_DIR".to_string(),
        runtime.join("agent-target").to_string_lossy().into_owned(),
    );
    Ok(OpenCodeConfigInit {
        profile_path: deps.profile_path.clone(),
        attempt_dir: runtime,
        protected_dir: protected,
        sock_dir: sock,
        model: deps.model.clone(),
        proxy_url: deps.proxy_url.clone(),
        paid_proxy: deps.paid_proxy,
        extra_ro_roots: deps.extra_ro_roots.clone(),
        extra_rw_roots: vec![PathBuf::from(&checkout.path)],
        extra_env: env,
    })
}

/// Production verifier-diagnosis child config (SF.6).
///
/// The sandbox `ATTEMPT` is the narrow `verify-runtime`; the run root is granted
/// **read-only** via `protected_dir`, so the diagnosis child can read the
/// checkout and the diff/failure inputs, and the prompt, without any parent
/// marker being writable and without consuming an `EXTRA_RO` slot. All supplied
/// RO roots are preserved (no truncation).
pub fn verifier_config_init(
    deps: &PhaseDeps,
    _checkout: &Checkout,
    execution: &agalma_contracts::ExecutionId,
) -> Result<OpenCodeConfigInit, ActivityError> {
    let root = run_root(&deps.state_dir, execution);
    let runtime = canonical_dir(&verify_runtime_dir(&deps.state_dir, execution))?;
    let protected = canonical_dir(&root)?;
    let sock = canonical_dir(&runtime.join("sock"))?;
    Ok(OpenCodeConfigInit {
        profile_path: deps.profile_path.clone(),
        attempt_dir: runtime,
        protected_dir: protected,
        sock_dir: sock,
        model: deps.verifier_model.clone(),
        proxy_url: deps
            .verifier_proxy_url
            .clone()
            .or_else(|| deps.proxy_url.clone()),
        paid_proxy: deps.paid_proxy,
        extra_ro_roots: deps.extra_ro_roots.clone(),
        extra_rw_roots: Vec::new(),
        extra_env: deps.toolchain_env.clone(),
    })
}

/// `<state>/protected/<exec>` — the read-only protected baseline root.
///
/// Outside `<state>/runs/<exec>` (the candidate/child writable tree), so a
/// protected command running under the sandbox cannot write the judge or the
/// parent pin state.
pub fn protected_root(state_dir: &Path, execution: &agalma_contracts::ExecutionId) -> PathBuf {
    state_dir
        .join("protected")
        .join(agalma_workspace::execution_dir_name(execution))
}

/// The scope/protected-baseline result for one candidate verify attempt (SF.6).
struct CandidateGuard {
    evidence: ScopeEvidence,
    denial: bool,
    /// Missing/invalid required inputs `park` (refuse); a scope denial is `red`
    /// (red retry, which exhausts to a park).
    park: bool,
    /// Why it was refused (denied paths, invalid policy, missing baseline).
    reason: String,
    /// The validated protected baseline (files + commands), when authorized.
    baseline: Option<scope_guard::ProtectedBaseline>,
}

/// Authorize the candidate change set and materialize the immutable protected
/// baseline (SF.6).
///
/// Always returns `Ok(Some(..))`: repository verification is **fail-closed**. A
/// missing recorded package, an invalid/unenforced policy, or a missing/opaque
/// baseline all refuse (park) rather than running an unprotected judge. The
/// admission source for these inputs is the operator `ProtectedInputs/v1`
/// manifest (`<state_dir>/protected-inputs.json`) frozen whole into the
/// package's `scope.oracle`; M0 fixture mode (`repo == None`) does not reach
/// this seam and stays a separate path.
///
/// - an **invalid recorded policy** refuses (no fail-open);
/// - the candidate's typed change set is authorized against the frozen scope
///   with **real symlink resolution** against the pinned candidate tree (deny
///   wins);
/// - a valid baseline's files are materialized into a **separate read-only
///   protected root outside the candidate tree** (never restored into the
///   candidate), so the *frozen* acceptance — run through `SandboxApi` in the
///   acceptance path, with the protected root as the read-only judge — is the
///   authority.
impl VerifyActivity {
    fn guard_candidate(
        &self,
        ctx: &OperationContext,
        checkout: &Checkout,
        verify_dir: &Path,
        candidate_sha: &str,
        package: Option<&agalma_contracts::TaskPackage>,
    ) -> Result<Option<CandidateGuard>, ActivityError> {
        // Repository verification is fail-closed: a missing recorded package,
        // an unenforced (empty) policy, or a missing/invalid protected baseline
        // all REFUSE (park) rather than running an unprotected judge. The inputs
        // come from the operator `ProtectedInputs/v1` manifest frozen into the
        // package's `scope.oracle`; M0 fixture mode (`repo == None`) does not
        // reach this seam and stays separate.
        let Some(package) = package else {
            return Ok(Some(CandidateGuard {
                evidence: ScopeEvidence {
                    candidate_sha: candidate_sha.to_string(),
                    package_digest: String::new(),
                    oracle_digest: None,
                    baseline_files: 0,
                    denied_paths: Vec::new(),
                },
                baseline: None,
                denial: true,
                park: true,
                reason: "repository verification requires a recorded TaskPackage; none is admitted"
                    .to_string(),
            }));
        };
        let mut evidence = ScopeEvidence {
            candidate_sha: candidate_sha.to_string(),
            package_digest: package.package_digest.clone(),
            oracle_digest: None,
            baseline_files: 0,
            denied_paths: Vec::new(),
        };

        // Finding 4: an invalid recorded policy refuses before any effect.
        let policy = match ScopePolicy::from_scope(&package.scope) {
            Ok(policy) => policy,
            Err(err) => {
                evidence.denied_paths.push(DeniedChange {
                    path: err.entry.clone(),
                    code: "invalid_policy".to_string(),
                    detail: err.detail.clone(),
                });
                return Ok(Some(CandidateGuard {
                    evidence,
                    baseline: None,
                    denial: true,
                    park: true,
                    reason: format!("invalid recorded scope policy: {err}"),
                }));
            }
        };

        // Finding 1: an unenforced (empty) scope is NOT permission to run
        // unprotected repository verification; it refuses.
        if !policy.enforced() {
            return Ok(Some(CandidateGuard {
                evidence,
                baseline: None,
                denial: true,
                park: true,
                reason: "repository verification has no enforced scope policy; refusing to run \
                         an unprotected judge"
                    .to_string(),
            }));
        }

        let changes = self
            .workspace
            .borrow()
            .changes(checkout)
            .map_err(contract_err)?;
        let report = policy.authorize_in_tree(&changes, verify_dir);
        evidence.denied_paths = report.denied.clone();
        if report.is_denied() {
            return Ok(Some(CandidateGuard {
                evidence,
                baseline: None,
                denial: true,
                park: false,
                reason: scope_failure_text(&report.denied),
            }));
        }

        // A protected package requires a valid frozen protected entry + baseline;
        // a missing/invalid one refuses rather than silently skipping the judge.
        let (bytes, entry) =
            scope_guard::entry_from_artifact(&self.deps.state_dir, &package.scope.oracle)
                .map_err(contract_err)?;
        let baseline = entry.baseline.clone();
        evidence.oracle_digest = Some(agalma_contracts::digest::sha256_digest(&bytes));
        evidence.baseline_files = baseline.files.len();

        // Materialize the read-only protected root OUTSIDE the candidate/child
        // writable tree; the sole judge must never live in a candidate-writable
        // tree.
        let protected_root = protected_root(&self.deps.state_dir, &ctx.execution_id);
        scope_guard::materialize_baseline(&protected_root, &baseline).map_err(contract_err)?;

        Ok(Some(CandidateGuard {
            evidence,
            denial: false,
            park: false,
            reason: String::new(),
            baseline: Some(baseline),
        }))
    }
}

/// Render the red-verdict failure text for a denied scope.
fn scope_failure_text(denied: &[DeniedChange]) -> String {
    let mut out = format!("candidate scope denied ({} path(s)):\n", denied.len());
    for denial in denied {
        out.push_str(&format!(
            "- {} ({}) {}\n",
            denial.path, denial.code, denial.detail
        ));
    }
    out
}

/// Create `path` if needed and return its canonical form (Seatbelt parameters
/// must be absolute, resolved paths).
fn canonical_dir(path: &Path) -> Result<PathBuf, ActivityError> {
    std::fs::create_dir_all(path)
        .map_err(|e| ActivityError::KnownFailure(format!("mkdir {}: {e}", path.display())))?;
    std::fs::canonicalize(path)
        .map_err(|e| ActivityError::KnownFailure(format!("resolve {}: {e}", path.display())))
}

/// Maximum Unix socket path length (`SUN_LEN`) we will address directly. The
/// platform limit is 104 bytes on macOS; a path must be strictly shorter.
pub(crate) const SUN_LEN_LIMIT: usize = 104;

/// A component-socket path for `name` under `state_dir` that always fits
/// `SUN_LEN`.
///
/// Unix socket `bind`/`connect` paths are capped at `SUN_LEN` (~104 bytes).
/// `state_dir` can be arbitrarily long (test scratch dirs especially), so a
/// raw `<state_dir>/<name>` path can overflow and the server fails to bind.
/// When a direct path fits we keep it (no behavior change). When it does not,
/// the socket is addressed through a short symlink alias of the state dir under
/// a bounded directory: the socket file still lands in the state dir (so
/// discovery, liveness/watch, and cleanup are unchanged), but the path stays
/// within the limit. Alias construction failure falls back to the direct path
/// (the caller reports the bind error, unchanged semantics).
pub(crate) fn bounded_socket_path(state_dir: &Path, name: &str) -> PathBuf {
    let direct = state_dir.join(name);
    if direct.as_os_str().len() < SUN_LEN_LIMIT {
        return direct;
    }
    match bounded_socket_alias(state_dir) {
        Ok(alias) => alias.join(name),
        // Fall back to the direct path: the caller reports the bind error.
        Err(_) => direct,
    }
}

/// A harness server socket path for `state_dir` that always fits `SUN_LEN`.
///
/// `InProcess` never binds, so it keeps the direct path; `Process` goes through
/// the shared bounded scheme ([`bounded_socket_path`]).
fn harness_socket_path(state_dir: &Path, transport: HarnessTransport, name: &str) -> PathBuf {
    match transport {
        HarnessTransport::InProcess => state_dir.join(name),
        HarnessTransport::Process => bounded_socket_path(state_dir, name),
    }
}

/// A short, bounded symlink alias to `state_dir` for socket addressing.
///
/// The alias lives at `<temp>/agalma-sock/<hash>` where `hash` is a stable
/// digest of the state dir, so its length is independent of how long
/// `state_dir` is and every run gets a distinct bounded base. Binding through
/// the alias creates the socket in `state_dir`, so the on-disk layout is
/// unchanged.
pub(crate) fn bounded_socket_alias(state_dir: &Path) -> std::io::Result<PathBuf> {
    let root = std::env::temp_dir().join("agalma-sock");
    std::fs::create_dir_all(&root)?;
    let alias = root.join(short_hash(&state_dir.to_string_lossy()));
    match std::fs::symlink_metadata(&alias) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(&alias)
                .map(|target| target == state_dir)
                .unwrap_or(false)
            {
                return Ok(alias);
            }
            std::fs::remove_file(&alias)?;
        }
        Ok(_) => {
            std::fs::remove_file(&alias)?;
        }
        Err(_) => {}
    }
    std::os::unix::fs::symlink(state_dir, &alias)?;
    Ok(alias)
}

/// FNV-1a 8-hex digest, used for bounded directory names.
pub(crate) fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash & 0xffff_ffff)
}

fn contract_err(err: ContractError) -> ActivityError {
    match err {
        ContractError::UnknownOutcome(message) => ActivityError::UnknownOutcome(message),
        other => ActivityError::KnownFailure(other.to_string()),
    }
}

/// Run one acceptance command in-process (scripted/deterministic tests only).
/// Mirrors the Seatbelt one-shot contract: captured exit code and output.
fn run_in_process(
    command: &str,
    cwd: &str,
    env: &BTreeMap<String, String>,
) -> Result<SandboxOutput, ActivityError> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .output()
        .map_err(|e| ActivityError::KnownFailure(format!("run acceptance `{command}`: {e}")))?;
    Ok(SandboxOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `intake` is a no-op that hands off to `checkout`.
struct NoopActivity;

impl Activity for NoopActivity {
    fn execute(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        Ok(ActivityOutcome {
            result: json!({ "phase": "intake" }),
            next: ActivityNext::Advance,
        })
    }

    fn effect_present(&mut self, _ctx: &OperationContext) -> EffectProbe {
        EffectProbe::Absent
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        self.execute(ctx)
    }
}

/// `checkout`: prepare the isolated git checkout and record the base SHA.
struct CheckoutActivity {
    deps: PhaseDeps,
    workspace: Rc<RefCell<WorkspaceHandle>>,
    state: Rc<RefCell<RunState>>,
}

fn checkout_outcome(checkout: &Checkout) -> ActivityOutcome {
    ActivityOutcome {
        result: json!({
            "base_sha": checkout.base_sha,
            "path": checkout.path,
            "candidate_branch": checkout.candidate_branch,
        }),
        next: ActivityNext::Advance,
    }
}

impl Activity for CheckoutActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = match &self.deps.repo {
            Some(repo) => self
                .workspace
                .borrow_mut()
                .prepare_repo(
                    &repo.origin,
                    &repo.base_ref,
                    &ctx.execution_id,
                    &self.deps.state_dir,
                )
                .map_err(contract_err)?,
            None => {
                let template = self.deps.fixture.to_string_lossy().into_owned();
                self.workspace
                    .borrow_mut()
                    .prepare(&template, &ctx.execution_id)
                    .map_err(contract_err)?
            }
        };
        workers::write_json(
            &checkout_artifact_path(&self.deps.state_dir, &ctx.execution_id),
            &checkout,
        )?;
        self.state.borrow_mut().checkout = Some(checkout.clone());
        Ok(checkout_outcome(&checkout))
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        let artifact = checkout_artifact_path(&self.deps.state_dir, &ctx.execution_id);
        if artifact.exists() {
            EffectProbe::Present
        } else if run_root(&self.deps.state_dir, &ctx.execution_id)
            .join("checkout")
            .is_dir()
        {
            // A partially-prepared checkout cannot be trusted.
            EffectProbe::Ambiguous
        } else {
            EffectProbe::Absent
        }
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let artifact = checkout_artifact_path(&self.deps.state_dir, &ctx.execution_id);
        let checkout: Checkout = workers::read_json(&artifact).ok_or_else(|| {
            ActivityError::KnownFailure(format!(
                "checkout reconcile: unreadable {}",
                artifact.display()
            ))
        })?;
        self.state.borrow_mut().checkout = Some(checkout.clone());
        Ok(checkout_outcome(&checkout))
    }
}

// ---------------------------------------------------------------------------
// build (live)
// ---------------------------------------------------------------------------

/// `build`: one confined OpenCode session that fixes the checkout.
struct BuildActivity {
    deps: PhaseDeps,
    state: Rc<RefCell<RunState>>,
    /// Live (Seatbelt/OpenCode) or scripted/deterministic implementation set.
    mode: ActivityMode,
    /// Lazily-started sandbox supervisor backing confined OpenCode launches.
    sandbox: SandboxSource,
}

/// Probe a build attempt via the durable worker/done markers.
fn build_probe(
    deps: &PhaseDeps,
    execution: &agalma_contracts::ExecutionId,
    attempt: u32,
) -> EffectProbe {
    if workers::read_done(&deps.state_dir, execution, "build", attempt).is_some() {
        return EffectProbe::Present;
    }
    let mut sandbox = SeatbeltSandbox::new();
    match workers::reconcile_survivor(&deps.state_dir, execution, "build", attempt, &mut sandbox) {
        Ok(Survivor::Terminated) | Ok(Survivor::AlreadyReconciled) => EffectProbe::Absent,
        Ok(Survivor::Died) => EffectProbe::Ambiguous,
        Ok(Survivor::NoRecord) => {
            if workers::started_exists(&deps.state_dir, execution, "build", attempt) {
                EffectProbe::Ambiguous
            } else {
                EffectProbe::Absent
            }
        }
        Err(err) => {
            eprintln!("agalma: build survivor reconciliation failed: {err}");
            EffectProbe::Ambiguous
        }
    }
}

/// Complete a build attempt from its durable completion marker.
fn build_reconcile(
    deps: &PhaseDeps,
    execution: &agalma_contracts::ExecutionId,
    attempt: u32,
) -> Result<ActivityOutcome, ActivityError> {
    workers::read_done(&deps.state_dir, execution, "build", attempt).ok_or_else(|| {
        ActivityError::KnownFailure(format!(
            "build reconcile: no completion marker for attempt {attempt}"
        ))
    })
}

impl BuildActivity {
    /// Resolve the harness kind for this operation: a pinned binding wins;
    /// otherwise the current selection is resolved (rebind is allowed only
    /// between operations/attempts).
    fn resolve_kind(&self, ctx: &OperationContext) -> Result<HarnessKind, ActivityError> {
        let ledger =
            SqliteLedger::open(self.deps.state_dir.join("ledger.sqlite")).map_err(contract_err)?;
        if let Some(record) =
            harness_binding::read_pinned_binding(&ledger, &ctx.execution_id, &ctx.operation_id)
                .map_err(contract_err)?
        {
            return HarnessKind::from_implementation(&record.implementation).ok_or_else(|| {
                ActivityError::KnownFailure(format!(
                    "pinned harness {:?} is not a known implementation",
                    record.implementation
                ))
            });
        }
        Ok(self.deps.harness.borrow().kind())
    }

    /// Persist the binding for this operation exactly once (no-op when already
    /// pinned): the component-registry row plus the per-operation pin event.
    fn persist_harness_binding(
        &self,
        ctx: &OperationContext,
        kind: HarnessKind,
    ) -> Result<(), ActivityError> {
        let mut ledger =
            SqliteLedger::open(self.deps.state_dir.join("ledger.sqlite")).map_err(contract_err)?;
        if harness_binding::read_pinned_binding(&ledger, &ctx.execution_id, &ctx.operation_id)
            .map_err(contract_err)?
            .is_some()
        {
            return Ok(());
        }
        let generation = ledger
            .execution(&ctx.execution_id)
            .map_err(contract_err)?
            .map(|record| record.generation)
            .unwrap_or(1);
        let plan = harness_binding::plan_binding(kind, generation, &self.deps.model)
            .map_err(contract_err)?;
        if let Some(fallback) = plan.fallback_plan() {
            eprintln!(
                "agalma: harness {} missing optional capabilities [{}]; declared fallback {}",
                kind.as_str(),
                plan.fallback.join(","),
                fallback
            );
        }
        harness_binding::persist_binding(
            &mut ledger,
            &ctx.execution_id,
            &ctx.operation_id,
            ctx.attempt,
            &plan,
        )
        .map_err(contract_err)
    }

    /// Live OpenCode build: one confined session whose model edits the checkout.
    fn run_live(&self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        let root = run_root(&self.deps.state_dir, &ctx.execution_id);
        std::fs::create_dir_all(&root)
            .map_err(|e| ActivityError::KnownFailure(format!("create run root: {e}")))?;
        // Parameterize the base builder prompt per attempt: the model sees the
        // task title, the acceptance commands, and the captured failure text (or
        // a verifier diagnosis artifact when M1.3 writes one).
        let prompt_path = write_builder_prompt(&self.deps, &root, ctx)?;

        // Production-produced builder config (shared helper, SF.6): narrow
        // ATTEMPT runtime, run root read-only (PROTECTED), the checkout the only
        // extra writable root.
        let config = build_config(builder_config_init(
            &self.deps,
            &checkout,
            &ctx.execution_id,
        )?);

        let sandbox_socket = self.sandbox.ensure().map_err(contract_err)?;
        let transport = HarnessTransport::from_env();
        let harness_socket = harness_socket_path(
            &self.deps.state_dir,
            transport,
            &format!("harness-build-{}.sock", ctx.attempt),
        );
        let mut harness = harness_process::open_opencode_harness(OpenCodeHarnessInit {
            transport,
            config,
            root: root.clone(),
            model: self.deps.model.clone(),
            generation: 1,
            sandbox_socket,
            harness_socket,
        })
        .map_err(ActivityError::KnownFailure)?;
        let attempt = harness
            .start_attempt(StartAttemptRequest {
                workspace: checkout.path.clone(),
                role: self.deps.role.clone(),
                genome_ref: ArtifactRef::derive("genome").to_string(),
                constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
                limits: Limits {
                    wall_ms: self.deps.turn_timeout.as_millis() as u64,
                    tokens: 0,
                    cost_micros: 0,
                },
                operation_id: ctx.operation_id.clone(),
            })
            .map_err(contract_err)?;
        if let Some(pid) = harness.attempt_pid() {
            workers::record_worker(
                &self.deps.state_dir,
                &ctx.execution_id,
                "build",
                ctx.attempt,
                ctx.operation_id.as_str(),
                ctx.lease_generation,
                pid,
                pid,
            )?;
        }
        mark_started(&self.deps, &ctx.execution_id, "build", ctx.attempt)?;

        let started = Instant::now();
        let outcome = self.drive(
            &mut harness,
            &attempt,
            ctx,
            &prompt_path,
            HarnessKind::OpenCode.as_str(),
        );
        let wall_ms = started.elapsed().as_millis() as u64;
        let evidence = harness.stop_attempt(&attempt);
        if let Ok(evidence) = &evidence {
            if !evidence.process_group_gone {
                eprintln!("agalma: build attempt cleanup: {}", evidence.detail);
            }
        }
        if let Ok(outcome) = &outcome {
            // Per-attempt cost: attribute the turn's usage to this build attempt.
            let usage = usage_from_result(&outcome.result);
            record_usage_event(
                &self.deps.state_dir,
                &ctx.execution_id,
                &ctx.operation_id,
                "build",
                &self.deps.role,
                &self.deps.model,
                &self.deps.route,
                ctx.attempt,
                usage,
                wall_ms,
            )?;
            workers::write_done(
                &self.deps.state_dir,
                &ctx.execution_id,
                "build",
                ctx.attempt,
                outcome,
            )?;
        }
        workers::clear_worker(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
        );
        outcome
    }

    /// Record a `provider_health` event for a stalled/failed session (best
    /// effort: observability must not fail the phase).
    fn note_provider_health(&self, ctx: &OperationContext, binding: &str, stall: &PollStall) {
        if let Err(err) = record_provider_health(
            &self.deps.state_dir,
            &ctx.execution_id,
            &self.deps.model,
            binding,
            ctx.attempt,
            stall,
        ) {
            eprintln!("agalma: provider_health for {}: {err}", ctx.execution_id);
        }
    }

    /// Record the pinned static `retry.escalate` decision for a `stalled`
    /// attempt (a failed verdict: retry while attempts remain, else park).
    fn note_stalled_retry_decision(&self, ctx: &OperationContext) {
        match execution_max_attempts(&self.deps.state_dir, &ctx.execution_id) {
            Ok(max_attempts) => {
                if let Err(err) = record_retry_decision(
                    &self.deps.state_dir,
                    &ctx.execution_id,
                    ctx.attempt,
                    max_attempts,
                ) {
                    eprintln!(
                        "agalma: retry.escalate for stalled {}: {err}",
                        ctx.execution_id
                    );
                }
            }
            Err(err) => eprintln!(
                "agalma: max_attempts for stalled {}: {err}",
                ctx.execution_id
            ),
        }
    }

    fn drive(
        &self,
        harness: &mut LiveHarness,
        _attempt: &AttemptHandle,
        ctx: &OperationContext,
        prompt_path: &Path,
        binding: &str,
    ) -> Result<ActivityOutcome, ActivityError> {
        let session = harness
            .create_session(CreateSessionRequest {
                role: self.deps.role.clone(),
                model: self.deps.model.clone(),
                prompts_ref: prompt_path.to_string_lossy().into_owned(),
                handoff_refs: Vec::new(),
                tool_policy_ref: ArtifactRef::derive("tool-policy").to_string(),
            })
            .map_err(contract_err)?;
        let op = harness
            .run_turn(
                &session,
                RunTurnRequest {
                    input_ref: ArtifactRef::derive("builder-turn"),
                    bounded_turns: 1,
                    deadline_ms: self.deps.turn_timeout.as_millis() as u64,
                },
            )
            .map_err(contract_err)?;
        let outcome = poll_terminal(
            harness,
            &op,
            self.deps.idle_timeout,
            self.deps.turn_timeout,
            Some(&*self.deps.stop),
        );
        // Cancel before close: the interrupt must reach a live session, and a
        // 404 means it already ended (benign). A terminal outcome needs no
        // cancel.
        settle_after_poll(harness, &op, &session, &outcome, binding);

        let failure = ArtifactRef::derive(&format!("build@{}", ctx.attempt));
        match outcome {
            PollOutcome::Terminal(OperationState::Completed { usage, .. }) => Ok(ActivityOutcome {
                result: json!({
                    "status": "completed",
                    "attempt": ctx.attempt,
                    "tokens_in": usage.tokens_in,
                    "tokens_out": usage.tokens_out,
                    "cost_usd": usage.cost_usd,
                    "completeness": usage.completeness.label(),
                }),
                next: ActivityNext::Advance,
            }),
            PollOutcome::Terminal(OperationState::Failed { reason }) => {
                self.note_provider_health(ctx, binding, &PollStall::failed(&reason, 0, 0));
                eprintln!("agalma: build attempt {} failed: {reason}", ctx.attempt);
                Ok(ActivityOutcome {
                    result: json!({ "status": "failed", "reason": reason, "attempt": ctx.attempt }),
                    next: ActivityNext::Retry { failure },
                })
            }
            PollOutcome::Terminal(OperationState::Running | OperationState::Unknown) => {
                Ok(ActivityOutcome {
                    result: json!({ "status": "unknown", "attempt": ctx.attempt }),
                    next: ActivityNext::Retry { failure },
                })
            }
            PollOutcome::Stalled(stall) => {
                // Progress watchdog / session-gone: the turn was already
                // cancelled before close (see above). Treat it as a failed
                // verdict (`stalled`) for the retry decision (retry while
                // attempts remain, park otherwise).
                self.note_provider_health(ctx, binding, &stall);
                self.note_stalled_retry_decision(ctx);
                eprintln!(
                    "agalma: build attempt {} stalled ({}): idle_ms={} wall_ms={}",
                    ctx.attempt,
                    stall.reason.as_str(),
                    stall.idle_ms,
                    stall.wall_ms
                );
                Ok(ActivityOutcome {
                    result: json!({
                        "status": "stalled",
                        "reason": stall.reason.as_str(),
                        "idle_ms": stall.idle_ms,
                        "wall_ms": stall.wall_ms,
                        "attempt": ctx.attempt,
                    }),
                    next: ActivityNext::Retry { failure },
                })
            }
            PollOutcome::TimedOut => {
                // The turn did not reach a terminal state within the hard bound
                // (free models can be chatty or slow). We cannot trust the turn
                // status, so we stop it and let the *verify* phase arbitrate the
                // candidate tree: a red verify drives the executor's bounded
                // build retry, a green verify integrates. This avoids discarding
                // a working edit just because the model never signed off.
                self.note_provider_health(
                    ctx,
                    binding,
                    &PollStall {
                        reason: PollStallReason::Idle,
                        idle_ms: self.deps.turn_timeout.as_millis() as u64,
                        tokens: 0,
                        wall_ms: self.deps.turn_timeout.as_millis() as u64,
                        last_state: "timed out".to_string(),
                    },
                );
                eprintln!(
                    "agalma: build attempt {} did not report terminal within {:?}; \
                     proceeding to verify",
                    ctx.attempt, self.deps.turn_timeout
                );
                Ok(ActivityOutcome {
                    result: json!({ "status": "timeout", "attempt": ctx.attempt }),
                    next: ActivityNext::Advance,
                })
            }
            PollOutcome::Stopped => {
                // Stop requested mid-turn (`agalma-r2l`): the session was
                // cancelled before close. Retry so the executor re-enters and
                // the outer stop policy (STOP check / kill latch) halts before
                // any further dispatch; no new paid call runs under a stop.
                eprintln!(
                    "agalma: build attempt {} cancelled: stop requested",
                    ctx.attempt
                );
                Ok(ActivityOutcome {
                    result: json!({ "status": "stopped", "attempt": ctx.attempt }),
                    next: ActivityNext::Retry { failure },
                })
            }
        }
    }
}

/// Deterministic scripted/reference builds and the dispatch that selects them.
impl BuildActivity {
    /// Deterministic scripted stand-in for the crash matrix (OpenCode choice,
    /// scripted mode): spawns a real `sleep` child as the recorded worker,
    /// applies the fixture fix, optionally sleeps (so a test can SIGKILL the
    /// conductor), then records completion.
    fn run_scripted(&self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        workers::clear_markers(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
        );
        // Write the per-attempt builder prompt deterministically so the retry
        // prompt (with the prior diagnosis) is observable in scripted tests.
        let root = run_root(&self.deps.state_dir, &ctx.execution_id);
        std::fs::create_dir_all(&root)
            .map_err(|e| ActivityError::KnownFailure(format!("create run root: {e}")))?;
        write_builder_prompt(&self.deps, &root, ctx)?;
        let started = Instant::now();

        let (pid, mut child) = spawn_sleeper()?;
        workers::record_worker(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
            ctx.operation_id.as_str(),
            ctx.lease_generation,
            pid,
            pid,
        )?;
        mark_started(&self.deps, &ctx.execution_id, "build", ctx.attempt)?;
        append_effect(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
        )?;
        apply_scripted_fix(&checkout.path)?;

        let sleep_ms = scripted_sleep_ms();
        if sleep_ms > 0 {
            std::thread::sleep(Duration::from_millis(sleep_ms));
        }

        let outcome = ActivityOutcome {
            result: json!({ "status": "scripted-build", "attempt": ctx.attempt }),
            next: ActivityNext::Advance,
        };
        record_usage_event(
            &self.deps.state_dir,
            &ctx.execution_id,
            &ctx.operation_id,
            "build",
            &self.deps.role,
            &self.deps.model,
            &self.deps.route,
            ctx.attempt,
            zero_usage(),
            started.elapsed().as_millis() as u64,
        )?;
        workers::write_done(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
            &outcome,
        )?;
        workers::clear_worker(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
        );
        let _ = child.kill();
        let _ = child.wait();
        Ok(outcome)
    }

    /// Deterministic reference build: a scripted `HarnessApi` turn (scenario per
    /// attempt, no vendor/network). The success profile applies the scripted
    /// change; the red profile leaves the acceptance red.
    fn run_reference(
        &self,
        ctx: &OperationContext,
        kind: HarnessKind,
    ) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        let root = run_root(&self.deps.state_dir, &ctx.execution_id);
        std::fs::create_dir_all(&root)
            .map_err(|e| ActivityError::KnownFailure(format!("create run root: {e}")))?;
        workers::clear_markers(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
        );
        let prompt_path = write_builder_prompt(&self.deps, &root, ctx)?;
        mark_started(&self.deps, &ctx.execution_id, "build", ctx.attempt)?;
        append_effect(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
        )?;

        let scenario_dir =
            root.join("reference")
                .join(format!("{}-attempt-{}", kind.as_str(), ctx.attempt));
        let transport = HarnessTransport::from_env();
        let socket = harness_socket_path(
            &self.deps.state_dir,
            transport,
            &format!("harness-ref-{}.sock", ctx.attempt),
        );
        let mut harness = harness_process::open_reference_harness(
            kind,
            transport,
            scenario_dir,
            socket,
            1,
            &self.deps.model,
        )
        .map_err(ActivityError::KnownFailure)?;
        let attempt = harness
            .start_attempt(StartAttemptRequest {
                workspace: checkout.path.clone(),
                role: self.deps.role.clone(),
                genome_ref: ArtifactRef::derive("genome").to_string(),
                constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
                limits: Limits {
                    wall_ms: self.deps.turn_timeout.as_millis() as u64,
                    tokens: 0,
                    cost_micros: 0,
                },
                operation_id: ctx.operation_id.clone(),
            })
            .map_err(contract_err)?;

        let started = Instant::now();
        let outcome = self.drive_reference(&mut *harness, ctx, &prompt_path, kind.as_str());
        let wall_ms = started.elapsed().as_millis() as u64;
        let _ = harness.stop_attempt(&attempt);
        if let Ok(outcome) = &outcome {
            let usage = usage_from_result(&outcome.result);
            record_usage_event(
                &self.deps.state_dir,
                &ctx.execution_id,
                &ctx.operation_id,
                "build",
                &self.deps.role,
                &self.deps.model,
                &self.deps.route,
                ctx.attempt,
                usage,
                wall_ms,
            )?;
            workers::write_done(
                &self.deps.state_dir,
                &ctx.execution_id,
                "build",
                ctx.attempt,
                outcome,
            )?;
        }
        outcome
    }

    /// One reference session/turn, polled to terminal.
    fn drive_reference(
        &self,
        harness: &mut dyn HarnessApi,
        ctx: &OperationContext,
        prompt_path: &Path,
        binding: &str,
    ) -> Result<ActivityOutcome, ActivityError> {
        let session = harness
            .create_session(CreateSessionRequest {
                role: self.deps.role.clone(),
                model: self.deps.model.clone(),
                prompts_ref: prompt_path.to_string_lossy().into_owned(),
                handoff_refs: Vec::new(),
                tool_policy_ref: ArtifactRef::derive("tool-policy").to_string(),
            })
            .map_err(contract_err)?;
        let op = harness
            .run_turn(
                &session,
                RunTurnRequest {
                    input_ref: ArtifactRef::derive("builder-turn"),
                    bounded_turns: 1,
                    deadline_ms: self.deps.turn_timeout.as_millis() as u64,
                },
            )
            .map_err(contract_err)?;
        let outcome = poll_terminal(
            harness,
            &op,
            self.deps.idle_timeout,
            self.deps.turn_timeout,
            Some(&*self.deps.stop),
        );
        // Cancel before close: the interrupt must reach a live session, and a
        // 404 means it already ended (benign). A terminal outcome needs no
        // cancel.
        settle_after_poll(harness, &op, &session, &outcome, binding);

        let failure = ArtifactRef::derive(&format!("build@{}", ctx.attempt));
        match outcome {
            PollOutcome::Terminal(OperationState::Completed { usage, .. }) => Ok(ActivityOutcome {
                result: json!({
                    "status": "completed",
                    "attempt": ctx.attempt,
                    "tokens_in": usage.tokens_in,
                    "tokens_out": usage.tokens_out,
                    "cost_usd": usage.cost_usd,
                    "completeness": usage.completeness.label(),
                }),
                next: ActivityNext::Advance,
            }),
            PollOutcome::Terminal(OperationState::Failed { reason }) => {
                self.note_provider_health(ctx, binding, &PollStall::failed(&reason, 0, 0));
                Ok(ActivityOutcome {
                    result: json!({ "status": "failed", "reason": reason, "attempt": ctx.attempt }),
                    next: ActivityNext::Retry { failure },
                })
            }
            PollOutcome::Terminal(_) => Ok(ActivityOutcome {
                result: json!({ "status": "unknown", "attempt": ctx.attempt }),
                next: ActivityNext::Retry { failure },
            }),
            PollOutcome::Stalled(stall) => {
                self.note_provider_health(ctx, binding, &stall);
                self.note_stalled_retry_decision(ctx);
                Ok(ActivityOutcome {
                    result: json!({
                        "status": "stalled",
                        "reason": stall.reason.as_str(),
                        "idle_ms": stall.idle_ms,
                        "wall_ms": stall.wall_ms,
                        "attempt": ctx.attempt,
                    }),
                    next: ActivityNext::Retry { failure },
                })
            }
            PollOutcome::TimedOut => Ok(ActivityOutcome {
                result: json!({ "status": "timeout", "attempt": ctx.attempt }),
                next: ActivityNext::Advance,
            }),
            PollOutcome::Stopped => Ok(ActivityOutcome {
                result: json!({ "status": "stopped", "attempt": ctx.attempt }),
                next: ActivityNext::Retry { failure },
            }),
        }
    }
}

impl Activity for BuildActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let kind = self.resolve_kind(ctx)?;
        self.persist_harness_binding(ctx, kind)?;
        match (kind, self.mode) {
            (HarnessKind::OpenCode, ActivityMode::Live) => self.run_live(ctx),
            (HarnessKind::OpenCode, ActivityMode::Scripted) => self.run_scripted(ctx),
            (HarnessKind::Reference | HarnessKind::ReferenceFail, _) => {
                self.run_reference(ctx, kind)
            }
        }
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        build_probe(&self.deps, &ctx.execution_id, ctx.attempt)
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        build_reconcile(&self.deps, &ctx.execution_id, ctx.attempt)
    }
}

// ---------------------------------------------------------------------------
// verify (live)
// ---------------------------------------------------------------------------

/// `verify`: a separate confined `cargo test` run (M0) or the task's acceptance
/// command list (repo mode, M1); exit codes are the verdict.
struct VerifyActivity {
    deps: PhaseDeps,
    /// Used to commit the candidate before verify and to clone the pristine
    /// verification checkout (repo mode).
    workspace: Rc<RefCell<WorkspaceHandle>>,
    state: Rc<RefCell<RunState>>,
    /// Run acceptance commands under Seatbelt (`Live`) or in-process
    /// (`Scripted`, deterministic tests).
    sandboxed: bool,
    /// Scripted mode replaces the fresh verifier diagnosis session with a
    /// deterministic, file-backed stand-in (no model call).
    scripted: bool,
    /// Lazily-started sandbox supervisor backing the live verifier session.
    sandbox: SandboxSource,
}

/// Probe a verify attempt via its durable markers.
fn verify_probe(
    deps: &PhaseDeps,
    execution: &agalma_contracts::ExecutionId,
    attempt: u32,
) -> EffectProbe {
    if workers::read_done(&deps.state_dir, execution, "verify", attempt).is_some() {
        return EffectProbe::Present;
    }
    if workers::started_exists(&deps.state_dir, execution, "verify", attempt) {
        // A verify that started but never recorded a verdict is ambiguous: the
        // sandbox may have partially run.
        return EffectProbe::Ambiguous;
    }
    EffectProbe::Absent
}

fn verify_reconcile(
    deps: &PhaseDeps,
    execution: &agalma_contracts::ExecutionId,
    attempt: u32,
) -> Result<ActivityOutcome, ActivityError> {
    workers::read_done(&deps.state_dir, execution, "verify", attempt).ok_or_else(|| {
        ActivityError::KnownFailure(format!(
            "verify reconcile: no completion marker for attempt {attempt}"
        ))
    })
}

impl VerifyActivity {
    /// Repo mode: commit the candidate on the candidate branch, clone a
    /// pristine verification checkout at that SHA, then run the task's
    /// acceptance commands there, each as its own launch. Any non-zero exit is
    /// the verdict; every command's outcome (command, exit code, duration,
    /// output artifact) is recorded and the failing commands are aggregated
    /// under `<run>/artifacts/`.
    ///
    /// On red, a *fresh* verifier session diagnoses the failure from file-only
    /// inputs (candidate diff + captured failure), the retry/escalate/park choice
    /// is recorded through the pinned static `retry.escalate` baseline, and an
    /// exhausted bound writes a postmortem before the executor parks the task.
    fn run_acceptance(&self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        let repo = self
            .deps
            .repo
            .as_ref()
            .ok_or_else(|| ActivityError::KnownFailure("repo mode required".into()))?;
        let root = run_root(&self.deps.state_dir, &ctx.execution_id);
        let artifacts = canonical_dir(&root.join("artifacts"))?;
        // SF.6: the verifier's sandbox ATTEMPT is a NARROW runtime root, not the
        // whole run root. Parent receipt/marker state (`<run>/verify/*.done`,
        // worker records, `checkout.json`, prompts) and the pins/protected root
        // all live outside it, so a confined verifier command cannot overwrite a
        // parent completion/worker marker.
        let runtime = canonical_dir(&verify_runtime_dir(&self.deps.state_dir, &ctx.execution_id))?;
        let attempt_dir = runtime.clone();
        let sock = canonical_dir(&runtime.join("sock"))?;
        let tmp = canonical_dir(&runtime.join("tmp"))?;
        let home = canonical_dir(&runtime.join("home"))?;
        mark_started(&self.deps, &ctx.execution_id, "verify", ctx.attempt)?;

        // 1. Commit the candidate working tree on the candidate branch. This
        //    moves the commit integration used to make *before* verify so the
        //    pristine verification checkout can be cloned at the candidate SHA.
        //    Integration later observes a clean tree and merges the same SHA.
        let candidate_sha = self
            .workspace
            .borrow()
            .commit_candidate(&checkout)
            .map_err(contract_err)?;

        // 2. Fresh verification environment: a clone of the candidate checkout
        //    at the candidate SHA with its own Git metadata under `<run>/verify`.
        //    The candidate checkout's dirty/working tree is never the
        //    verification environment.
        let verify_path = self
            .workspace
            .borrow()
            .prepare_verify_checkout(
                &checkout,
                &self.deps.state_dir,
                &ctx.execution_id,
                &candidate_sha,
            )
            .map_err(contract_err)?;
        let verify_dir = canonical_dir(&verify_path)?;

        // SF.6: authorize the candidate change set against the frozen scope,
        // materialize the immutable protected baseline into a separate read-only
        // protected root (outside the candidate), and run the *frozen* baseline
        // commands plus the frozen task acceptance (from the admitted package,
        // never the live task) so a candidate cannot substitute its own
        // always-pass tests for the judge. A package that fails to load
        // (missing/invalid protected entry or disagreement) refuses (parks)
        // rather than aborting the run.
        let package = match resolve_recorded_package(&self.deps.state_dir, &ctx.execution_id) {
            Ok(package) => package,
            Err(err) => {
                let reason = format!("protected inputs refused: {err}");
                eprintln!("agalma: verify attempt {} refused: {reason}", ctx.attempt);
                let outcome = park_outcome(&reason);
                workers::write_done(
                    &self.deps.state_dir,
                    &ctx.execution_id,
                    "verify",
                    ctx.attempt,
                    &outcome,
                )?;
                return Ok(outcome);
            }
        };
        let guard = self.guard_candidate(
            ctx,
            &checkout,
            &verify_dir,
            &candidate_sha,
            package.as_ref(),
        )?;
        // Commands run in order: the frozen task acceptance, then the frozen
        // protected baseline commands (the protected judge). Both must pass. A
        // denied guard returned above, so it has no commands.
        let mut acceptance: Vec<String> = package
            .as_ref()
            .map(|p| p.definition.acceptance.clone())
            .filter(|cmds| !cmds.is_empty())
            .unwrap_or_else(|| repo.acceptance.clone());
        acceptance.extend(
            guard
                .as_ref()
                .and_then(|guard| guard.baseline.as_ref())
                .map(|baseline| baseline.commands.clone())
                .unwrap_or_default(),
        );
        // The sandbox's read-only protected root: outside `<run>` (ATTEMPT), so
        // the judge cannot lie in a child writable tree. Canonicalize it for the
        // sandbox profile parameter.
        let protected_judge =
            canonical_dir(&protected_root(&self.deps.state_dir, &ctx.execution_id))?;

        if let Some(guard) = &guard {
            workers::write_json(
                &scope_evidence_path(&self.deps.state_dir, &ctx.execution_id, ctx.attempt),
                &guard.evidence,
            )?;
            if guard.denial {
                let failure_path = artifacts.join(format!("failure@{}.txt", ctx.attempt));
                std::fs::write(&failure_path, &guard.reason).map_err(|e| {
                    ActivityError::KnownFailure(format!(
                        "write scope failure {}: {e}",
                        failure_path.display()
                    ))
                })?;
                eprintln!(
                    "agalma: verify attempt {} refused (scope/baseline): {}",
                    ctx.attempt, guard.reason
                );
                // Missing/invalid required inputs park (refuse); a scope denial
                // is red (retry, exhausting to a park).
                if guard.park {
                    let outcome = park_outcome(&guard.reason);
                    workers::write_done(
                        &self.deps.state_dir,
                        &ctx.execution_id,
                        "verify",
                        ctx.attempt,
                        &outcome,
                    )?;
                    return Ok(outcome);
                }
                let outcome = ActivityOutcome {
                    result: json!({
                        "status": "red",
                        "reason": "scope_or_baseline_denied",
                        "attempt": ctx.attempt,
                        "candidate_sha": candidate_sha,
                        "oracle_digest": guard.evidence.oracle_digest,
                        "denied_paths": guard.evidence.denied_paths,
                        "detail": guard.reason,
                        "failure": failure_path.to_string_lossy(),
                    }),
                    next: ActivityNext::Retry {
                        failure: ArtifactRef::derive(&format!("verify@{}", ctx.attempt)),
                    },
                };
                workers::write_done(
                    &self.deps.state_dir,
                    &ctx.execution_id,
                    "verify",
                    ctx.attempt,
                    &outcome,
                )?;
                return Ok(outcome);
            }
        }

        let mut env = self.deps.toolchain_env.clone();
        env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        env.insert("TMPDIR".to_string(), tmp.to_string_lossy().into_owned());
        // Seed the run-local CARGO_HOME with a read-only registry link so cargo
        // resolves dependencies offline despite the empty confined HOME.
        env.insert(
            "CARGO_HOME".to_string(),
            ensure_cargo_home(&runtime)?.to_string_lossy().into_owned(),
        );
        // Build artifacts stay inside the narrow runtime root (ATTEMPT), never
        // in a parent receipt directory.
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            runtime.join("verify-target").to_string_lossy().into_owned(),
        );
        let mut path = env
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| "/usr/bin:/bin".to_string());
        path.push_str(":/usr/bin:/bin:/usr/sbin:/sbin");
        env.insert("PATH".to_string(), path);
        // Parent-controlled protected runner inputs (SF.6 amendment): the
        // read-only protected root (outside ATTEMPT/EXTRA_RW) and the pinned
        // candidate directory.
        env.insert(
            "AGALMA_PROTECTED_DIR".to_string(),
            protected_judge.to_string_lossy().into_owned(),
        );
        env.insert(
            "AGALMA_CANDIDATE_DIR".to_string(),
            verify_dir.to_string_lossy().into_owned(),
        );
        let started = Instant::now();
        let mut sandbox = SeatbeltSandbox::new();
        let mut command_results: Vec<serde_json::Value> = Vec::new();
        let mut failures: Vec<serde_json::Value> = Vec::new();
        for (index, command) in acceptance.iter().enumerate() {
            let log_path = artifacts.join(format!("accept@{}-{index}.txt", ctx.attempt));
            let cmd_started = Instant::now();
            let output = if self.sandboxed {
                let spec = LaunchSpec {
                    program: "sh".to_string(),
                    args: vec!["-c".to_string(), command.clone()],
                    cwd: verify_dir.to_string_lossy().into_owned(),
                    env: env.clone(),
                    profile: self.deps.profile_path.to_string_lossy().into_owned(),
                    attempt: AttemptId::derive(ctx.attempt),
                    attempt_dir: attempt_dir.to_string_lossy().into_owned(),
                    protected_dir: protected_judge.to_string_lossy().into_owned(),
                    sock_dir: sock.to_string_lossy().into_owned(),
                    extra_ro_roots: self
                        .deps
                        .extra_ro_roots
                        .iter()
                        .map(|root| root.to_string_lossy().into_owned())
                        .collect(),
                    // The only extra writable root is the pristine candidate
                    // checkout (cargo may need a Cargo.lock). Parent receipt
                    // state is NOT granted.
                    extra_rw_roots: vec![verify_dir.to_string_lossy().into_owned()],
                };
                sandbox
                    .run(&spec, self.deps.verify_timeout)
                    .map_err(contract_err)?
            } else {
                run_in_process(command, &verify_dir.to_string_lossy(), &env)?
            };
            let duration_ms = cmd_started.elapsed().as_millis() as u64;
            let log = format!(
                "$ {command}\nexit: {}\nduration_ms: {duration_ms}\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
                output.exit_code, output.stdout, output.stderr
            );
            std::fs::write(&log_path, log).map_err(|e| {
                ActivityError::KnownFailure(format!("write verify log {}: {e}", log_path.display()))
            })?;
            let entry = json!({
                "attempt": ctx.attempt,
                "command": command,
                "exit_code": output.exit_code,
                "duration_ms": duration_ms,
                "artifact": log_path.to_string_lossy(),
            });
            command_results.push(entry.clone());
            if output.exit_code != 0 {
                failures.push(entry);
            }
        }

        if !failures.is_empty() {
            let first_exit = failures[0]
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1) as i32;
            let first_command = failures[0]
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let failure_path = artifacts.join(format!("failure@{}.txt", ctx.attempt));
            let failure = format!(
                "{} of {} acceptance command(s) failed on attempt {}\n--- failures ---\n{}\n",
                failures.len(),
                command_results.len(),
                ctx.attempt,
                failures
                    .iter()
                    .map(|f| {
                        format!(
                            "{} (exit {})",
                            f.get("command").and_then(|v| v.as_str()).unwrap_or(""),
                            f.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            std::fs::write(&failure_path, &failure).map_err(|e| {
                ActivityError::KnownFailure(format!(
                    "write failure {}: {e}",
                    failure_path.display()
                ))
            })?;
            eprintln!(
                "agalma: verify attempt {} red ({} failing): {first_command}",
                ctx.attempt,
                failures.len()
            );
            // Per-attempt cost: the Rust-run acceptance has no model usage, but
            // its wall time is recorded for the digest.
            record_usage_event(
                &self.deps.state_dir,
                &ctx.execution_id,
                &ctx.operation_id,
                "verify",
                "acceptance",
                "",
                "",
                ctx.attempt,
                zero_usage(),
                started.elapsed().as_millis() as u64,
            )?;
            // Handoff via files only: candidate diff + captured failure.
            write_candidate_diff(&checkout, &artifacts, ctx.attempt)?;
            let diagnosis = self.diagnose(&checkout, &root, &artifacts, ctx, &failure_path)?;
            let max_attempts = execution_max_attempts(&self.deps.state_dir, &ctx.execution_id)?;
            record_retry_decision(
                &self.deps.state_dir,
                &ctx.execution_id,
                ctx.attempt,
                max_attempts,
            )?;
            if ctx.attempt >= max_attempts {
                write_postmortem(
                    &self.deps.state_dir,
                    &ctx.execution_id,
                    ctx.attempt,
                    max_attempts,
                )?;
            }
            let outcome = ActivityOutcome {
                result: json!({
                    "status": "red",
                    "exit_code": first_exit,
                    "attempt": ctx.attempt,
                    "candidate_sha": candidate_sha,
                    "verify_path": verify_dir.to_string_lossy(),
                    "command_results": command_results,
                    "failing_commands": failures,
                    "diagnosis": diagnosis.to_string_lossy(),
                    "wall_ms": started.elapsed().as_millis() as u64,
                }),
                next: ActivityNext::Retry {
                    failure: ArtifactRef::derive(&format!("verify@{}", ctx.attempt)),
                },
            };
            workers::write_done(
                &self.deps.state_dir,
                &ctx.execution_id,
                "verify",
                ctx.attempt,
                &outcome,
            )?;
            return Ok(outcome);
        }

        let wall_ms = started.elapsed().as_millis() as u64;
        record_usage_event(
            &self.deps.state_dir,
            &ctx.execution_id,
            &ctx.operation_id,
            "verify",
            "acceptance",
            "",
            "",
            ctx.attempt,
            zero_usage(),
            wall_ms,
        )?;
        // SF.6: pin the verified candidate identity (candidate SHA + oracle
        // digest + frozen package digest + attempt) so a changed candidate or a
        // mismatched package is refused at the integration boundary.
        if let Some(guard) = &guard {
            let pin = CandidatePin {
                execution_id: ctx.execution_id.as_str().to_string(),
                generation: ctx.lease_generation,
                package_digest: guard.evidence.package_digest.clone(),
                attempt: ctx.attempt,
                candidate_sha: candidate_sha.clone(),
                oracle_digest: guard.evidence.oracle_digest.clone().unwrap_or_default(),
            };
            workers::write_json(
                &scope_pin_path(&self.deps.state_dir, &ctx.execution_id),
                &pin,
            )?;
        }
        let oracle_digest = guard
            .as_ref()
            .and_then(|guard| guard.evidence.oracle_digest.clone());
        let outcome = ActivityOutcome {
            result: json!({
                "status": "green",
                "exit_code": 0,
                "attempt": ctx.attempt,
                "candidate_sha": candidate_sha,
                "oracle_digest": oracle_digest,
                "verify_path": verify_dir.to_string_lossy(),
                "command_results": command_results,
                "wall_ms": wall_ms,
            }),
            next: ActivityNext::Advance,
        };
        workers::write_done(
            &self.deps.state_dir,
            &ctx.execution_id,
            "verify",
            ctx.attempt,
            &outcome,
        )?;
        Ok(outcome)
    }

    /// Render and write the per-attempt verifier prompt under `<run>/prompts/`;
    /// the confined verifier reads it via its read-only run-root PROTECTED grant.
    fn write_verifier_prompt(
        &self,
        root: &Path,
        ctx: &OperationContext,
    ) -> Result<PathBuf, ActivityError> {
        let text = render_verifier_prompt(VERIFIER_PROMPT, &self.deps, root, ctx.attempt);
        let dir = root.join("prompts");
        std::fs::create_dir_all(&dir)
            .map_err(|e| ActivityError::KnownFailure(format!("create prompt dir: {e}")))?;
        let path = dir.join(format!("verifier@{}.md", ctx.attempt));
        std::fs::write(&path, text).map_err(|e| {
            ActivityError::KnownFailure(format!("write prompt {}: {e}", path.display()))
        })?;
        Ok(path)
    }

    /// Produce `artifacts/diagnosis@<attempt>.md` from a *fresh* verifier
    /// session. Scripted mode writes a deterministic diagnosis; live mode runs a
    /// read-only OpenCode session whose prompt names the file-only inputs and the
    /// diagnosis output path.
    fn diagnose(
        &self,
        checkout: &Checkout,
        root: &Path,
        artifacts: &Path,
        ctx: &OperationContext,
        failure_path: &Path,
    ) -> Result<PathBuf, ActivityError> {
        // The retry handoff reads the diagnosis from `<run>/artifacts/`, but the
        // confined verifier child may only write inside its narrow ATTEMPT root
        // (SF.6). The live session writes to `<run>/verify-runtime/diagnosis@N.md`;
        // the parent then persists it under artifacts.
        let artifacts_diagnosis = artifacts.join(format!("diagnosis@{}.md", ctx.attempt));
        let started = Instant::now();
        let usage = if self.scripted {
            let failure = std::fs::read_to_string(failure_path).unwrap_or_default();
            let diff =
                std::fs::read_to_string(artifacts.join(format!("diff@{}.patch", ctx.attempt)))
                    .unwrap_or_default();
            std::fs::write(
                &artifacts_diagnosis,
                render_scripted_diagnosis(ctx.attempt, &failure, &diff),
            )
            .map_err(|e| {
                ActivityError::KnownFailure(format!(
                    "write diagnosis {}: {e}",
                    artifacts_diagnosis.display()
                ))
            })?;
            append_effect(
                &self.deps.state_dir,
                &ctx.execution_id,
                "verifier",
                ctx.attempt,
            )?;
            zero_usage()
        } else {
            let runtime =
                canonical_dir(&verify_runtime_dir(&self.deps.state_dir, &ctx.execution_id))?;
            // The diagnosis inputs (diff/failure) live under `<run>/artifacts`
            // and are readable through the verifier's read-only run-root
            // PROTECTED grant; the output is written inside the narrow ATTEMPT.
            let prompt_path = self.write_verifier_prompt(root, ctx)?;
            let runtime_diagnosis = runtime.join(format!("diagnosis@{}.md", ctx.attempt));
            let usage = self.run_verifier_session(
                checkout,
                root,
                ctx,
                &prompt_path,
                failure_path,
                &runtime_diagnosis,
            )?;
            if runtime_diagnosis.exists() {
                std::fs::copy(&runtime_diagnosis, &artifacts_diagnosis).map_err(|e| {
                    ActivityError::KnownFailure(format!(
                        "persist diagnosis {}: {e}",
                        artifacts_diagnosis.display()
                    ))
                })?;
            }
            usage
        };
        record_usage_event(
            &self.deps.state_dir,
            &ctx.execution_id,
            &ctx.operation_id,
            "verify",
            "verifier",
            &self.deps.verifier_model,
            &self.deps.verifier_route,
            ctx.attempt,
            usage,
            started.elapsed().as_millis() as u64,
        )?;
        Ok(artifacts_diagnosis)
    }

    /// Live verifier: a fresh confined OpenCode attempt/session, read-only with
    /// respect to the checkout. Returns the session usage.
    fn run_verifier_session(
        &self,
        checkout: &Checkout,
        root: &Path,
        ctx: &OperationContext,
        prompt_path: &Path,
        failure_path: &Path,
        diagnosis_path: &Path,
    ) -> Result<Usage, ActivityError> {
        // Production verifier-diagnosis config (shared helper): narrow ATTEMPT
        // runtime, run root read-only (diagnosis inputs + checkout + prompt), no
        // extra writable root, all supplied RO roots preserved.
        let config = build_config(verifier_config_init(
            &self.deps,
            checkout,
            &ctx.execution_id,
        )?);

        let sandbox_socket = self.sandbox.ensure().map_err(contract_err)?;
        let transport = HarnessTransport::from_env();
        let harness_socket = harness_socket_path(
            &self.deps.state_dir,
            transport,
            &format!("harness-verify-{}.sock", ctx.attempt),
        );
        let mut harness = harness_process::open_opencode_harness(OpenCodeHarnessInit {
            transport,
            config,
            root: root.to_path_buf(),
            model: self.deps.verifier_model.clone(),
            generation: 1,
            sandbox_socket,
            harness_socket,
        })
        .map_err(ActivityError::KnownFailure)?;
        let attempt = harness
            .start_attempt(StartAttemptRequest {
                workspace: checkout.path.clone(),
                role: "verifier".to_string(),
                genome_ref: ArtifactRef::derive("genome").to_string(),
                constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
                limits: Limits {
                    wall_ms: self.deps.turn_timeout.as_millis() as u64,
                    tokens: 0,
                    cost_micros: 0,
                },
                operation_id: ctx.operation_id.clone(),
            })
            .map_err(contract_err)?;
        let usage =
            self.run_verifier_turn(&mut harness, ctx, prompt_path, failure_path, diagnosis_path);
        let _ = harness.stop_attempt(&attempt);
        usage
    }

    fn run_verifier_turn(
        &self,
        harness: &mut LiveHarness,
        ctx: &OperationContext,
        prompt_path: &Path,
        failure_path: &Path,
        diagnosis_path: &Path,
    ) -> Result<Usage, ActivityError> {
        let diff_path = run_root(&self.deps.state_dir, &ctx.execution_id)
            .join("artifacts")
            .join(format!("diff@{}.patch", ctx.attempt));
        let session = harness
            .create_session(CreateSessionRequest {
                role: "verifier".to_string(),
                model: self.deps.verifier_model.clone(),
                prompts_ref: prompt_path.to_string_lossy().into_owned(),
                handoff_refs: vec![
                    ArtifactRef::derive(&format!("diff@{}", ctx.attempt)),
                    ArtifactRef::derive(&format!("failure@{}", ctx.attempt)),
                ],
                tool_policy_ref: ArtifactRef::derive("tool-policy-verifier").to_string(),
            })
            .map_err(contract_err)?;
        let op = harness
            .run_turn(
                &session,
                RunTurnRequest {
                    input_ref: ArtifactRef::derive("verifier-turn"),
                    bounded_turns: 1,
                    deadline_ms: self.deps.turn_timeout.as_millis() as u64,
                },
            )
            .map_err(contract_err)?;
        let outcome = poll_terminal(
            harness,
            &op,
            self.deps.idle_timeout,
            self.deps.turn_timeout,
            Some(&*self.deps.stop),
        );
        settle_after_poll(
            harness,
            &op,
            &session,
            &outcome,
            HarnessKind::OpenCode.as_str(),
        );
        let usage = match outcome {
            PollOutcome::Terminal(OperationState::Completed { usage, .. }) => usage,
            PollOutcome::Terminal(OperationState::Failed { reason }) => {
                record_provider_health(
                    &self.deps.state_dir,
                    &ctx.execution_id,
                    &self.deps.verifier_model,
                    HarnessKind::OpenCode.as_str(),
                    ctx.attempt,
                    &PollStall::failed(&reason, 0, 0),
                )
                .unwrap_or_else(|err| eprintln!("agalma: provider_health for verifier: {err}"));
                eprintln!("agalma: verifier session failed: {reason}");
                zero_usage()
            }
            PollOutcome::Stalled(stall) => {
                record_provider_health(
                    &self.deps.state_dir,
                    &ctx.execution_id,
                    &self.deps.verifier_model,
                    HarnessKind::OpenCode.as_str(),
                    ctx.attempt,
                    &stall,
                )
                .unwrap_or_else(|err| eprintln!("agalma: provider_health for verifier: {err}"));
                eprintln!(
                    "agalma: verifier session stalled ({}): idle_ms={} wall_ms={}",
                    stall.reason.as_str(),
                    stall.idle_ms,
                    stall.wall_ms
                );
                zero_usage()
            }
            PollOutcome::TimedOut => {
                record_provider_health(
                    &self.deps.state_dir,
                    &ctx.execution_id,
                    &self.deps.verifier_model,
                    HarnessKind::OpenCode.as_str(),
                    ctx.attempt,
                    &PollStall {
                        reason: PollStallReason::Idle,
                        idle_ms: self.deps.turn_timeout.as_millis() as u64,
                        tokens: 0,
                        wall_ms: self.deps.turn_timeout.as_millis() as u64,
                        last_state: "timed out".to_string(),
                    },
                )
                .unwrap_or_else(|err| eprintln!("agalma: provider_health for verifier: {err}"));
                zero_usage()
            }
            PollOutcome::Stopped => {
                eprintln!("agalma: verifier session cancelled: stop requested");
                zero_usage()
            }
            PollOutcome::Terminal(_) => zero_usage(),
        };
        // The verifier is expected to write the diagnosis itself (the prompt
        // names the absolute output path). If it did not, record a fallback so
        // the retry still has a diagnosis artifact and the inputs are visible.
        if !diagnosis_path.exists() {
            let fallback = format!(
                "# Verifier diagnosis (attempt {})\n\n## cause\n\n\
                 Verifier session produced no diagnosis file; see inputs `{}` and `{}`.\n\n\
                 ## suggested fix\n\nInspect the candidate diff against the captured failure.\n\n\
                 ## evidence\n\nSee the failure and diff artifacts.\n\n## confidence\n\n0.0 — no diagnosis produced.\n",
                ctx.attempt,
                diff_path.display(),
                failure_path.display(),
            );
            std::fs::write(diagnosis_path, fallback).map_err(|e| {
                ActivityError::KnownFailure(format!(
                    "write fallback diagnosis {}: {e}",
                    diagnosis_path.display()
                ))
            })?;
        }
        Ok(usage)
    }
}

impl Activity for VerifyActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        if self.deps.repo.is_some() {
            return self.run_acceptance(ctx);
        }
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        let root = run_root(&self.deps.state_dir, &ctx.execution_id);
        let artifacts = canonical_dir(&root.join("artifacts"))?;
        // SF.6: narrow the verifier ATTEMPT to a runtime root so parent receipt
        // markers (`<run>/verify/<attempt>.done`) are not inside it.
        let runtime = canonical_dir(&verify_runtime_dir(&self.deps.state_dir, &ctx.execution_id))?;
        let attempt_dir = runtime.clone();
        let protected = canonical_dir(&root.join("protected"))?;
        let sock = canonical_dir(&runtime.join("sock"))?;
        let tmp = canonical_dir(&runtime.join("tmp"))?;
        let home = canonical_dir(&runtime.join("home"))?;
        let log_path = artifacts.join(format!("verify@{}.log", ctx.attempt));
        mark_started(&self.deps, &ctx.execution_id, "verify", ctx.attempt)?;

        let cargo = cargo_bin()?;
        let mut env = self.deps.toolchain_env.clone();
        env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        env.insert("TMPDIR".to_string(), tmp.to_string_lossy().into_owned());
        env.insert(
            "CARGO_HOME".to_string(),
            ensure_cargo_home(&runtime)?.to_string_lossy().into_owned(),
        );
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            runtime.join("verify-target").to_string_lossy().into_owned(),
        );

        let spec = LaunchSpec {
            program: cargo.to_string_lossy().into_owned(),
            args: vec!["test".to_string()],
            cwd: checkout.path.clone(),
            env,
            profile: self.deps.profile_path.to_string_lossy().into_owned(),
            attempt: AttemptId::derive(ctx.attempt),
            attempt_dir: attempt_dir.to_string_lossy().into_owned(),
            protected_dir: protected.to_string_lossy().into_owned(),
            sock_dir: sock.to_string_lossy().into_owned(),
            extra_ro_roots: self
                .deps
                .extra_ro_roots
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect(),
            // The candidate checkout is the only extra writable root.
            extra_rw_roots: vec![checkout.path.clone()],
        };

        let mut sandbox = SeatbeltSandbox::new();
        let output = sandbox
            .run(&spec, self.deps.verify_timeout)
            .map_err(contract_err)?;
        let log = format!(
            "$ cargo test (exit {})\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
            output.exit_code, output.stdout, output.stderr
        );
        std::fs::write(&log_path, log).map_err(|e| {
            ActivityError::KnownFailure(format!("write verify log {}: {e}", log_path.display()))
        })?;

        let outcome = if output.exit_code == 0 {
            ActivityOutcome {
                result: json!({
                    "status": "green",
                    "exit_code": output.exit_code,
                    "attempt": ctx.attempt,
                    "artifact": log_path.to_string_lossy(),
                }),
                next: ActivityNext::Advance,
            }
        } else {
            eprintln!(
                "agalma: verify attempt {} red (exit {}); log {}",
                ctx.attempt,
                output.exit_code,
                log_path.display()
            );
            ActivityOutcome {
                result: json!({
                    "status": "red",
                    "exit_code": output.exit_code,
                    "attempt": ctx.attempt,
                    "artifact": log_path.to_string_lossy(),
                }),
                next: ActivityNext::Retry {
                    failure: ArtifactRef::derive(&format!("verify@{}", ctx.attempt)),
                },
            }
        };
        workers::write_done(
            &self.deps.state_dir,
            &ctx.execution_id,
            "verify",
            ctx.attempt,
            &outcome,
        )?;
        Ok(outcome)
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        verify_probe(&self.deps, &ctx.execution_id, ctx.attempt)
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        verify_reconcile(&self.deps, &ctx.execution_id, ctx.attempt)
    }
}

// ---------------------------------------------------------------------------
// verify (scripted)
// ---------------------------------------------------------------------------

/// Deterministic verify stand-in: records a green verdict with no side effects.
struct ScriptedVerifyActivity {
    deps: PhaseDeps,
    state: Rc<RefCell<RunState>>,
}

impl Activity for ScriptedVerifyActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let _ = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        workers::clear_markers(
            &self.deps.state_dir,
            &ctx.execution_id,
            "verify",
            ctx.attempt,
        );
        append_effect(
            &self.deps.state_dir,
            &ctx.execution_id,
            "verify",
            ctx.attempt,
        )?;
        let outcome = ActivityOutcome {
            result: json!({ "status": "green", "exit_code": 0, "attempt": ctx.attempt }),
            next: ActivityNext::Advance,
        };
        workers::write_done(
            &self.deps.state_dir,
            &ctx.execution_id,
            "verify",
            ctx.attempt,
            &outcome,
        )?;
        Ok(outcome)
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        verify_probe(&self.deps, &ctx.execution_id, ctx.attempt)
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        verify_reconcile(&self.deps, &ctx.execution_id, ctx.attempt)
    }
}

// ---------------------------------------------------------------------------
// integrate
// ---------------------------------------------------------------------------

/// `integrate`: compare-and-update `main` to the candidate and tag `m0/<exec>`.
struct IntegrateActivity {
    deps: PhaseDeps,
    workspace: Rc<RefCell<WorkspaceHandle>>,
    state: Rc<RefCell<RunState>>,
}

fn integrate_tag(checkout: &Checkout) -> String {
    format!(
        "m0/{}",
        checkout.candidate_branch.trim_start_matches("candidate/")
    )
}

/// Repo-mode integration tag (`m1/<execution>`).
fn repo_integrate_tag(checkout: &Checkout) -> String {
    format!(
        "m1/{}",
        checkout.candidate_branch.trim_start_matches("candidate/")
    )
}

/// SF.6: a durable verify verdict may satisfy the pin only when it is the
/// **successful** verdict for the SAME attempt, candidate and oracle — not
/// merely a `status == "green"` outcome. Binds the recorded verdict's identity
/// (attempt, candidate SHA, oracle digest) and its advancing/exit semantics.
pub fn verify_verdict_matches_pin(
    pin: &CandidatePin,
    outcome: &ActivityOutcome,
) -> Result<(), String> {
    if !matches!(outcome.next, ActivityNext::Advance) {
        return Err("durable verdict is not an advancing (successful) outcome".to_string());
    }
    let r = &outcome.result;
    let status = r.get("status").and_then(|v| v.as_str());
    let exit = r.get("exit_code").and_then(|v| v.as_i64());
    let attempt = r.get("attempt").and_then(|v| v.as_u64());
    let candidate = r.get("candidate_sha").and_then(|v| v.as_str());
    let oracle = r.get("oracle_digest").and_then(|v| v.as_str());
    if status != Some("green") {
        return Err(format!("verdict status {status:?} is not green"));
    }
    if exit != Some(0) {
        return Err(format!("verdict exit_code {exit:?} is not 0"));
    }
    if attempt != Some(pin.attempt as u64) {
        return Err(format!(
            "verdict attempt {attempt:?} does not match pin attempt {}",
            pin.attempt
        ));
    }
    if candidate != Some(pin.candidate_sha.as_str()) {
        return Err("verdict candidate_sha does not match the pin".to_string());
    }
    if pin.oracle_digest.is_empty() {
        if oracle.is_some_and(|s| !s.is_empty()) {
            return Err("verdict oracle present but pin oracle is empty".to_string());
        }
    } else if oracle != Some(pin.oracle_digest.as_str()) {
        return Err("verdict oracle_digest does not match the pin".to_string());
    }
    Ok(())
}

/// A fail-closed park outcome (missing/invalid required inputs, identity
/// mismatch). Distinct from a red verify, which retries.
fn park_outcome(reason: &str) -> ActivityOutcome {
    ActivityOutcome {
        result: json!({ "status": "parked", "reason": reason }),
        next: ActivityNext::Park {
            reason: reason.to_string(),
        },
    }
}

fn integrate_outcome(receipt: &agalma_contracts::MergeReceipt, tag: &str) -> ActivityOutcome {
    ActivityOutcome {
        result: json!({
            "expected_main_sha": receipt.expected_main_sha,
            "candidate_sha": receipt.candidate_sha,
            "result_sha": receipt.result_sha,
            "tag": tag,
        }),
        next: ActivityNext::Advance,
    }
}

impl Activity for IntegrateActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        let mut workspace = self.workspace.borrow_mut();

        // Repo mode (M1): CAS the origin's `main` to the candidate and tag the
        // origin `m1/<exec>`.
        if let Some(repo) = &self.deps.repo {
            let tag = repo_integrate_tag(&checkout);
            if let Some(receipt) = workspace
                .origin_receipt(&checkout, &repo.origin)
                .map_err(contract_err)?
            {
                return Ok(integrate_outcome(&receipt, &tag));
            }
            // SF.6: require the authoritative verification pin and revalidate
            // its full identity (execution/generation, package digest, attempt,
            // candidate SHA, oracle digest) at the actual integration boundary.
            // The frozen package and its oracle digest are re-derived from the
            // immutable store, never from candidate content.
            let package = match resolve_recorded_package(&self.deps.state_dir, &ctx.execution_id) {
                Ok(Some(package)) => package,
                Ok(None) => {
                    return Ok(park_outcome(&format!(
                        "repository integration for {} has no recorded TaskPackage; refusing",
                        ctx.execution_id
                    )))
                }
                Err(err) => {
                    return Ok(park_outcome(&format!(
                        "protected inputs refused at integration: {err}"
                    )))
                }
            };
            let oracle_digest =
                match scope_guard::entry_from_artifact(&self.deps.state_dir, &package.scope.oracle)
                {
                    Ok((bytes, _entry)) => agalma_contracts::digest::sha256_digest(&bytes),
                    Err(err) => {
                        return Ok(park_outcome(&format!("frozen oracle unreadable: {err}")))
                    }
                };
            let pin = match workers::read_json::<CandidatePin>(&scope_pin_path(
                &self.deps.state_dir,
                &ctx.execution_id,
            )) {
                Some(pin) => pin,
                None => {
                    return Ok(park_outcome(&format!(
                        "protected execution {} has no verification pin; refusing to integrate \
                             an unverified candidate",
                        ctx.execution_id
                    )))
                }
            };
            // The pin's attempt must correspond to a durable *successful* (green)
            // verification verdict: a missing/corrupt/non-green/stale attempt
            // refuses, so an altered attempt with all other fields unchanged
            // cannot ship.
            let verdict = workers::read_done(
                &self.deps.state_dir,
                &ctx.execution_id,
                "verify",
                pin.attempt,
            );
            let Some(verdict) = verdict else {
                return Ok(park_outcome(&format!(
                    "verification pin attempt {} has no durable verdict; refusing to integrate",
                    pin.attempt
                )));
            };
            if let Err(reason) = verify_verdict_matches_pin(&pin, &verdict) {
                return Ok(park_outcome(&format!(
                    "verification verdict refused: {reason}"
                )));
            }
            let current = match workspace.candidate_sha(&checkout) {
                Ok(sha) => sha,
                Err(err) => return Ok(park_outcome(&err.to_string())),
            };
            if let Err(err) = pin.revalidate(&scope_guard::PinIdentity {
                execution_id: ctx.execution_id.as_str(),
                generation: ctx.lease_generation,
                package_digest: &package.package_digest,
                candidate_sha: &current,
                oracle_digest: &oracle_digest,
            }) {
                return Ok(park_outcome(&err.to_string()));
            }
            // The verified tree must be exactly the tree that ships.
            if let Err(err) = ensure_clean_worktree(&checkout) {
                return Ok(park_outcome(&err.to_string()));
            }
            // Enforce the expected candidate at the actual merge mutation
            // boundary: Workspace refuses to move `main` if the committed tree
            // differs from the verified candidate SHA.
            let receipt = match workspace.integrate_origin_verified(
                &checkout,
                &repo.origin,
                &checkout.base_sha,
                &pin.candidate_sha,
            ) {
                Ok(receipt) => receipt,
                Err(err) => return Ok(park_outcome(&format!("integration refused: {err}"))),
            };
            return Ok(integrate_outcome(&receipt, &tag));
        }

        // Guard against a double merge if the probe missed an already-landed
        // integration: reconcile from Git state instead of re-merging.
        if let Some(receipt) = workspace
            .integrated_receipt(&checkout)
            .map_err(contract_err)?
        {
            let tag = integrate_tag(&checkout);
            workspace
                .tag(&tag, &receipt.result_sha)
                .map_err(contract_err)?;
            return Ok(integrate_outcome(&receipt, &tag));
        }
        let receipt = workspace
            .integrate(&checkout, &checkout.base_sha)
            .map_err(contract_err)?;
        let tag = integrate_tag(&checkout);
        Ok(integrate_outcome(&receipt, &tag))
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        let Ok(checkout) = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)
        else {
            return EffectProbe::Ambiguous;
        };
        let mut workspace = self.workspace.borrow_mut();
        if let Some(repo) = &self.deps.repo {
            return match workspace.origin_receipt(&checkout, &repo.origin) {
                Ok(Some(_)) => EffectProbe::Present,
                Ok(None) => EffectProbe::Absent,
                Err(err) => {
                    eprintln!("agalma: repo integrate probe failed: {err}");
                    EffectProbe::Ambiguous
                }
            };
        }
        match workspace.integrated_receipt(&checkout) {
            Ok(Some(_)) => EffectProbe::Present,
            Ok(None) => EffectProbe::Absent,
            Err(err) => {
                eprintln!("agalma: integrate probe failed: {err}");
                EffectProbe::Ambiguous
            }
        }
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        let mut workspace = self.workspace.borrow_mut();
        if let Some(repo) = &self.deps.repo {
            let tag = repo_integrate_tag(&checkout);
            let receipt = workspace
                .origin_receipt(&checkout, &repo.origin)
                .map_err(contract_err)?
                .ok_or_else(|| {
                    ActivityError::KnownFailure(
                        "integrate reconcile: origin main is not at the candidate".to_string(),
                    )
                })?;
            return Ok(integrate_outcome(&receipt, &tag));
        }
        let receipt = workspace
            .integrated_receipt(&checkout)
            .map_err(contract_err)?
            .ok_or_else(|| {
                ActivityError::KnownFailure(
                    "integrate reconcile: main is not at the candidate".to_string(),
                )
            })?;
        let tag = integrate_tag(&checkout);
        workspace
            .tag(&tag, &receipt.result_sha)
            .map_err(contract_err)?;
        Ok(integrate_outcome(&receipt, &tag))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Write the phase's started marker.
fn mark_started(
    deps: &PhaseDeps,
    execution: &agalma_contracts::ExecutionId,
    phase: &str,
    attempt: u32,
) -> Result<(), ActivityError> {
    workers::write_json(
        &workers::started_path(&deps.state_dir, execution, phase, attempt),
        &json!({ "phase": phase, "attempt": attempt }),
    )
}

/// Append one effect line for scripted-mode evidence.
fn append_effect(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
    phase: &str,
    attempt: u32,
) -> Result<(), ActivityError> {
    let path = effects_log_path(state_dir, execution);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ActivityError::KnownFailure(format!("create {}: {e}", parent.display()))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| ActivityError::KnownFailure(format!("open effects log: {e}")))?;
    writeln!(file, "{phase}@{attempt}")
        .map_err(|e| ActivityError::KnownFailure(format!("append effect: {e}")))?;
    file.sync_all()
        .map_err(|e| ActivityError::KnownFailure(format!("sync effect: {e}")))
}

/// Scripted build sleep duration in milliseconds (`AGALMA_SCRIPTED_BUILD_SLEEP_MS`).
fn scripted_sleep_ms() -> u64 {
    std::env::var(SCRIPTED_SLEEP_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Spawn a long-lived child as a process-group leader to stand in for a confined
/// worker. Returns `(pid, child)` with `pgid == pid`.
#[cfg(unix)]
fn spawn_sleeper() -> Result<(u32, std::process::Child), ActivityError> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("/bin/sleep");
    command.arg("600");
    command.process_group(0);
    // Detach stdio: the worker must not hold the conductor's (or a test's)
    // captured pipe open after the conductor exits.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = command
        .spawn()
        .map_err(|e| ActivityError::KnownFailure(format!("spawn scripted worker: {e}")))?;
    let pid = child.id();
    Ok((pid, child))
}

#[cfg(not(unix))]
fn spawn_sleeper() -> Result<(u32, std::process::Child), ActivityError> {
    Err(ActivityError::KnownFailure(
        "scripted worker requires unix process groups".to_string(),
    ))
}

/// Apply the fixture fix (`answer() == 42`) directly to the checkout.
fn apply_scripted_fix(checkout_path: &str) -> Result<(), ActivityError> {
    let src = Path::new(checkout_path).join("src").join("lib.rs");
    std::fs::write(
        &src,
        "//! Fixed by the scripted M0.8 build.\n\n/// Returns the answer the acceptance test expects.\npub fn answer() -> u32 {\n    42\n}\n",
    )
    .map_err(|e| ActivityError::KnownFailure(format!("apply scripted fix {}: {e}", src.display())))
}

/// Write the per-attempt builder prompt under `<run>/prompts/` and return its
/// path.
///
/// In fixture mode (`repo == None`) the base prompt file is the explicit fixture
/// prompt and is returned unchanged. In repository mode the recorded immutable
/// `TaskPackage/v1` is resolved for this execution from the frozen package store
/// (never a live backlog read) and its full context is rendered (`agalma-5fy.1`).
fn write_builder_prompt(
    deps: &PhaseDeps,
    root: &Path,
    ctx: &OperationContext,
) -> Result<PathBuf, ActivityError> {
    let base = std::fs::read_to_string(&deps.builder_prompt).unwrap_or_default();
    let package = if deps.repo.is_some() {
        resolve_recorded_package(&deps.state_dir, &ctx.execution_id)?
    } else {
        None
    };
    let text = render_builder_prompt(
        &deps.state_dir,
        &base,
        deps.repo.as_ref(),
        package.as_ref(),
        root,
        ctx.attempt,
    )?;
    // The prompt lives under `<run>/prompts`; the confined builder reads it
    // read-only via its PROTECTED run-root grant (writes stay in ATTEMPT).
    let dir = root.join("prompts");
    std::fs::create_dir_all(&dir)
        .map_err(|e| ActivityError::KnownFailure(format!("create prompt dir: {e}")))?;
    let path = dir.join(format!("builder@{}.md", ctx.attempt));
    std::fs::write(&path, text).map_err(|e| {
        ActivityError::KnownFailure(format!("write prompt {}: {e}", path.display()))
    })?;
    Ok(path)
}

/// Reconstruct a [`Usage`] from an activity result payload (best effort).
fn usage_from_result(result: &serde_json::Value) -> Usage {
    let tokens_in = result
        .get("tokens_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tokens_out = result
        .get("tokens_out")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cost_usd = result
        .get("cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let completeness = match result.get("completeness").and_then(|v| v.as_str()) {
        Some("Complete") => Completeness::Complete,
        Some("Partial") => Completeness::Partial,
        _ => Completeness::Unknown,
    };
    Usage {
        tokens_in,
        tokens_out,
        cost_usd,
        completeness,
    }
}

/// Resolve the recorded immutable `TaskPackage/v1` for an execution from the
/// frozen package store (`<state_dir>/packages/`), never from a live backlog
/// lookup (`agalma-5fy.1`).
///
/// Returns `Ok(None)` when the store holds no package for this execution (a
/// legacy or harness-driven repository build); an ambiguous store (two distinct
/// recorded packages for one execution) is a known failure rather than a guess.
/// A package that fails reference verification is also a known failure.
pub fn resolve_recorded_package(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
) -> Result<Option<TaskPackage>, ActivityError> {
    use crate::task_package::{load_admitted_package, store_root, PACKAGE_HASH_SUBDIR};

    let dir = store_root(state_dir).join(PACKAGE_HASH_SUBDIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ActivityError::KnownFailure(format!(
                "cannot read package store {}: {err}",
                dir.display()
            )))
        }
    };
    let target = execution.as_str();
    let mut digest: Option<String> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if document.get("execution_id").and_then(|v| v.as_str()) != Some(target) {
            continue;
        }
        let Some(found) = document.get("package_digest").and_then(|v| v.as_str()) else {
            continue;
        };
        match &digest {
            Some(existing) if existing != found => {
                return Err(ActivityError::KnownFailure(format!(
                    "multiple distinct recorded packages for execution {target}; refusing to guess"
                )));
            }
            _ => digest = Some(found.to_string()),
        }
    }
    match digest {
        Some(digest) => load_admitted_package(state_dir, &digest)
            .map(Some)
            .map_err(|err| ActivityError::KnownFailure(err.to_string())),
        None => Ok(None),
    }
}

/// Render the per-attempt builder prompt for one dispatch.
///
/// In repository mode this *receives* the recorded immutable `TaskPackage/v1`
/// (`agalma-5fy.1`); it never re-reads a live backlog and never falls back to
/// the M0 fixture prompt or a partial title/acceptance prompt. Required context
/// is the recorded package plus, within it, the frozen task title and at least
/// one frozen repository instruction object; a missing package or missing
/// required section is a known failure. An explicitly empty optional section
/// (design, notes, handoffs) is valid and renders as an explicit marker.
///
/// `repo == None` is the explicit fixture path: the base prompt text (the
/// fixture prompt materialized by the composition root) is returned unchanged.
pub fn render_builder_prompt(
    state_dir: &Path,
    base: &str,
    repo: Option<&RepoMode>,
    package: Option<&TaskPackage>,
    root: &Path,
    attempt: u32,
) -> Result<String, ActivityError> {
    if repo.is_none() {
        return Ok(base.to_string());
    }
    let Some(package) = package else {
        return Err(ActivityError::KnownFailure(
            "repository-mode build has no recorded TaskPackage; refusing to dispatch without \
             immutable inputs"
                .to_string(),
        ));
    };
    render_repository_builder_prompt(state_dir, base, package, root, attempt)
}

/// Render the full frozen task context onto the repository base prompt.
fn render_repository_builder_prompt(
    state_dir: &Path,
    base: &str,
    package: &TaskPackage,
    root: &Path,
    attempt: u32,
) -> Result<String, ActivityError> {
    if package.definition.title.trim().is_empty() {
        return Err(ActivityError::KnownFailure(
            "recorded TaskPackage has no task title; refusing to render the builder prompt"
                .to_string(),
        ));
    }
    let instructions: Vec<&ContextItem> = package
        .context
        .iter()
        .filter(|item| item.kind == ContextKind::Instruction)
        .collect();
    if instructions.is_empty() {
        return Err(ActivityError::KnownFailure(
            "recorded TaskPackage has no frozen repository instructions; refusing to substitute \
             the M0 base prompt"
                .to_string(),
        ));
    }

    let mut out = String::new();
    out.push_str(base);
    out.push_str("\n\n---\n\n# Frozen task context\n\n");
    out.push_str(
        "This context was recorded before dispatch and is immutable. Do not re-read the \
         backlog, and do not infer the task from its title. The sections below are \
         authoritative.\n\n",
    );

    out.push_str("## Task identity\n\n");
    out.push_str(&format!("- task: `{}`\n", package.task_id));
    out.push_str(&format!("- execution: `{}`\n", package.execution_id));
    out.push_str(&format!(
        "- directive: `{}` v{}\n",
        package.directive.id, package.directive.version
    ));
    out.push_str(&format!(
        "- repository: `{}` at `{}`\n\n",
        package.repository.identity, package.repository.base_sha
    ));

    out.push_str("## Scope, files, contract, invariants, and non-goals\n\n");
    out.push_str(package.definition.description.text.trim_end());
    out.push_str("\n\n");

    render_optional_section(&mut out, "## Design", &package.definition.design.text);
    render_optional_section(&mut out, "## Notes", &package.definition.notes.text);

    for (heading, kind) in [
        ("## Shared design", ContextKind::SharedDesign),
        ("## Dependency handoffs", ContextKind::DependencyHandoff),
        ("## Parent design", ContextKind::ParentDesign),
    ] {
        let items: Vec<&ContextItem> = package
            .context
            .iter()
            .filter(|item| item.kind == kind)
            .collect();
        render_context_items(state_dir, &mut out, heading, &items)?;
    }

    render_context_items(
        state_dir,
        &mut out,
        "## Repository rules and instructions (immutable)",
        &instructions,
    )?;

    out.push_str("## Permitted change scope\n\n");
    render_path_list(&mut out, "Allowed paths", &package.scope.allowed_paths);
    render_path_list(
        &mut out,
        "Protected paths (do not modify)",
        &package.scope.protected_paths,
    );
    render_path_list(
        &mut out,
        "Readable artifacts (immutable)",
        &package.scope.readable_artifacts,
    );
    out.push('\n');

    out.push_str("## Acceptance commands (each must exit 0)\n\n");
    if package.definition.acceptance.is_empty() {
        out.push_str("(no acceptance commands recorded)\n\n");
    } else {
        for (index, command) in package.definition.acceptance.iter().enumerate() {
            out.push_str(&format!("{}. `{command}`\n", index + 1));
        }
        out.push('\n');
    }

    if !package.definition.machine_block.trim().is_empty() {
        out.push_str("## Frozen directive block\n\n```yaml\n");
        out.push_str(package.definition.machine_block.trim_end());
        out.push_str("\n```\n\n");
    }

    if attempt > 1 {
        render_retry_context(&mut out, root, attempt);
    }

    Ok(out)
}

/// Render one optional inline section, marking an explicitly empty one.
fn render_optional_section(out: &mut String, heading: &str, text: &str) {
    out.push_str(heading);
    out.push_str("\n\n");
    if text.trim().is_empty() {
        out.push_str("(empty; none was recorded)\n\n");
    } else {
        out.push_str(text.trim_end());
        out.push_str("\n\n");
    }
}

/// Render a group of by-reference context objects, verifying each object digest.
fn render_context_items(
    state_dir: &Path,
    out: &mut String,
    heading: &str,
    items: &[&ContextItem],
) -> Result<(), ActivityError> {
    out.push_str(heading);
    out.push_str("\n\n");
    if items.is_empty() {
        out.push_str("(none recorded)\n\n");
        return Ok(());
    }
    for item in items {
        let bytes =
            crate::task_package::read_artifact(state_dir, &item.artifact).map_err(|err| {
                ActivityError::KnownFailure(format!(
                    "required context object `{}` is unreadable: {err}",
                    item.artifact.path
                ))
            })?;
        out.push_str(&format!(
            "### `{}` (artifact `{}`, sha256 `{}`)\n\n",
            item.source, item.artifact.path, item.artifact.digest
        ));
        out.push_str("```text\n");
        out.push_str(String::from_utf8_lossy(&bytes).trim_end());
        out.push_str("\n```\n\n");
    }
    Ok(())
}

/// Render a labelled list of paths (an empty list is stated explicitly).
fn render_path_list(out: &mut String, label: &str, paths: &[String]) {
    if paths.is_empty() {
        out.push_str(&format!("- {label}: (none)\n"));
    } else {
        out.push_str(&format!("- {label}:\n"));
        for path in paths {
            out.push_str(&format!("  - `{path}`\n"));
        }
    }
}

/// Append the file-only retry handoff: the prior captured failure and verifier
/// diagnosis, referenced by path and inlined so the builder can re-read them.
fn render_retry_context(out: &mut String, root: &Path, attempt: u32) {
    let previous = attempt - 1;
    let failure = root
        .join("artifacts")
        .join(format!("failure@{previous}.txt"));
    if let Ok(text) = std::fs::read_to_string(&failure) {
        out.push_str(&format!(
            "## Previous attempt failure\n\nArtifact (read it directly): `{}`\n\n```text\n",
            failure.display()
        ));
        out.push_str(text.trim_end());
        out.push_str("\n```\n\n");
    }
    let diagnosis = root
        .join("artifacts")
        .join(format!("diagnosis@{previous}.md"));
    if let Ok(text) = std::fs::read_to_string(&diagnosis) {
        out.push_str(&format!(
            "## Verifier diagnosis\n\nArtifact (read it directly): `{}`\n\n",
            diagnosis.display()
        ));
        out.push_str(text.trim_end());
        out.push_str("\n\n");
    }
}

/// Render the per-attempt verifier prompt: the base verifier prompt plus the
/// file-only input paths and the absolute diagnosis output path.
fn render_verifier_prompt(base: &str, deps: &PhaseDeps, root: &Path, attempt: u32) -> String {
    // The diagnosis inputs are parent-owned under `<run>/artifacts` and readable
    // through the verifier's read-only run-root PROTECTED grant; the diagnosis
    // output is written inside the verifier's narrow ATTEMPT runtime, then the
    // parent persists it under artifacts.
    let artifacts = root.join("artifacts");
    let diagnosis = root
        .join("verify-runtime")
        .join(format!("diagnosis@{attempt}.md"));
    let mut out = String::new();
    out.push_str(base);
    out.push_str("\n\n---\n\n## This attempt\n\n");
    if let Some(repo) = &deps.repo {
        out.push_str(&format!("Task title: {}\n\n", repo.task_title));
    }
    out.push_str(&format!(
        "Attempt: {attempt}\n\nInputs (read both):\n\
         - candidate diff: `{}`\n\
         - captured failure: `{}`\n\n\
         Do not modify the repository. Write your diagnosis to:\n\
         `{}`\n",
        artifacts.join(format!("diff@{attempt}.patch")).display(),
        artifacts.join(format!("failure@{attempt}.txt")).display(),
        diagnosis.display(),
    ));
    out
}

/// Deterministic scripted diagnosis (no model call). Emits the four verifier
/// sections so the retry prompt and tests can rely on the format.
fn render_scripted_diagnosis(attempt: u32, failure: &str, diff: &str) -> String {
    let evidence = failure.lines().take(20).collect::<Vec<_>>().join("\n");
    let diff_lines = diff.lines().count();
    format!(
        "# Verifier diagnosis (attempt {attempt})\n\n\
         ## cause\n\nThe candidate change does not satisfy the acceptance command: the \
         acceptance output shows the expected marker is absent from the fixed source.\n\n\
         ## suggested fix\n\nMake the minimal source change the acceptance command \
         checks for (see the captured failure). Do not weaken the acceptance test.\n\n\
         ## evidence\n\n```text\n{evidence}\n```\n\nCandidate diff: {diff_lines} line(s).\n\n\
         ## confidence\n\n0.6 — derived deterministically from the captured failure.\n",
    )
}

/// Write `artifacts/diff@<attempt>.patch`: the candidate working tree against the
/// recorded base SHA (`git diff <base>` in the checkout).
fn write_candidate_diff(
    checkout: &Checkout,
    artifacts: &Path,
    attempt: u32,
) -> Result<PathBuf, ActivityError> {
    let out = Command::new("git")
        .arg("--no-pager")
        .args(["diff", checkout.base_sha.as_str(), "--no-color"])
        .current_dir(&checkout.path)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ActivityError::KnownFailure(format!("git diff candidate: {e}")))?;
    if !out.status.success() {
        return Err(ActivityError::KnownFailure(format!(
            "git diff candidate failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let path = artifacts.join(format!("diff@{attempt}.patch"));
    std::fs::write(&path, &out.stdout)
        .map_err(|e| ActivityError::KnownFailure(format!("write diff {}: {e}", path.display())))?;
    Ok(path)
}

/// Refuse to integrate when the candidate working tree is dirty or carries
/// untracked files (SF.6): `integrate_origin` commits the dirty tree, so a
/// change made after verification would ship an unverified SHA.
fn ensure_clean_worktree(checkout: &Checkout) -> Result<(), ActivityError> {
    let out = Command::new("git")
        .arg("--no-pager")
        .args(["status", "--porcelain"])
        .current_dir(&checkout.path)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ActivityError::KnownFailure(format!("git status candidate: {e}")))?;
    if !out.status.success() {
        return Err(ActivityError::KnownFailure(format!(
            "git status candidate failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
        return Err(ActivityError::KnownFailure(
            "candidate working tree changed after verification (dirty or untracked); refusing \
             to integrate an unverified tree"
                .to_string(),
        ));
    }
    Ok(())
}

/// A fully-known zero usage (Rust-run acceptance and the scripted verifier).
pub(crate) fn zero_usage() -> Usage {
    Usage {
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        completeness: Completeness::Complete,
    }
}

/// Record a per-attempt `usage` execution event (cost/wall-time attribution for
/// the M1.4 digest). Events are additive and ignored by the phase fold.
#[allow(clippy::too_many_arguments)]
fn record_usage_event(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
    operation_id: &OperationId,
    phase: &str,
    role: &str,
    model: &str,
    route: &str,
    attempt: u32,
    usage: Usage,
    wall_ms: u64,
) -> Result<(), ActivityError> {
    // H3/H6: attribute cash vs subscription quota separately. For paid routes
    // the provider-reported per-turn cost is the real cash figure; free models
    // are (0, 0); Go subscription quota is the notional table cost.
    let (cash_usd, quota_usd) = crate::routing::cash_quota_for_usage(
        model,
        usage.cost_usd,
        usage.tokens_in,
        usage.tokens_out,
    );
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).map_err(contract_err)?;
    let event = agalma_contracts::ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: "usage".to_string(),
        payload: json!({
            "operation_id": operation_id,
            "phase": phase,
            "role": role,
            "model": model,
            "route": route,
            "attempt": attempt,
            "tokens_in": usage.tokens_in,
            "tokens_out": usage.tokens_out,
            "cost_usd": usage.cost_usd,
            "cash_usd": cash_usd,
            "quota_usd": quota_usd,
            "completeness": usage.completeness.label(),
            "wall_ms": wall_ms,
        }),
        recorded_at_unix_ms: now_ms(),
    };
    ledger
        .commit(agalma_contracts::CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![event],
            receipts: vec![],
            intents: vec![],
        })
        .map_err(contract_err)?;
    Ok(())
}

/// Record a `provider_health` event for a stalled/failed session (H1). Events
/// are additive and ignored by the phase fold; the digest derives a stall count
/// from them.
pub fn record_provider_health(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
    model: &str,
    binding: &str,
    attempt: u32,
    stall: &PollStall,
) -> Result<(), ActivityError> {
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).map_err(contract_err)?;
    let event = agalma_contracts::ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: "provider_health".to_string(),
        payload: json!({
            "model": model,
            "binding": binding,
            "attempt": attempt,
            "reason": stall.reason.as_str(),
            "idle_ms": stall.idle_ms,
            "tokens": stall.tokens,
            "wall_ms": stall.wall_ms,
        }),
        recorded_at_unix_ms: now_ms(),
    };
    ledger
        .commit(agalma_contracts::CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![event],
            receipts: vec![],
            intents: vec![],
        })
        .map_err(contract_err)?;
    Ok(())
}

/// `max_attempts` recorded in the execution's preparation/creation event.
fn execution_max_attempts(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
) -> Result<u32, ActivityError> {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).map_err(contract_err)?;
    for event in ledger.events(execution).map_err(contract_err)? {
        if matches!(
            event.kind.as_str(),
            "execution_prepared" | "execution_created" | "execution_activated"
        ) {
            return Ok(event
                .payload
                .get("max_attempts")
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as u32);
        }
    }
    Err(ActivityError::KnownFailure(
        "no execution preparation/creation event for retry decision".to_string(),
    ))
}

/// Record the `retry.escalate` decision for a red verify through the pinned
/// static baseline. In M1 escalation is a recorded no-op (same model): with
/// `tier == max_tier == 0` the baseline retries while attempts remain and parks
/// once they are exhausted.
fn record_retry_decision(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
    attempt: u32,
    max_attempts: u32,
) -> Result<(), ActivityError> {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).map_err(contract_err)?;
    let mut decider = StaticDecider::new(ledger);
    let options: Vec<DecisionOption> = ["retry", "escalate", "park"]
        .iter()
        .map(|id| DecisionOption {
            option_id: (*id).to_string(),
            attributes: json!({ "action": id }),
        })
        .collect();
    let request = DecisionRequest {
        version: DECISION_API_VERSION,
        operation_id: OperationId::new(format!("op:retry:{execution}:{attempt}")),
        kind: DecisionKind::RetryEscalate,
        context: json!({
            "attempt": attempt,
            "max_attempts": max_attempts,
            "tier": 0,
            "max_tier": 0,
            "verdict": "retryable",
        }),
        artifacts: vec![
            ArtifactRef::derive(&format!("failure@{attempt}")),
            ArtifactRef::derive(&format!("diagnosis@{attempt}")),
        ],
        options,
        deadline_unix_ms: 0,
        pin: DecisionPin {
            policy: RETRY_ESCALATE_BASELINE.to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    };
    let outcome = decider
        .decide(&request, DecisionAttempt::Baseline)
        .map_err(contract_err)?;
    eprintln!("agalma: retry.escalate attempt {attempt}/{max_attempts} -> {outcome:?}");
    Ok(())
}

/// Write `artifacts/postmortem.md` when a red verify exhausts `max_attempts`:
/// attempts, per-attempt verdicts, retry decisions, and recorded cost.
fn write_postmortem(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
    attempt: u32,
    max_attempts: u32,
) -> Result<PathBuf, ActivityError> {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).map_err(contract_err)?;
    let events = ledger.events(execution).map_err(contract_err)?;

    let mut out = String::new();
    out.push_str(&format!("# Postmortem — {execution}\n\n"));
    out.push_str(&format!(
        "Parked after {attempt} of {max_attempts} attempts: the acceptance commands did not pass.\n\n"
    ));

    out.push_str("## Attempts\n\n");
    out.push_str("| attempt | phase | verdict | wall_ms |\n|---|---|---|---|\n");
    for event in &events {
        if event.kind != "usage" {
            continue;
        }
        let n = event
            .payload
            .get("attempt")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let phase = event
            .payload
            .get("phase")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let role = event
            .payload
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let wall = event
            .payload
            .get("wall_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push_str(&format!("| {n} | {phase} ({role}) | recorded | {wall} |\n"));
    }

    out.push_str("\n## Verdicts\n\n");
    for n in 1..=attempt {
        let receipt = ledger
            .operation_receipt(&OperationId::derive(execution, &format!("verify@{n}")))
            .map_err(contract_err)?;
        let verdict = receipt
            .as_ref()
            .and_then(|r| r.result.pointer("/result/status"))
            .and_then(|v| v.as_str())
            .unwrap_or(if n == attempt { "red" } else { "unknown" });
        out.push_str(&format!("- attempt {n}: verify = {verdict}\n"));
    }

    out.push_str("\n## Decisions\n\n");
    for n in 1..=attempt {
        let op = OperationId::new(format!("op:retry:{execution}:{n}"));
        let outcome = ledger.decision_outcome(&op).map_err(contract_err)?;
        out.push_str(&format!("- attempt {n}: retry.escalate = {outcome:?}\n"));
    }

    let cost: f64 = events
        .iter()
        .filter(|e| e.kind == "usage")
        .filter_map(|e| e.payload.get("cost_usd").and_then(|v| v.as_f64()))
        .sum();
    let tokens_in: u64 = events
        .iter()
        .filter(|e| e.kind == "usage")
        .filter_map(|e| e.payload.get("tokens_in").and_then(|v| v.as_u64()))
        .sum();
    let tokens_out: u64 = events
        .iter()
        .filter(|e| e.kind == "usage")
        .filter_map(|e| e.payload.get("tokens_out").and_then(|v| v.as_u64()))
        .sum();
    out.push_str("\n## Cost\n\n");
    out.push_str(&format!(
        "- total cost_usd: {cost:.6}\n- total tokens_in: {tokens_in}\n- total tokens_out: {tokens_out}\n"
    ));

    let artifacts = canonical_dir(&run_root(state_dir, execution).join("artifacts"))?;
    let path = artifacts.join("postmortem.md");
    std::fs::write(&path, out).map_err(|e| {
        ActivityError::KnownFailure(format!("write postmortem {}: {e}", path.display()))
    })?;
    Ok(path)
}

/// Current unix time in milliseconds.
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Why a polled session stopped making progress (H1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollStallReason {
    /// No token/state delta for the idle bound.
    Idle,
    /// The remote session is gone (404 / connection refused).
    SessionGone,
    /// The session ended in a terminal failure (not a progress stall).
    Error,
}

impl PollStallReason {
    /// Stable label (`idle` | `session_gone` | `error`).
    pub fn as_str(self) -> &'static str {
        match self {
            PollStallReason::Idle => "idle",
            PollStallReason::SessionGone => "session_gone",
            PollStallReason::Error => "error",
        }
    }
}

/// Details of a stalled/failed session, used for the `provider_health` event
/// and the `stalled` verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct PollStall {
    pub reason: PollStallReason,
    pub idle_ms: u64,
    pub tokens: u64,
    pub wall_ms: u64,
    /// Last observed state (or the transport error) rendering.
    pub last_state: String,
}

impl PollStall {
    /// A terminal failure (not a progress stall), recorded for provider health.
    pub fn failed(last_state: impl Into<String>, wall_ms: u64, tokens: u64) -> Self {
        PollStall {
            reason: PollStallReason::Error,
            idle_ms: 0,
            tokens,
            wall_ms,
            last_state: last_state.into(),
        }
    }
}

/// Outcome of driving one operation to a terminal state.
#[derive(Clone, Debug, PartialEq)]
pub enum PollOutcome {
    /// A terminal state was observed.
    Terminal(OperationState),
    /// The session stopped progressing (idle watchdog or transport loss).
    Stalled(PollStall),
    /// The hard wall-clock bound elapsed without a terminal state (existing
    /// behavior: advance to verify / record zero usage).
    TimedOut,
    /// A stop was requested (STOP sentinel or kill latch) while the operation
    /// was running: the caller must cancel/close the session (`agalma-r2l`).
    Stopped,
}

/// Progress fingerprint: the operation status plus usage/token deltas and
/// event/step count observed through `inspect_operation` + `read_events`. Any
/// change resets the idle clock.
#[derive(Clone, PartialEq, Eq)]
struct ProgressFingerprint {
    state: String,
    tokens_in: u64,
    tokens_out: u64,
    cost_micros: i64,
    steps: usize,
}

impl ProgressFingerprint {
    fn tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

fn progress_fingerprint<H: HarnessApi + ?Sized>(
    harness: &mut H,
    op: &OperationHandle,
    state: &OperationState,
) -> ProgressFingerprint {
    let mut fingerprint = ProgressFingerprint {
        state: state.label(),
        tokens_in: 0,
        tokens_out: 0,
        cost_micros: 0,
        steps: 0,
    };
    match harness.read_events(op) {
        Ok(events) => {
            fingerprint.steps = events.len();
            for event in events {
                if let Event::UsageSnapshot { usage } = event {
                    fingerprint.tokens_in += usage.tokens_in;
                    fingerprint.tokens_out += usage.tokens_out;
                    fingerprint.cost_micros += (usage.cost_usd * 1_000_000.0).round() as i64;
                }
            }
        }
        Err(err) => {
            // A best-effort progress read: an unreadable log is not progress.
            eprintln!("agalma: read_events: {err}");
        }
    }
    fingerprint
}

/// Whether a poll error is a terminal transport failure (session gone): the
/// remote session is unreachable, so waiting for the deadline is pointless.
fn transport_error_is_terminal(err: &ContractError) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("-> 404")
        || text.contains("not found")
        || text.contains("unknown operation")
        || text.contains("unknown session")
        || text.contains("connection refused")
        || text.contains("connection reset")
        || text.contains("broken pipe")
        || text.contains("connection closed")
}

/// Cancel an operation, treating a "session gone" error as benign.
///
/// The conductor cancels *before* closing the session so the interrupt reaches
/// a live session. If the session already ended on its own, the cancel 404s;
/// that is "already gone", not an error, and must not be reported as a failure.
fn cancel_tolerating_gone<H: HarnessApi + ?Sized>(
    harness: &mut H,
    op: &OperationHandle,
    context: &str,
) {
    match harness.cancel_operation(op) {
        Ok(_) => {}
        Err(err) => {
            if transport_error_is_terminal(&err) {
                eprintln!("agalma: {context} cancel: session already gone");
            } else {
                eprintln!("agalma: {context} cancel: {err}");
            }
        }
    }
}

/// Cancel (for non-terminal verdicts) then close the session, in that order.
///
/// The interrupt must reach a live session, so it runs before the DELETE; a
/// terminal outcome needs no cancel. Centralized here so the ordering is
/// provable in one place and identical across build/reference/verifier paths.
fn settle_after_poll<H: HarnessApi + ?Sized>(
    harness: &mut H,
    op: &OperationHandle,
    session: &SessionHandle,
    outcome: &PollOutcome,
    context: &str,
) {
    if matches!(
        outcome,
        PollOutcome::Stalled(_) | PollOutcome::TimedOut | PollOutcome::Stopped
    ) {
        cancel_tolerating_gone(harness, op, context);
    }
    let _ = harness.close_session(session);
}

/// Poll `inspect_operation` until terminal, a stall, the hard wall bound, or a
/// stop request.
///
/// The progress watchdog cancels a session with no token/state delta for
/// `idle_timeout` (verdict `stalled`); a session-not-found/connection-refused
/// transport error fast-fails immediately. Transient poll errors log and keep
/// waiting. `wall_timeout` is the unchanged hard cap. When `stop` reports a
/// stop, the poll returns [`PollOutcome::Stopped`] promptly (within one poll
/// interval) so the caller cancels the session (`agalma-r2l`).
pub fn poll_terminal<H: HarnessApi + ?Sized>(
    harness: &mut H,
    op: &OperationHandle,
    idle_timeout: Duration,
    wall_timeout: Duration,
    stop: Option<&dyn StopControl>,
) -> PollOutcome {
    let started = Instant::now();
    let deadline = started + wall_timeout;
    let mut last: Option<ProgressFingerprint> = None;
    let mut last_change = started;
    let mut tokens = 0u64;
    loop {
        if let Some(stop) = stop {
            if stop.stopped() {
                return PollOutcome::Stopped;
            }
        }
        match harness.inspect_operation(op) {
            Ok(state) if state.is_terminal() => return PollOutcome::Terminal(state),
            Ok(state) => {
                let fingerprint = progress_fingerprint(harness, op, &state);
                tokens = fingerprint.tokens();
                let changed = last.as_ref() != Some(&fingerprint);
                if changed {
                    last = Some(fingerprint);
                    last_change = Instant::now();
                }
                let idle = last_change.elapsed();
                if idle_timeout > Duration::ZERO && idle >= idle_timeout {
                    return PollOutcome::Stalled(PollStall {
                        reason: PollStallReason::Idle,
                        idle_ms: idle.as_millis() as u64,
                        tokens,
                        wall_ms: started.elapsed().as_millis() as u64,
                        last_state: state.label(),
                    });
                }
            }
            Err(err) if transport_error_is_terminal(&err) => {
                return PollOutcome::Stalled(PollStall {
                    reason: PollStallReason::SessionGone,
                    idle_ms: last_change.elapsed().as_millis() as u64,
                    tokens,
                    wall_ms: started.elapsed().as_millis() as u64,
                    last_state: err.to_string(),
                });
            }
            Err(err) => {
                // Transient poll error: log and continue (existing behavior).
                eprintln!("agalma: inspect_operation: {err}");
            }
        }
        if Instant::now() >= deadline {
            return PollOutcome::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Ensure the run-local `CARGO_HOME` (`<run>/cargo-home`) exposes the real cargo
/// registry read-only so `cargo --offline` resolution works inside the sandbox.
///
/// Confined phases run with `HOME` pointed at an empty run-local directory, so
/// cargo defaults to an empty `CARGO_HOME` and offline resolution fails with
/// `no matching package named ... found ... offline mode`. The populated
/// registry lives at `$CARGO_HOME/registry` (or `$HOME/.cargo/registry`) and is
/// already exposed read-only via `extra_ro_roots`; this seeds a
/// `<run>/cargo-home/registry` symlink at it, keeping writes (the package
/// cache/lock) inside the run dir.
///
/// Idempotent: a correct link is left alone; a wrong or self-referential link is
/// replaced. When no real registry exists the helper skips without failing (cargo
/// then reports its own offline miss).
pub fn ensure_cargo_home(run_root: &Path) -> Result<PathBuf, ActivityError> {
    let cargo_home = run_root.join("cargo-home");
    std::fs::create_dir_all(&cargo_home)
        .map_err(|e| ActivityError::KnownFailure(format!("mkdir {}: {e}", cargo_home.display())))?;

    // Resolve the real cargo home: `$CARGO_HOME` when set and non-empty, else
    // `$HOME/.cargo` (mirrors `composition::toolchain`).
    let real_cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    let Some(real_cargo_home) = real_cargo_home else {
        return Ok(cargo_home);
    };
    let real_registry = real_cargo_home.join("registry");
    if !real_registry.is_dir() {
        // Fresh toolchain or vendored setup with no registry cache: nothing to
        // link. Do not fail; the offline miss is cargo's to report.
        return Ok(cargo_home);
    }

    let link = cargo_home.join("registry");
    match std::fs::symlink_metadata(&link) {
        Ok(meta) => {
            let already_correct = meta.file_type().is_symlink()
                && std::fs::canonicalize(&link)
                    .ok()
                    .zip(std::fs::canonicalize(&real_registry).ok())
                    .map(|(a, b)| a == b)
                    .unwrap_or(false);
            if already_correct {
                return Ok(cargo_home);
            }
            // Remove only the stale entry itself: a symlink/file is unlinked, and
            // a real directory is removed only when empty so live registry data
            // is never destroyed.
            let removed = if meta.file_type().is_symlink() || meta.is_file() {
                std::fs::remove_file(&link)
            } else {
                std::fs::remove_dir(&link)
            };
            removed.map_err(|e| {
                ActivityError::KnownFailure(format!(
                    "replace stale cargo registry entry {}: {e}",
                    link.display()
                ))
            })?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ActivityError::KnownFailure(format!(
                "inspect {}: {err}",
                link.display()
            )));
        }
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_registry, &link).map_err(|e| {
        ActivityError::KnownFailure(format!(
            "link cargo registry {} -> {}: {e}",
            link.display(),
            real_registry.display()
        ))
    })?;
    #[cfg(not(unix))]
    {
        let _ = (&real_registry, &link);
    }
    Ok(cargo_home)
}

/// Resolve the `cargo` binary: `$CARGO`/`CARGO` env, then `$CARGO_HOME/bin/cargo`,
/// then `$HOME/.cargo/bin/cargo`, then `PATH`.
fn cargo_bin() -> Result<PathBuf, ActivityError> {
    if let Some(path) = std::env::var_os("CARGO").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
    }
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        candidates.push(PathBuf::from(home).join("bin").join("cargo"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".cargo").join("bin").join("cargo"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("cargo"));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            ActivityError::KnownFailure(
                "cargo binary not found (set CARGO or CARGO_HOME)".to_string(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_testkit::ScratchDir;

    fn deps(repo: Option<RepoMode>) -> PhaseDeps {
        PhaseDeps {
            state_dir: PathBuf::from("state"),
            fixture: PathBuf::from("fixture"),
            model: "test/offline".to_string(),
            verifier_model: "test/offline".to_string(),
            route: "builder:fix".to_string(),
            verifier_route: "verifier:fix".to_string(),
            role: "builder".to_string(),
            profile_path: PathBuf::from("worker.sb"),
            builder_prompt: PathBuf::from("builder.md"),
            proxy_url: None,
            verifier_proxy_url: None,
            paid_proxy: false,
            extra_ro_roots: Vec::new(),
            toolchain_env: BTreeMap::new(),
            turn_timeout: Duration::from_secs(1),
            verify_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            repo,
            harness: crate::harness_binding::harness_handle(
                crate::harness_binding::HarnessChoice::OpenCode,
            ),
            stop: Rc::new(crate::stop::FileStopControl::new(std::path::Path::new(
                "state",
            ))),
        }
    }

    fn scratch_dir(name: &str) -> ScratchDir {
        let dir = ScratchDir::new(&format!("phases-{name}"));
        dir.child("artifacts");
        dir
    }

    /// A long state dir must not push a process-transport harness socket past
    /// `SUN_LEN`; the bounded alias keeps the path short while the socket still
    /// resolves into the state dir.
    #[test]
    fn process_harness_socket_path_is_bounded_for_long_state_dirs() {
        let dir = ScratchDir::new("phases-bounded-sock");
        // A state dir that is, on its own, well past the socket-path limit.
        let long = dir.join("s").join("t".repeat(180));
        std::fs::create_dir_all(&long).expect("long state dir");

        for name in [
            "harness-ref-1.sock",
            "harness-build-2.sock",
            "harness-verify-3.sock",
        ] {
            let path = harness_socket_path(&long, HarnessTransport::Process, name);
            assert!(
                path.as_os_str().len() < SUN_LEN_LIMIT,
                "{name}: {} bytes ({})",
                path.as_os_str().len(),
                path.display()
            );
            assert_eq!(path.file_name().unwrap().to_string_lossy(), name);
            // The real socket file still lands under the state dir.
            assert_eq!(
                std::fs::canonicalize(path.parent().unwrap()).unwrap(),
                std::fs::canonicalize(&long).unwrap(),
                "alias resolves into the state dir"
            );
        }

        // In-process never binds: the direct path is kept.
        let direct = harness_socket_path(&long, HarnessTransport::InProcess, "harness-ref-1.sock");
        assert_eq!(direct, long.join("harness-ref-1.sock"));

        // A short state dir is untouched (no alias, no behavior change).
        let short = Path::new("/agalma/state");
        assert_eq!(
            harness_socket_path(short, HarnessTransport::Process, "harness-ref-1.sock"),
            short.join("harness-ref-1.sock")
        );
    }

    /// `bounded_socket_path` keeps a short direct path, bounds a long one, and
    /// reuses the same symlink alias across calls (stable bounded base).
    #[test]
    fn bounded_socket_path_reuses_alias_and_bounds_long_dirs() {
        let dir = ScratchDir::new("phases-bounded-reuse");
        let long = dir.join("s").join("t".repeat(180));
        std::fs::create_dir_all(&long).expect("long state dir");

        let first = bounded_socket_path(&long, "ledger.sock");
        let second = bounded_socket_path(&long, "workspace.sock");
        assert!(
            first.as_os_str().len() < SUN_LEN_LIMIT,
            "{}",
            first.display()
        );
        assert!(
            second.as_os_str().len() < SUN_LEN_LIMIT,
            "{}",
            second.display()
        );
        assert_eq!(
            first.parent().unwrap(),
            second.parent().unwrap(),
            "both sockets share the same bounded alias"
        );

        // The alias is a symlink that resolves into the state dir; a repeated
        // call reuses the same link (stable per-run base).
        let alias = first.parent().unwrap();
        let meta = std::fs::symlink_metadata(alias).expect("alias metadata");
        assert!(meta.file_type().is_symlink(), "alias is a symlink");
        assert_eq!(std::fs::read_link(alias).unwrap(), long);
        assert_eq!(
            std::fs::canonicalize(alias).unwrap(),
            std::fs::canonicalize(&long).unwrap(),
            "alias resolves into the state dir"
        );

        // A short state dir is untouched (no alias, no behavior change).
        let short = Path::new("/agalma/state");
        assert_eq!(
            bounded_socket_path(short, "ledger.sock"),
            short.join("ledger.sock")
        );
    }

    #[test]
    fn record_usage_event_attributes_paid_cash_and_quota() {
        let dir = scratch_dir("usage-cash");
        let exec = agalma_contracts::ExecutionId::new("exec:usage-cash:1");
        record_usage_event(
            &dir,
            &exec,
            &OperationId::new("op:usage-cash"),
            "build",
            "builder",
            "opencode-go/deepseek-v4.1-flash",
            "builder:fix",
            1,
            Usage {
                tokens_in: 1_000,
                tokens_out: 500,
                cost_usd: 0.0042,
                completeness: Completeness::Complete,
            },
            1_200,
        )
        .expect("record usage");
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let events = ledger.events(&exec).expect("events");
        let usage = events
            .iter()
            .find(|e| e.kind == "usage")
            .expect("usage event");
        let cash = usage.payload["cash_usd"].as_f64().unwrap();
        let quota = usage.payload["quota_usd"].as_f64().unwrap();
        assert!(cash > 0.0, "paid turn cash: {cash}");
        assert!(
            (cash - 0.0042).abs() < 1e-12,
            "reported cost is cash: {cash}"
        );
        assert!(quota > 0.0, "Go allowance quota: {quota}");
    }

    #[test]
    fn record_usage_event_keeps_free_turns_at_zero_cash() {
        let dir = scratch_dir("usage-free");
        let exec = agalma_contracts::ExecutionId::new("exec:usage-free:1");
        record_usage_event(
            &dir,
            &exec,
            &OperationId::new("op:usage-free"),
            "build",
            "builder",
            "opencode/big-pickle",
            "builder:fix",
            1,
            Usage {
                tokens_in: 41_970,
                tokens_out: 3_361,
                cost_usd: 0.0,
                completeness: Completeness::Complete,
            },
            1_000,
        )
        .expect("record usage");
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let events = ledger.events(&exec).expect("events");
        let usage = events
            .iter()
            .find(|e| e.kind == "usage")
            .expect("usage event");
        assert_eq!(usage.payload["cash_usd"].as_f64().unwrap(), 0.0);
        assert_eq!(usage.payload["quota_usd"].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn render_prompt_without_repo_returns_base() {
        let base = "base prompt";
        let text = render_builder_prompt(Path::new("."), base, None, None, Path::new("."), 1)
            .expect("fixture render");
        assert_eq!(text, base);
    }

    #[test]
    fn render_prompt_repo_mode_without_package_is_a_known_failure() {
        let repo = RepoMode {
            origin: PathBuf::from("origin"),
            base_ref: "main".to_string(),
            task_title: "Fix the thing".to_string(),
            acceptance: vec!["sh -c 'true'".to_string(), "cargo test".to_string()],
        };
        let err =
            render_builder_prompt(Path::new("."), "base", Some(&repo), None, Path::new("."), 1)
                .expect_err("repository mode without a recorded package must refuse");
        assert!(matches!(err, ActivityError::KnownFailure(_)), "got {err:?}");
    }

    // -----------------------------------------------------------------------
    // S1 fix (agalma-csw.3): live progress keeps the watchdog honest, and the
    // session is cancelled before it is closed.
    // -----------------------------------------------------------------------

    use agalma_contracts::harness::{CancelAck, Describe, StopEvidence, API_VERSION, HARNESS_API};
    use agalma_contracts::ids::SessionId;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FakeMode {
        /// A running turn whose token counters advance on every poll.
        Progressing,
        /// A running turn with a constant event set (a genuine hang).
        Silent,
    }

    struct FakeTurn {
        mode: FakeMode,
        polls: u32,
        terminal_after: u32,
        /// Ordered record of `cancel`/`close` calls, to assert the fix's order.
        calls: Vec<&'static str>,
        /// When set, `cancel_operation` returns a session-gone 404.
        cancel_gone: bool,
        /// When set, create the `STOP` sentinel at this path once `polls`
        /// reaches the threshold (drives the stop-poll cancellation test).
        stop_file: Option<(PathBuf, u32)>,
    }

    impl FakeTurn {
        fn new(mode: FakeMode, terminal_after: u32) -> Self {
            FakeTurn {
                mode,
                polls: 0,
                terminal_after,
                calls: Vec::new(),
                cancel_gone: false,
                stop_file: None,
            }
        }

        fn op() -> OperationHandle {
            OperationHandle {
                id: OperationId::new("op:progress:turn/1"),
            }
        }

        fn session() -> SessionHandle {
            SessionHandle {
                id: SessionId::derive(&AttemptId::derive(1), 1),
            }
        }
    }

    impl HarnessApi for FakeTurn {
        fn describe(&self) -> Describe {
            Describe {
                api: HARNESS_API.to_string(),
                api_version: API_VERSION,
                impl_name: "fake".to_string(),
                impl_version: "fake/v1".to_string(),
                capabilities: Default::default(),
            }
        }

        fn start_attempt(
            &mut self,
            _req: StartAttemptRequest,
        ) -> Result<AttemptHandle, ContractError> {
            Ok(AttemptHandle {
                id: AttemptId::derive(1),
            })
        }

        fn create_session(
            &mut self,
            _req: CreateSessionRequest,
        ) -> Result<SessionHandle, ContractError> {
            Ok(FakeTurn::session())
        }

        fn run_turn(
            &mut self,
            _session: &SessionHandle,
            _req: RunTurnRequest,
        ) -> Result<OperationHandle, ContractError> {
            Ok(FakeTurn::op())
        }

        fn inspect_operation(
            &mut self,
            _op: &OperationHandle,
        ) -> Result<OperationState, ContractError> {
            self.polls += 1;
            if let Some((path, after)) = &self.stop_file {
                if self.polls >= *after {
                    let _ = std::fs::write(path, b"stop");
                }
            }
            if self.polls >= self.terminal_after {
                return Ok(OperationState::Completed {
                    result_ref: ArtifactRef::derive("turn-result"),
                    usage: Usage {
                        tokens_in: self.polls as u64 * 1000,
                        tokens_out: self.polls as u64,
                        cost_usd: 0.0,
                        completeness: Completeness::Complete,
                    },
                });
            }
            Ok(OperationState::Running)
        }

        fn read_events(&mut self, _op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
            let mut events = vec![Event::TurnStarted];
            if self.mode == FakeMode::Progressing {
                // A partial live snapshot that moves each poll.
                events.push(Event::UsageSnapshot {
                    usage: Usage {
                        tokens_in: self.polls as u64 * 1000,
                        tokens_out: self.polls as u64,
                        cost_usd: 0.0,
                        completeness: Completeness::Partial,
                    },
                });
            }
            Ok(events)
        }

        fn cancel_operation(&mut self, _op: &OperationHandle) -> Result<CancelAck, ContractError> {
            self.calls.push("cancel");
            if self.cancel_gone {
                return Err(ContractError::UnknownOutcome(
                    "GET /api/session/ses_x/interrupt -> 404".to_string(),
                ));
            }
            Ok(CancelAck {
                acknowledged: true,
                terminated: false,
                detail: "fake: interrupted".to_string(),
            })
        }

        fn close_session(&mut self, _session: &SessionHandle) -> Result<(), ContractError> {
            self.calls.push("close");
            Ok(())
        }

        fn stop_attempt(
            &mut self,
            _attempt: &AttemptHandle,
        ) -> Result<StopEvidence, ContractError> {
            Ok(StopEvidence {
                process_group_gone: true,
                detail: "fake: stopped".to_string(),
            })
        }
    }

    #[test]
    fn progressing_turn_never_false_stalls() {
        // Terminal on the ~5th poll (~2 s) with a 1.2 s idle bound: a constant
        // fingerprint would stall first; the moving partial snapshot keeps it
        // alive to a real terminal state.
        let mut harness = FakeTurn::new(FakeMode::Progressing, 5);
        let op = FakeTurn::op();
        let started = Instant::now();
        let outcome = poll_terminal(
            &mut harness,
            &op,
            Duration::from_millis(1200),
            Duration::from_secs(30),
            None,
        );
        match outcome {
            PollOutcome::Terminal(OperationState::Completed { usage, .. }) => {
                assert_eq!(usage.tokens_in, 5000, "terminal usage preserved");
            }
            other => panic!(
                "expected completion, got {other:?} after {:?}",
                started.elapsed()
            ),
        }
    }

    #[test]
    fn silent_turn_with_same_bound_still_stalls() {
        // Control: without the progress signal the same turn is (correctly)
        // classified idle — the watchdog semantics are preserved.
        let mut harness = FakeTurn::new(FakeMode::Silent, 5);
        let op = FakeTurn::op();
        let outcome = poll_terminal(
            &mut harness,
            &op,
            Duration::from_millis(1200),
            Duration::from_secs(30),
            None,
        );
        match outcome {
            PollOutcome::Stalled(stall) => {
                assert_eq!(stall.reason, PollStallReason::Idle, "{stall:?}");
            }
            other => panic!("expected idle stall, got {other:?}"),
        }
    }

    #[test]
    fn non_terminal_verdict_cancels_before_close() {
        let mut harness = FakeTurn::new(FakeMode::Silent, 5);
        let op = FakeTurn::op();
        let session = FakeTurn::session();
        let stall = PollOutcome::Stalled(PollStall {
            reason: PollStallReason::Idle,
            idle_ms: 120_000,
            tokens: 0,
            wall_ms: 120_000,
            last_state: "Running".to_string(),
        });
        settle_after_poll(&mut harness, &op, &session, &stall, "test");
        assert_eq!(
            harness.calls,
            vec!["cancel", "close"],
            "interrupt must precede the DELETE"
        );
    }

    #[test]
    fn stop_sentinel_during_poll_cancels_within_bound() {
        // agalma-r2l: a STOP sentinel created while the turn runs must cancel
        // the poll promptly (within the poll interval), and the sentinel must
        // remain (no implicit clearing anywhere).
        let dir = scratch_dir("stop-during-poll");
        let stop_path = dir.join("STOP");
        let mut harness = FakeTurn::new(FakeMode::Silent, 1_000);
        harness.stop_file = Some((stop_path.clone(), 1));
        let control = crate::stop::FileStopControl::new(&dir);
        let op = FakeTurn::op();
        let started = Instant::now();
        let outcome = poll_terminal(
            &mut harness,
            &op,
            Duration::from_secs(120),
            Duration::from_secs(600),
            Some(&control),
        );
        let elapsed = started.elapsed();
        match outcome {
            PollOutcome::Stopped => {}
            other => panic!("expected stop, got {other:?} after {elapsed:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "stop cancellation bounded by the poll interval: {elapsed:?}"
        );
        assert!(
            stop_path.exists(),
            "stop state retained (never implicitly cleared)"
        );
    }

    #[test]
    fn injected_stop_control_drives_poll_terminal() {
        // The phase poll consumes the control injected through PhaseDeps
        // (`agalma-5fy.5`) rather than constructing a second FileStopControl.
        struct AlwaysStopped;
        impl StopControl for AlwaysStopped {
            fn stopped(&self) -> bool {
                true
            }
        }
        let mut deps = deps(None);
        deps.stop = Rc::new(AlwaysStopped);
        let mut harness = FakeTurn::new(FakeMode::Silent, 5);
        let op = FakeTurn::op();
        let outcome = poll_terminal(
            &mut harness,
            &op,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Some(&*deps.stop),
        );
        assert!(
            matches!(outcome, PollOutcome::Stopped),
            "the injected control drives the poll: {outcome:?}"
        );
    }

    #[test]
    fn stopped_verdict_cancels_before_close() {
        // The shared settle path treats a stop like a stall: cancel then close.
        let mut harness = FakeTurn::new(FakeMode::Silent, 5);
        let op = FakeTurn::op();
        let session = FakeTurn::session();
        settle_after_poll(&mut harness, &op, &session, &PollOutcome::Stopped, "test");
        assert_eq!(harness.calls, vec!["cancel", "close"]);
    }

    #[test]
    fn terminal_verdict_closes_without_cancelling() {
        let mut harness = FakeTurn::new(FakeMode::Silent, 5);
        let op = FakeTurn::op();
        let session = FakeTurn::session();
        let done = PollOutcome::Terminal(OperationState::Completed {
            result_ref: ArtifactRef::derive("turn-result"),
            usage: Usage {
                tokens_in: 1,
                tokens_out: 1,
                cost_usd: 0.0,
                completeness: Completeness::Complete,
            },
        });
        settle_after_poll(&mut harness, &op, &session, &done, "test");
        assert_eq!(harness.calls, vec!["close"]);
    }

    #[test]
    fn cancel_404_is_benign_and_still_closes() {
        // A session that ended on its own (or our own close) 404s the cancel;
        // that is "already gone", not a failure, and the close still runs.
        let mut harness = FakeTurn::new(FakeMode::Silent, 5);
        harness.cancel_gone = true;
        let op = FakeTurn::op();
        let session = FakeTurn::session();
        let stall = PollOutcome::Stalled(PollStall {
            reason: PollStallReason::SessionGone,
            idle_ms: 10,
            tokens: 0,
            wall_ms: 20,
            last_state: "gone".to_string(),
        });
        settle_after_poll(&mut harness, &op, &session, &stall, "test");
        assert_eq!(harness.calls, vec!["cancel", "close"]);
    }
}
