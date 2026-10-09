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

use agalma_contracts::{ContractError, ExecutionApi, ExecutionPhase};
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;
use agalma_sandbox::PRODUCT_PROFILE;
use agalma_workspace::Workspace;

use crate::config::Config;
use crate::phases::{activities, PhaseDeps, RunState};
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::task::{self, TaskDescriptor};

/// Builder prompt shipped with the conductor and materialized into the state
/// dir at boot.
pub const BUILDER_PROMPT: &str = include_str!("../prompts/builder.md");

/// Default bound on one model turn (free model can be slow; M0 allows minutes).
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// Default bound on the sandboxed verify command.
pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(900);

/// The wired service graph for one conductor process.
pub struct Composition {
    /// Serial executor with all five phase activities registered.
    pub executor: Executor<SqliteLedger>,
    /// Parent-owned provider proxy; held for the process lifetime.
    pub proxy: ProviderProxy,
    /// Parsed hardcoded-task descriptor.
    pub task: TaskDescriptor,
    pub state_dir: PathBuf,
    pub fixture: PathBuf,
    pub ledger_path: PathBuf,
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
        let ledger = SqliteLedger::open(&ledger_path)?;

        let task = task::load_task_descriptor(&fixture)
            .map_err(|e| ContractError::KnownFailure(e.to_string()))?;

        let mut executor = Executor::new(
            ledger,
            ExecutorConfig {
                max_attempts: task.max_attempts,
                execution_definition_version: EXECUTION_DEFINITION_VERSION,
            },
        );

        let workspace = Workspace::new(state_dir.clone());

        // Provider egress for the confined harness (S0b profile denies
        // arbitrary network). The proxy binds a loopback port; its URL is handed
        // to the harness via `OpenCodeHarnessConfig::proxy_url`.
        let proxy = ProviderProxy::start(ProxyConfig::from_env())
            .map_err(|e| ContractError::KnownFailure(format!("provider proxy: {e}")))?;

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
        std::fs::write(&builder_prompt, BUILDER_PROMPT).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot write builder prompt {}: {e}",
                builder_prompt.display()
            ))
        })?;

        let (extra_ro_roots, toolchain_env) = toolchain(config)?;

        let deps = PhaseDeps {
            state_dir: state_dir.clone(),
            fixture: fixture.clone(),
            model: config.model.clone(),
            role: task.role.clone(),
            profile_path,
            builder_prompt,
            proxy_url: Some(proxy.url()),
            extra_ro_roots,
            toolchain_env,
            turn_timeout: DEFAULT_TURN_TIMEOUT,
            verify_timeout: DEFAULT_VERIFY_TIMEOUT,
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
fn toolchain(_config: &Config) -> Result<(Vec<PathBuf>, BTreeMap<String, String>), ContractError> {
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
