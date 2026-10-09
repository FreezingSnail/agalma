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
//!   `<run>/verify/` (its own Git metadata), and each task acceptance command
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
    AttemptHandle, Completeness, CreateSessionRequest, HarnessApi, Limits, OperationHandle,
    OperationState, RunTurnRequest, StartAttemptRequest, Usage,
};
use agalma_contracts::ids::{ArtifactRef, AttemptId};
use agalma_contracts::sandbox::LaunchSpec;
use agalma_contracts::{
    Checkout, ContractError, DecisionApi, DecisionAttempt, DecisionKind, DecisionOption,
    DecisionPin, DecisionRequest, LedgerApi, OperationId, WorkspaceApi, DECISION_API_VERSION,
};
use agalma_decision::{StaticDecider, RETRY_ESCALATE_BASELINE};
use agalma_execution::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, OperationContext,
};
use agalma_harness_opencode::{OpenCodeHarness, OpenCodeHarnessConfig};
use agalma_ledger::SqliteLedger;
use agalma_sandbox::{SandboxOutput, SeatbeltSandbox};
use agalma_workspace::Workspace;
use serde_json::json;

use crate::workers::{self, Survivor};

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
    pub model: String,
    pub role: String,
    /// Rendered, fail-closed sandbox profile.
    pub profile_path: PathBuf,
    /// Builder prompt file (path passed to the harness as `prompts_ref`).
    pub builder_prompt: PathBuf,
    /// Parent-owned provider proxy URL handed to the confined harness.
    pub proxy_url: Option<String>,
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
    /// Repo mode (M1): when set, checkout clones `origin` at `base_ref` and
    /// verify runs the task's acceptance commands instead of `cargo test`.
    pub repo: Option<RepoMode>,
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
    workspace: Rc<RefCell<Workspace>>,
    state: Rc<RefCell<RunState>>,
) -> PhaseActivities {
    activities_with_mode(deps, workspace, state, ActivityMode::from_env())
}

