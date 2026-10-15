//! Composition root: wires implementations behind Agalma contracts.
//!
//! Dependency direction is `conductor -> implementations -> contracts`. Each
//! implementation crate is constructed here and injected into the executor (the
//! SQLite ledger), the workspace, the sandbox-backed harness, and the loopback
//! provider proxy. No implementation is imported by another implementation; the
//! architecture lint in `tests/architecture.rs` enforces that.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use agalma_contracts::{ContractError, ExecutionApi, ExecutionPhase, LedgerApi};
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_sandbox::PRODUCT_PROFILE;

use crate::config::Config;
use crate::harness_binding::{harness_handle, HarnessChoice};
use crate::ledger_process::{LedgerHandle, LedgerTransport};
use crate::phases::{activities, PhaseDeps, RunState};
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::stop::{BoundStopControl, StopControl};
use crate::task::{self, TaskDescriptor};
use crate::workspace_process::{WorkspaceHandle, WorkspaceTransport};

/// Repository-mode builder prompt shipped with the conductor. It is the base
/// text for every repository (`agalma work`) build; the per-attempt render adds
/// the frozen task context (`agalma-5fy.1`). Pinned as the builder role prompt
/// bytes at admission (`work::role_inputs`).
pub const BUILDER_PROMPT: &str = include_str!("../prompts/builder.md");

/// Fixture-mode builder prompt (M0): the deliberate one-line broken-function
/// exercise driven by `agalma run --fixture`. Kept on its own prompt path so a
/// repository feature build never inherits the fixture's one-line instruction
/// (`agalma-5fy.1`).
pub const BUILDER_FIXTURE_PROMPT: &str = include_str!("../prompts/builder-fixture.md");

/// Default bound on one model turn (free model can be slow; M0 allows minutes).
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// Default bound on the sandboxed verify command.
pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(900);
/// Default idle bound for the progress watchdog (H1): a session with no
/// token/state delta for this long is cancelled and classified `stalled`.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Environment variable overriding [`DEFAULT_IDLE_TIMEOUT`] (whole seconds).
pub const IDLE_TIMEOUT_ENV: &str = "AGALMA_IDLE_TIMEOUT_SECS";
/// Environment variable overriding the designer session turn bound (whole
/// seconds).
///
/// The designer default stays [`DEFAULT_TURN_TIMEOUT`] (600 s): the pilot
/// `agalma-i5h.7` authored 10 children plus edges and crossed 600 s on a slow
/// paid model, so operators raise this per host for large decompositions
/// without touching the worker/concierge/reviewer bounds.
pub const DESIGNER_TIMEOUT_ENV: &str = "AGALMA_DESIGNER_TIMEOUT_SECS";

/// Resolve the idle watchdog bound from [`IDLE_TIMEOUT_ENV`] or the default.
/// A non-numeric or zero value falls back to the default.
pub fn resolve_idle_timeout() -> Duration {
    timeout_from_secs(
        std::env::var(IDLE_TIMEOUT_ENV).ok().as_deref(),
        DEFAULT_IDLE_TIMEOUT,
    )
}

/// Resolve the designer session turn bound from [`DESIGNER_TIMEOUT_ENV`] or the
/// default. A non-numeric or zero value falls back to [`DEFAULT_TURN_TIMEOUT`].
pub fn resolve_designer_timeout() -> Duration {
    timeout_from_secs(
        std::env::var(DESIGNER_TIMEOUT_ENV).ok().as_deref(),
        DEFAULT_TURN_TIMEOUT,
    )
}

/// Pure whole-seconds parser shared by the idle and designer resolvers: a
/// trimmed positive integer wins, anything else (absent, zero, negative,
/// garbage) falls back to `default`. Kept free of process-env access so it is
/// unit-testable without mutating the environment.
fn timeout_from_secs(raw: Option<&str>, default: Duration) -> Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// The wired service graph for one conductor process.
pub struct Composition {
    /// Serial executor with all five phase activities registered.
    pub executor: Executor<LedgerHandle>,
    /// Parent-owned provider proxy; held for the process lifetime.
    pub proxy: ProviderProxy,
    /// Parsed hardcoded-task descriptor.
    pub task: TaskDescriptor,
    pub state_dir: PathBuf,
    pub fixture: PathBuf,
    pub ledger_path: PathBuf,
    /// Shared bound stop control used by the fixture/run drive loop and phase
    /// polls (`agalma-5fy.5`).
    pub stop: Rc<dyn StopControl>,
}

