//! M0.7 phase activities: the real Git/Seatbelt/OpenCode effects behind the
//! executor's [`agalma_execution::Activity`] seam.
//!
//! ```text
//! intake -> checkout -> build -> verify -> integrate -> done | parked
//! ```
//!
//! - `checkout`: [`WorkspaceApi::prepare`] copies the fixture template and
//!   records the base SHA.
//! - `build`: one confined OpenCode session in the checkout with the builder
//!   prompt; the model edits the checkout. A non-completed turn is a bounded
//!   retry.
//! - `verify`: a *separate* confined `cargo test` run; the exit code is the
//!   verdict, full output is captured under `<state>/runs/<exec>/artifacts/`.
//! - `integrate`: [`WorkspaceApi::integrate`] compare-and-updates `main` and
//!   tags `m0/<exec>`.
//!
//! The checkout is shared between the checkout and integrate activities through
//! an [`Rc<RefCell<_>>`] because the executor runs one logical operation at a
//! time. The harness and verify sandbox are constructed per attempt so each
//! launch is isolated and cleanup is proven by `stop_attempt`/process exit.
//!
//! Recovery is deliberately minimal at M0.7 (`effect_present` is `Absent`, so
//! no probe ever reports a surviving effect); the full recovery matrix is M0.8.

use std::cell::RefCell;
use std::collections::BTreeMap;
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

/// Mutable state shared between phase activities for one run.
#[derive(Default)]
pub struct RunState {
    /// The checkout prepared by the checkout phase.
    pub checkout: Option<Checkout>,
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

/// Build the five phase activities over shared state.
pub fn activities(
    deps: PhaseDeps,
    workspace: Rc<RefCell<Workspace>>,
    state: Rc<RefCell<RunState>>,
) -> PhaseActivities {
    PhaseActivities {
        intake: Box::new(NoopActivity),
        checkout: Box::new(CheckoutActivity {
            deps: deps.clone(),
            workspace: Rc::clone(&workspace),
            state: Rc::clone(&state),
        }),
        build: Box::new(BuildActivity {
            deps: deps.clone(),
            state: Rc::clone(&state),
        }),
        verify: Box::new(VerifyActivity {
            deps: deps.clone(),
            state: Rc::clone(&state),
        }),
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

impl Activity for CheckoutActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let template = self.deps.fixture.to_string_lossy().into_owned();
        let checkout = self
            .workspace
            .borrow_mut()
            .prepare(&template, &ctx.execution_id)
            .map_err(contract_err)?;
        self.state.borrow_mut().checkout = Some(checkout.clone());
        Ok(ActivityOutcome {
            result: json!({
                "base_sha": checkout.base_sha,
                "path": checkout.path,
                "candidate_branch": checkout.candidate_branch,
            }),
            next: ActivityNext::Advance,
        })
    }

    fn effect_present(&mut self, _ctx: &OperationContext) -> EffectProbe {
        EffectProbe::Absent
    }

    fn reconcile(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        Err(ActivityError::KnownFailure(
            "checkout reconcile is not supported at M0.7".to_string(),
        ))
    }
}

/// `build`: one confined OpenCode session that fixes the checkout.
struct BuildActivity {
    deps: PhaseDeps,
    state: Rc<RefCell<RunState>>,
}

impl BuildActivity {
    fn run_turn(&self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow()
            .checkout
            .clone()
            .ok_or_else(|| ActivityError::KnownFailure("build before checkout".to_string()))?;
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

        let outcome = self.drive(&mut harness, &attempt, ctx);
        let evidence = harness.stop_attempt(&attempt);
        if let Ok(evidence) = evidence {
            if !evidence.process_group_gone {
                eprintln!("agalma: build attempt cleanup: {}", evidence.detail);
            }
        }
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

    fn effect_present(&mut self, _ctx: &OperationContext) -> EffectProbe {
        EffectProbe::Absent
    }

    fn reconcile(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        Err(ActivityError::KnownFailure(
            "build reconcile is not supported at M0.7".to_string(),
        ))
    }
}

/// `verify`: a separate confined `cargo test` run; exit code is the verdict.
struct VerifyActivity {
    deps: PhaseDeps,
    state: Rc<RefCell<RunState>>,
}

impl Activity for VerifyActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout = self
            .state
            .borrow()
            .checkout
            .clone()
            .ok_or_else(|| ActivityError::KnownFailure("verify before checkout".to_string()))?;
        let root = run_root(&self.deps.state_dir, &ctx.execution_id);
        let attempt_dir = canonical_dir(&root)?;
        let protected = canonical_dir(&root.join("protected"))?;
        let sock = canonical_dir(&root.join("sock"))?;
        let tmp = canonical_dir(&root.join("verify-tmp"))?;
        let home = canonical_dir(&root.join("verify-home"))?;
        let artifacts = canonical_dir(&root.join("artifacts"))?;
        let log_path = artifacts.join(format!("verify@{}.log", ctx.attempt));

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

        if output.exit_code == 0 {
            Ok(ActivityOutcome {
                result: json!({
                    "status": "green",
                    "exit_code": output.exit_code,
                    "attempt": ctx.attempt,
                    "artifact": log_path.to_string_lossy(),
                }),
                next: ActivityNext::Advance,
            })
        } else {
            eprintln!(
                "agalma: verify attempt {} red (exit {}); log {}",
                ctx.attempt,
                output.exit_code,
                log_path.display()
            );
            Ok(ActivityOutcome {
                result: json!({
                    "status": "red",
                    "exit_code": output.exit_code,
                    "attempt": ctx.attempt,
                    "artifact": log_path.to_string_lossy(),
                }),
                next: ActivityNext::Retry {
                    failure: ArtifactRef::derive(&format!("verify@{}", ctx.attempt)),
                },
            })
        }
    }

    fn effect_present(&mut self, _ctx: &OperationContext) -> EffectProbe {
        EffectProbe::Absent
    }

    fn reconcile(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        Err(ActivityError::KnownFailure(
            "verify reconcile is not supported at M0.7".to_string(),
        ))
    }
}

/// `integrate`: compare-and-update `main` to the candidate and tag `m0/<exec>`.
struct IntegrateActivity {
    deps: PhaseDeps,
    workspace: Rc<RefCell<Workspace>>,
    state: Rc<RefCell<RunState>>,
}

impl Activity for IntegrateActivity {
    fn execute(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout =
            self.state.borrow().checkout.clone().ok_or_else(|| {
                ActivityError::KnownFailure("integrate before checkout".to_string())
            })?;
        let expected_main_sha = checkout.base_sha.clone();
        let receipt = self
            .workspace
            .borrow_mut()
            .integrate(&checkout, &expected_main_sha)
            .map_err(contract_err)?;
        let tag = format!(
            "m0/{}",
            checkout.candidate_branch.trim_start_matches("candidate/")
        );
        let _ = &self.deps;
        Ok(ActivityOutcome {
            result: json!({
                "expected_main_sha": receipt.expected_main_sha,
                "candidate_sha": receipt.candidate_sha,
                "result_sha": receipt.result_sha,
                "tag": tag,
            }),
            next: ActivityNext::Advance,
        })
    }

    fn effect_present(&mut self, _ctx: &OperationContext) -> EffectProbe {
        EffectProbe::Absent
    }

    fn reconcile(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        Err(ActivityError::KnownFailure(
            "integrate reconcile is not supported at M0.7".to_string(),
        ))
    }
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