/// Build the five phase activities over shared state with an explicit mode.
pub fn activities_with_mode(
    deps: PhaseDeps,
    workspace: Rc<RefCell<Workspace>>,
    state: Rc<RefCell<RunState>>,
    mode: ActivityMode,
) -> PhaseActivities {
    let build: Box<dyn Activity> = match mode {
        ActivityMode::Live => Box::new(BuildActivity {
            deps: deps.clone(),
            state: Rc::clone(&state),
        }),
        ActivityMode::Scripted => Box::new(ScriptedBuildActivity {
            deps: deps.clone(),
            state: Rc::clone(&state),
        }),
    };
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
        })
    } else {
        match mode {
            ActivityMode::Live => Box::new(VerifyActivity {
                deps: deps.clone(),
                workspace: Rc::clone(&workspace),
                state: Rc::clone(&state),
                sandboxed: true,
                scripted: false,
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

/// Create `path` if needed and return its canonical form (Seatbelt parameters
/// must be absolute, resolved paths).
fn canonical_dir(path: &Path) -> Result<PathBuf, ActivityError> {
    std::fs::create_dir_all(path)
        .map_err(|e| ActivityError::KnownFailure(format!("mkdir {}: {e}", path.display())))?;
    std::fs::canonicalize(path)
        .map_err(|e| ActivityError::KnownFailure(format!("resolve {}: {e}", path.display())))
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
    workspace: Rc<RefCell<Workspace>>,
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
    fn run_turn(&self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)?;
        let root = run_root(&self.deps.state_dir, &ctx.execution_id);
        std::fs::create_dir_all(&root)
            .map_err(|e| ActivityError::KnownFailure(format!("create run root: {e}")))?;
        let protected = canonical_dir(&root.join("protected"))?;
        let sock = canonical_dir(&root.join("sock"))?;
        // Parameterize the base builder prompt per attempt: the model sees the
        // task title, the acceptance commands, and the captured failure text (or
        // a verifier diagnosis artifact when M1.3 writes one).
        let prompt_path = write_builder_prompt(&self.deps, &root, ctx)?;

        let mut config =
            OpenCodeHarnessConfig::new(&self.deps.profile_path, &root, &protected, &sock);
        config.model = Some(self.deps.model.clone());
        config.proxy_url = self.deps.proxy_url.clone();
        config.extra_ro_roots = self.deps.extra_ro_roots.clone();
        let mut env = self.deps.toolchain_env.clone();
        env.insert(
            "CARGO_HOME".to_string(),
            root.join("cargo-home").to_string_lossy().into_owned(),
        );
        // Keep build artifacts out of the checkout so the candidate commit is
        // the source change, not a multi-megabyte `target/` tree.
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            root.join("agent-target").to_string_lossy().into_owned(),
        );
        config.extra_env = env;

        let mut harness = OpenCodeHarness::new(Box::new(SeatbeltSandbox::new()), config);
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
        let outcome = self.drive(&mut harness, &attempt, ctx, &prompt_path);
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

    fn drive(
        &self,
        harness: &mut OpenCodeHarness,
        _attempt: &AttemptHandle,
        ctx: &OperationContext,
        prompt_path: &Path,
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
        let state = poll_terminal(harness, &op, self.deps.turn_timeout);
        let _ = harness.close_session(&session);

        let failure = ArtifactRef::derive(&format!("build@{}", ctx.attempt));
        match state {
            Some(OperationState::Completed { usage, .. }) => Ok(ActivityOutcome {
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
            Some(OperationState::Failed { reason }) => {
                eprintln!("agalma: build attempt {} failed: {reason}", ctx.attempt);
                Ok(ActivityOutcome {
                    result: json!({ "status": "failed", "reason": reason, "attempt": ctx.attempt }),
                    next: ActivityNext::Retry { failure },
                })
            }
            Some(OperationState::Running) | Some(OperationState::Unknown) => Ok(ActivityOutcome {
                result: json!({ "status": "unknown", "attempt": ctx.attempt }),
                next: ActivityNext::Retry { failure },
            }),
            None => {
                // The turn did not reach a terminal state within the bound
                // (free models can be chatty or slow). We cannot trust the turn
                // status, so we stop it and let the *verify* phase arbitrate the
                // candidate tree: a red verify drives the executor's bounded
                // build retry, a green verify integrates. This avoids discarding
                // a working edit just because the model never signed off.
                if let Err(err) = harness.cancel_operation(&op) {
                    eprintln!("agalma: build cancel after timeout: {err}");
                }
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
        }
    }
}

impl Activity for BuildActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        self.run_turn(ctx)
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        build_probe(&self.deps, &ctx.execution_id, ctx.attempt)
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        build_reconcile(&self.deps, &ctx.execution_id, ctx.attempt)
    }
}

// ---------------------------------------------------------------------------
// build (scripted)
// ---------------------------------------------------------------------------

/// Deterministic build stand-in for the crash matrix: spawns a real `sleep`
/// child as the recorded worker, applies the fixture fix, optionally sleeps (so
/// a test can SIGKILL the conductor), then records completion.
struct ScriptedBuildActivity {
    deps: PhaseDeps,
    state: Rc<RefCell<RunState>>,
}

impl Activity for ScriptedBuildActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
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
    workspace: Rc<RefCell<Workspace>>,
    state: Rc<RefCell<RunState>>,
    /// Run acceptance commands under Seatbelt (`Live`) or in-process
    /// (`Scripted`, deterministic tests).
    sandboxed: bool,
    /// Scripted mode replaces the fresh verifier diagnosis session with a
    /// deterministic, file-backed stand-in (no model call).
    scripted: bool,
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
        let attempt_dir = canonical_dir(&root)?;
        let protected = canonical_dir(&root.join("protected"))?;
        let sock = canonical_dir(&root.join("sock"))?;
        let tmp = canonical_dir(&root.join("verify-tmp"))?;
        let home = canonical_dir(&root.join("verify-home"))?;
        let artifacts = canonical_dir(&root.join("artifacts"))?;
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

        let mut env = self.deps.toolchain_env.clone();
        env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        env.insert("TMPDIR".to_string(), tmp.to_string_lossy().into_owned());
        let mut path = env
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| "/usr/bin:/bin".to_string());
        path.push_str(":/usr/bin:/bin:/usr/sbin:/sbin");
        env.insert("PATH".to_string(), path);

        let started = Instant::now();
        let mut sandbox = SeatbeltSandbox::new();
        let mut command_results: Vec<serde_json::Value> = Vec::new();
        let mut failures: Vec<serde_json::Value> = Vec::new();
        for (index, command) in repo.acceptance.iter().enumerate() {
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
                    protected_dir: protected.to_string_lossy().into_owned(),
                    sock_dir: sock.to_string_lossy().into_owned(),
                    extra_ro_roots: self
                        .deps
                        .extra_ro_roots
                        .iter()
                        .map(|root| root.to_string_lossy().into_owned())
                        .collect(),
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
            ctx.attempt,
            zero_usage(),
            wall_ms,
        )?;
        let outcome = ActivityOutcome {
            result: json!({
                "status": "green",
                "exit_code": 0,
                "attempt": ctx.attempt,
                "candidate_sha": candidate_sha,
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

    /// Render and write the per-attempt verifier prompt under `<run>/prompts/`.
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
        let diagnosis_path = artifacts.join(format!("diagnosis@{}.md", ctx.attempt));
        let prompt_path = self.write_verifier_prompt(root, ctx)?;
        let started = Instant::now();
        let usage = if self.scripted {
            let failure = std::fs::read_to_string(failure_path).unwrap_or_default();
            let diff =
                std::fs::read_to_string(artifacts.join(format!("diff@{}.patch", ctx.attempt)))
                    .unwrap_or_default();
            std::fs::write(
                &diagnosis_path,
                render_scripted_diagnosis(ctx.attempt, &failure, &diff),
            )
            .map_err(|e| {
                ActivityError::KnownFailure(format!(
                    "write diagnosis {}: {e}",
                    diagnosis_path.display()
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
            self.run_verifier_session(
                checkout,
                root,
                ctx,
                &prompt_path,
                failure_path,
                &diagnosis_path,
            )?
        };
        record_usage_event(
            &self.deps.state_dir,
            &ctx.execution_id,
            &ctx.operation_id,
            "verify",
            "verifier",
            ctx.attempt,
            usage,
            started.elapsed().as_millis() as u64,
        )?;
        Ok(diagnosis_path)
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
        let protected = canonical_dir(&root.join("protected"))?;
        let sock = canonical_dir(&root.join("sock"))?;
        let mut config =
            OpenCodeHarnessConfig::new(&self.deps.profile_path, root, &protected, &sock);
        config.model = Some(self.deps.model.clone());
        config.proxy_url = self.deps.proxy_url.clone();
        config.extra_ro_roots = self.deps.extra_ro_roots.clone();
        config.extra_env = self.deps.toolchain_env.clone();

        let mut harness = OpenCodeHarness::new(Box::new(SeatbeltSandbox::new()), config);
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
        harness: &mut OpenCodeHarness,
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
                model: self.deps.model.clone(),
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
        let state = poll_terminal(harness, &op, self.deps.turn_timeout);
        let _ = harness.close_session(&session);
        let usage = match state {
            Some(OperationState::Completed { usage, .. }) => usage,
            Some(OperationState::Failed { reason }) => {
                eprintln!("agalma: verifier session failed: {reason}");
                zero_usage()
            }
            _ => {
                let _ = harness.cancel_operation(&op);
                zero_usage()
            }
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
        let attempt_dir = canonical_dir(&root)?;
        let protected = canonical_dir(&root.join("protected"))?;
        let sock = canonical_dir(&root.join("sock"))?;
        let tmp = canonical_dir(&root.join("verify-tmp"))?;
        let home = canonical_dir(&root.join("verify-home"))?;
        let artifacts = canonical_dir(&root.join("artifacts"))?;
        let log_path = artifacts.join(format!("verify@{}.log", ctx.attempt));
        mark_started(&self.deps, &ctx.execution_id, "verify", ctx.attempt)?;

        let cargo = cargo_bin()?;
        let mut env = self.deps.toolchain_env.clone();
        env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        env.insert("TMPDIR".to_string(), tmp.to_string_lossy().into_owned());
        env.insert(
            "CARGO_HOME".to_string(),
            root.join("cargo-home").to_string_lossy().into_owned(),
        );
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            root.join("verify-target").to_string_lossy().into_owned(),
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
    workspace: Rc<RefCell<Workspace>>,
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
            let receipt = workspace
                .integrate_origin(&checkout, &repo.origin, &checkout.base_sha)
                .map_err(contract_err)?;
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
/// path. The base prompt (`prompts/builder.md`) is the M0 text; repo mode appends
/// the task title, acceptance commands, and any prior failure/diagnosis artifact.
fn write_builder_prompt(
    deps: &PhaseDeps,
    root: &Path,
    ctx: &OperationContext,
) -> Result<PathBuf, ActivityError> {
    let base = std::fs::read_to_string(&deps.builder_prompt).unwrap_or_default();
    let text = render_builder_prompt(&base, deps, root, ctx.attempt);
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

/// Render the per-attempt builder prompt: the base prompt plus (in repo mode)
/// the task title, acceptance commands, and the prior failure/diagnosis artifacts
/// (both inlined and referenced by path, so the builder can re-read them).
fn render_builder_prompt(base: &str, deps: &PhaseDeps, root: &Path, attempt: u32) -> String {
    let Some(repo) = &deps.repo else {
        return base.to_string();
    };
    let mut out = String::new();
    out.push_str(base);
    out.push_str("\n\n---\n\n## Task\n\n");
    out.push_str(&format!("Title: {}\n", repo.task_title));
    out.push_str("\nAcceptance commands (run by the conductor; every one must exit 0):\n\n");
    for (index, command) in repo.acceptance.iter().enumerate() {
        out.push_str(&format!("{}. `{command}`\n", index + 1));
    }
    if attempt > 1 {
        let failure = root
            .join("artifacts")
            .join(format!("failure@{}.txt", attempt - 1));
        if let Ok(text) = std::fs::read_to_string(&failure) {
            out.push_str(&format!(
                "\n## Previous attempt failure\n\nArtifact: `{}`\n\n```text\n",
                failure.display()
            ));
            out.push_str(text.trim_end());
            out.push_str("\n```\n");
        }
        let diagnosis = root
            .join("artifacts")
            .join(format!("diagnosis@{}.md", attempt - 1));
        if let Ok(text) = std::fs::read_to_string(&diagnosis) {
            out.push_str(&format!(
                "\n## Verifier diagnosis\n\nArtifact: `{}`\n\n",
                diagnosis.display()
            ));
            out.push_str(&text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// Render the per-attempt verifier prompt: the base verifier prompt plus the
/// file-only input paths and the absolute diagnosis output path.
fn render_verifier_prompt(base: &str, deps: &PhaseDeps, root: &Path, attempt: u32) -> String {
    let artifacts = root.join("artifacts");
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
        artifacts.join(format!("diagnosis@{attempt}.md")).display(),
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
    attempt: u32,
    usage: Usage,
    wall_ms: u64,
) -> Result<(), ActivityError> {
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).map_err(contract_err)?;
    let event = agalma_contracts::ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: "usage".to_string(),
        payload: json!({
            "operation_id": operation_id,
            "phase": phase,
            "role": role,
            "attempt": attempt,
            "tokens_in": usage.tokens_in,
            "tokens_out": usage.tokens_out,
            "cost_usd": usage.cost_usd,
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

/// `max_attempts` recorded in the `execution_created` event.
fn execution_max_attempts(
    state_dir: &Path,
    execution: &agalma_contracts::ExecutionId,
) -> Result<u32, ActivityError> {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).map_err(contract_err)?;
    for event in ledger.events(execution).map_err(contract_err)? {
        if event.kind == "execution_created" {
            return Ok(event
                .payload
                .get("max_attempts")
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as u32);
        }
    }
    Err(ActivityError::KnownFailure(
        "no execution_created event for retry decision".to_string(),
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

/// Poll `inspect_operation` until terminal or `timeout`; `None` on timeout.
fn poll_terminal(
    harness: &mut OpenCodeHarness,
    op: &OperationHandle,
    timeout: Duration,
) -> Option<OperationState> {
    let deadline = Instant::now() + timeout;
    loop {
        match harness.inspect_operation(op) {
            Ok(state) if state.is_terminal() => return Some(state),
            Ok(_) => {}
            Err(err) => {
                eprintln!("agalma: inspect_operation: {err}");
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
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

    fn deps(repo: Option<RepoMode>) -> PhaseDeps {
        PhaseDeps {
            state_dir: PathBuf::from("state"),
            fixture: PathBuf::from("fixture"),
            model: "test/offline".to_string(),
            role: "builder".to_string(),
            profile_path: PathBuf::from("worker.sb"),
            builder_prompt: PathBuf::from("builder.md"),
            proxy_url: None,
            extra_ro_roots: Vec::new(),
            toolchain_env: BTreeMap::new(),
            turn_timeout: Duration::from_secs(1),
            verify_timeout: Duration::from_secs(1),
            repo,
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-runs")
            .join(format!("phases-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(dir.join("artifacts")).expect("scratch dir");
        dir
    }

    #[test]
    fn render_prompt_without_repo_returns_base() {
        let base = "base prompt";
        let text = render_builder_prompt(base, &deps(None), Path::new("."), 1);
        assert_eq!(text, base);
    }

    #[test]
    fn render_prompt_repo_mode_includes_title_and_acceptance() {
        let deps = deps(Some(RepoMode {
            origin: PathBuf::from("origin"),
            base_ref: "main".to_string(),
            task_title: "Fix the thing".to_string(),
            acceptance: vec!["sh -c 'true'".to_string(), "cargo test".to_string()],
        }));
        let text = render_builder_prompt("base", &deps, Path::new("."), 1);
        assert!(text.contains("Fix the thing"), "{text}");
        assert!(text.contains("sh -c 'true'"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
    }

    #[test]
    fn render_prompt_retry_includes_captured_failure_and_diagnosis() {
        let dir = scratch_dir("prompt-retry");
        std::fs::write(
            dir.join("artifacts").join("failure@1.txt"),
            "command 0 failed: grep MAGIC",
        )
        .unwrap();
        std::fs::write(
            dir.join("artifacts").join("diagnosis@1.md"),
            "the function returns 0",
        )
        .unwrap();
        let deps = deps(Some(RepoMode {
            origin: PathBuf::from("origin"),
            base_ref: "main".to_string(),
            task_title: "Fix".to_string(),
            acceptance: vec!["sh -c 'true'".to_string()],
        }));
        let text = render_builder_prompt("base", &deps, &dir, 2);
        assert!(text.contains("grep MAGIC"), "{text}");
        assert!(text.contains("the function returns 0"), "{text}");
    }
}