/// Bounded survivor cleanup used when the stop decision is `Stopped`
/// (`agalma-5fy.5`): terminate live workers recorded for the authoritative bound
/// pending intents, without constructing the normal composition (no
/// proxy/workspace/prompts/session/task admission). Reuses the existing
/// [`crate::workers::reconcile_survivor`] path.
pub(crate) fn reconcile_bound_survivors(state_dir: &Path, ledger: &LedgerHandle) {
    let pending = match ledger.pending_intents() {
        Ok(pending) => pending,
        Err(err) => {
            eprintln!("agalma: survivor cleanup: pending intents unreadable: {err}");
            return;
        }
    };
    let mut sandbox = agalma_sandbox::SeatbeltSandbox::new();
    for intent in pending {
        if intent.consumed {
            continue;
        }
        let Some(kind) = intent.payload.get("kind").and_then(|v| v.as_str()) else {
            continue;
        };
        let attempt = intent
            .payload
            .get("attempt")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        if let Err(err) = crate::workers::reconcile_survivor(
            state_dir,
            &intent.execution_id,
            kind,
            attempt,
            &mut sandbox,
        ) {
            eprintln!(
                "agalma: survivor cleanup for {} failed: {err}",
                intent.execution_id
            );
        }
    }
}

impl Composition {
    /// Build the full service graph from resolved configuration.
    pub fn build(config: &Config) -> Result<Composition, ContractError> {
        std::fs::create_dir_all(&config.state_dir).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot create state dir {}: {e}",
                config.state_dir.display()
            ))
        })?;
        // Absolute, symlink-resolved state dir: the harness rejects relative
        // `location.directory` values, and Seatbelt subpath matching needs
        // canonical roots.
        let state_dir = std::fs::canonicalize(&config.state_dir).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot resolve state dir {}: {e}",
                config.state_dir.display()
            ))
        })?;
        let fixture =
            std::fs::canonicalize(&config.fixture).unwrap_or_else(|_| config.fixture.clone());

        let ledger_path = state_dir.join("ledger.sqlite");
        // Entry-aware STOP guard before any ledger open (`agalma-5fy.5`): a
        // present stop must never initialize a database or start the proxy.
        if let Some(reason) = crate::stop::observe_sentinel(&state_dir).reason() {
            return Err(ContractError::KnownFailure(format!(
                "stop admission refused: {reason}"
            )));
        }
        // Explicit fresh initialization vs established open through the selected
        // binding (`agalma-5fy.5`): a lost/partial established ledger is refused
        // here, never recreated. The same bound control is shared with the phase
        // polls and the fixture/run drive loop.
        let mut ledger =
            crate::ledger_process::open_admission_ledger(&state_dir, LedgerTransport::from_env())
                .map_err(ContractError::KnownFailure)?;
        let stop: Rc<dyn StopControl> = Rc::new(BoundStopControl::new(&state_dir, ledger.clone()));
        // Typed bound decision BEFORE normal composition (proxy/workspace/prompts/
        // activities/task admission) (`agalma-5fy.5`):
        // - Running: normal composition;
        // - Unavailable: immediate refusal (no attempt to infer/read lost state);
        // - Stopped: bounded existing survivor cleanup over the authoritative
        //   bound pending intents, then refusal — no proxy/workspace/session/task
        //   admission.
        match stop.decision() {
            crate::stop::StopDecision::Running => {}
            crate::stop::StopDecision::Unavailable(reason) => {
                return Err(ContractError::KnownFailure(format!(
                    "stop state unavailable; refusing composition: {reason}"
                )));
            }
            crate::stop::StopDecision::Stopped(reason) => {
                reconcile_bound_survivors(&state_dir, &ledger);
                return Err(ContractError::KnownFailure(format!(
                    "stop policy active; refusing normal composition: {reason}"
                )));
            }
        }

        let task = task::load_task_descriptor(&fixture)
            .map_err(|e| ContractError::KnownFailure(e.to_string()))?;

        let mut executor = Executor::new(
            ledger.clone(),
            ExecutorConfig {
                max_attempts: task.max_attempts,
                execution_definition_version: EXECUTION_DEFINITION_VERSION,
            },
        );

        let workspace =
            WorkspaceHandle::from_transport(&state_dir, WorkspaceTransport::from_env())?;

        // Provider egress for the confined harness (S0b profile denies
        // arbitrary network). The proxy binds a loopback port; its URL is handed
        // to the harness via `OpenCodeHarnessConfig::proxy_url`.
        let proxy = ProviderProxy::start(ProxyConfig::from_env())
            .map_err(|e| ContractError::KnownFailure(format!("provider proxy: {e}")))?;
        // Record the token-free provider credential audit (best effort) through
        // the bound handle, so under `process` transport it rides the conductor's
        // single ledger connection instead of opening a secondary WAL connection.
        if let Err(err) = crate::provider_auth::record_audit_via(&mut ledger, &proxy.auth_state()) {
            eprintln!("agalma: provider_auth audit: {err}");
        }

        let profile_path = state_dir.join("worker.sb");
        std::fs::write(&profile_path, PRODUCT_PROFILE).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot write sandbox profile {}: {e}",
                profile_path.display()
            ))
        })?;

        let prompts_dir = state_dir.join("prompts");
        std::fs::create_dir_all(&prompts_dir)
            .map_err(|e| ContractError::KnownFailure(format!("cannot create prompts dir: {e}")))?;
        let builder_prompt = prompts_dir.join("builder.md");
        // Fixture runs (`agalma run --fixture`) use the explicit fixture prompt;
        // repository runs materialize the frozen repository prompt through
        // `work::phase_deps` instead (`agalma-5fy.1`).
        std::fs::write(&builder_prompt, BUILDER_FIXTURE_PROMPT).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot write builder prompt {}: {e}",
                builder_prompt.display()
            ))
        })?;

        let (extra_ro_roots, toolchain_env) = toolchain()?;

        let deps = PhaseDeps {
            state_dir: state_dir.clone(),
            fixture: fixture.clone(),
            model: config.model.clone(),
            verifier_model: config.model.clone(),
            route: "builder:direct".to_string(),
            verifier_route: "verifier:direct".to_string(),
            role: task.role.clone(),
            profile_path,
            builder_prompt,
            proxy_url: Some(proxy.url()),
            paid_proxy: false,
            extra_ro_roots,
            toolchain_env,
            turn_timeout: DEFAULT_TURN_TIMEOUT,
            verify_timeout: DEFAULT_VERIFY_TIMEOUT,
            idle_timeout: resolve_idle_timeout(),
            repo: None,
            harness: harness_handle(HarnessChoice::OpenCode),
            stop: Rc::clone(&stop),
        };

        let shared_state = Rc::new(RefCell::new(RunState::default()));
        let shared_workspace = Rc::new(RefCell::new(workspace));
        let acts = activities(deps, shared_workspace, shared_state);
        executor.add_activity(ExecutionPhase::Intake, acts.intake);
        executor.add_activity(ExecutionPhase::Checkout, acts.checkout);
        executor.add_activity(ExecutionPhase::Build, acts.build);
        executor.add_activity(ExecutionPhase::Verify, acts.verify);
        executor.add_activity(ExecutionPhase::Integrate, acts.integrate);
        // Boot gates (schema already checked by the ledger open): replay events,
        // projection agreement, and survivor reconciliation. Must run after the
        // activities are registered so probes can terminate survivors.
        executor.recover()?;

        Ok(Composition {
            executor,
            proxy,
            task,
            state_dir,
            fixture,
            ledger_path,
            stop,
        })
    }

    /// The ledger path (signal handler opens a second connection for the latch).
    pub fn ledger_path(&self) -> &Path {
        &self.ledger_path
    }
}

