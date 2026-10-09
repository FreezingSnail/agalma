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
//! - `verify`: a *separate* confined `cargo test` run; the exit code is the
//!   verdict, full output is captured under `<state>/runs/<exec>/artifacts/`.
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
use std::rc::Rc;
use std::time::{Duration, Instant};

use agalma_contracts::harness::{
    AttemptHandle, CreateSessionRequest, HarnessApi, Limits, OperationHandle, OperationState,
    RunTurnRequest, StartAttemptRequest,
};
use agalma_contracts::ids::{ArtifactRef, AttemptId};
use agalma_contracts::sandbox::LaunchSpec;
use agalma_contracts::{Checkout, ContractError, WorkspaceApi};
use agalma_execution::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, OperationContext,
};
use agalma_harness_opencode::{OpenCodeHarness, OpenCodeHarnessConfig};
use agalma_sandbox::SeatbeltSandbox;
use agalma_workspace::Workspace;
use serde_json::json;

use crate::workers::{self, Survivor};

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
    let verify: Box<dyn Activity> = match mode {
        ActivityMode::Live => Box::new(VerifyActivity {
            deps: deps.clone(),
            state: Rc::clone(&state),
        }),
        ActivityMode::Scripted => Box::new(ScriptedVerifyActivity {
            deps: deps.clone(),
            state: Rc::clone(&state),
        }),
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
        let template = self.deps.fixture.to_string_lossy().into_owned();
        let checkout = self
            .workspace
            .borrow_mut()
            .prepare(&template, &ctx.execution_id)
            .map_err(contract_err)?;
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
                pid,
                pid,
            )?;
        }
        mark_started(&self.deps, &ctx.execution_id, "build", ctx.attempt)?;

        let outcome = self.drive(&mut harness, &attempt, ctx);
        let evidence = harness.stop_attempt(&attempt);
        if let Ok(evidence) = &evidence {
            if !evidence.process_group_gone {
                eprintln!("agalma: build attempt cleanup: {}", evidence.detail);
            }
        }
        if let Ok(outcome) = &outcome {
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
    ) -> Result<ActivityOutcome, ActivityError> {
        let session = harness
            .create_session(CreateSessionRequest {
                role: self.deps.role.clone(),
                model: self.deps.model.clone(),
                prompts_ref: self.deps.builder_prompt.to_string_lossy().into_owned(),
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

        let (pid, mut child) = spawn_sleeper()?;
        workers::record_worker(
            &self.deps.state_dir,
            &ctx.execution_id,
            "build",
            ctx.attempt,
            ctx.operation_id.as_str(),
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

/// `verify`: a separate confined `cargo test` run; exit code is the verdict.
struct VerifyActivity {
    deps: PhaseDeps,
    state: Rc<RefCell<RunState>>,
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

impl Activity for VerifyActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
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

fn integrate_outcome(
    receipt: &agalma_contracts::MergeReceipt,
    checkout: &Checkout,
) -> ActivityOutcome {
    ActivityOutcome {
        result: json!({
            "expected_main_sha": receipt.expected_main_sha,
            "candidate_sha": receipt.candidate_sha,
            "result_sha": receipt.result_sha,
            "tag": integrate_tag(checkout),
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
            return Ok(integrate_outcome(&receipt, &checkout));
        }
        let receipt = workspace
            .integrate(&checkout, &checkout.base_sha)
            .map_err(contract_err)?;
        Ok(integrate_outcome(&receipt, &checkout))
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        let Ok(checkout) = self
            .state
            .borrow_mut()
            .ensure_checkout(&self.deps.state_dir, &ctx.execution_id)
        else {
            return EffectProbe::Ambiguous;
        };
        match self.workspace.borrow_mut().integrated_receipt(&checkout) {
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
        Ok(integrate_outcome(&receipt, &checkout))
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