/// Resolve the read-only toolchain roots and environment handed to the confined
/// worker/verify.
///
/// Empirically (`which cargo rustc`, `rustc --print sysroot`) this host runs
/// rustup shims at `$CARGO_HOME/bin/cargo` (default `~/.cargo/bin`) selecting a
/// toolchain under `$RUSTUP_HOME` (default `~/.rustup/toolchains/<host>`). Both
/// homes must be readable inside the sandwich; `RUSTUP_HOME` and
/// `RUSTUP_TOOLCHAIN` must be set because the isolated `HOME` is not the user's.
/// Writes stay denied: only these read-only roots are added.
pub(crate) fn toolchain() -> Result<(Vec<PathBuf>, BTreeMap<String, String>), ContractError> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        ContractError::KnownFailure("HOME is not set; cannot locate toolchain".into())
    })?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".cargo"));
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".rustup"));

    let mut roots = Vec::new();
    for candidate in [&cargo_home, &rustup_home] {
        if candidate.is_dir() {
            let canonical = std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.clone());
            roots.push(canonical);
        }
    }

    let cargo_bin = cargo_home.join("bin");
    let cargo_bin = std::fs::canonicalize(&cargo_bin).unwrap_or(cargo_bin);
    let path = format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin",
        cargo_bin.display()
    );

    let mut env = BTreeMap::new();
    env.insert(
        "RUSTUP_HOME".to_string(),
        rustup_home.to_string_lossy().into_owned(),
    );
    env.insert(
        "RUSTUP_TOOLCHAIN".to_string(),
        std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "stable".to_string()),
    );
    env.insert("PATH".to_string(), path);
    Ok((roots, env))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_from_secs_parses_positive_and_falls_back_otherwise() {
        let default = Duration::from_secs(600);
        // A positive whole-second value wins (and is trimmed).
        assert_eq!(
            timeout_from_secs(Some("1800"), default),
            Duration::from_secs(1800)
        );
        assert_eq!(
            timeout_from_secs(Some(" 42 "), default),
            Duration::from_secs(42)
        );
        // Absent, zero, negative, and garbage all fall back to the default.
        assert_eq!(timeout_from_secs(None, default), default);
        assert_eq!(timeout_from_secs(Some("0"), default), default);
        assert_eq!(timeout_from_secs(Some("-5"), default), default);
        assert_eq!(timeout_from_secs(Some("soon"), default), default);
        assert_eq!(timeout_from_secs(Some(""), default), default);
    }

    #[test]
    fn timeout_from_secs_defaults_to_the_turn_bound() {
        assert_eq!(
            timeout_from_secs(None, DEFAULT_TURN_TIMEOUT),
            DEFAULT_TURN_TIMEOUT
        );
        assert_eq!(
            timeout_from_secs(Some("garbage"), DEFAULT_TURN_TIMEOUT),
            DEFAULT_TURN_TIMEOUT
        );
    }
}
