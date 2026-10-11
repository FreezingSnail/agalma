//! Shared confined OpenCode session seam (CP.3/CP.4, `agalma-i5h.4/.5`).
//!
//! The epic sweep (CP.3) runs one designer session per epic; the concierge
//! intake (CP.4) runs one concierge session per goal. Both go through the same
//! machinery: a per-attempt sandbox profile, the OpenCode adapter, a harness
//! socket, one bounded turn, and a terminal poll. Factoring it here keeps that
//! path single-sourced; [`crate::sweep::LiveDesignerSession`] and
//! [`crate::concierge::LiveConciergeSession`] differ only in the
//! [`SessionSpec`] they hand in.
//!
//! [`run_session`] returns the optional session output captured from an
//! attempt-root handoff file ([`SessionSpec::output_path`]): the designer
//! ignores it and relies on the `bd` side effects; the concierge reads its
//! output contract from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::harness::{
    CreateSessionRequest, HarnessApi, Limits, OperationState, RunTurnRequest, StartAttemptRequest,
};
use agalma_contracts::ids::ArtifactRef;
use agalma_contracts::{ContractError, OperationId};
use agalma_harness_opencode::{build_config, OpenCodeConfigInit};

use crate::harness_process::{self, HarnessTransport, OpenCodeHarnessInit};
use crate::phases::{ensure_cargo_home, poll_terminal, PollOutcome};
use crate::routing;
use crate::sandbox_process::SandboxSource;
use crate::stop::{FileStopControl, StopControl};

/// Per-conductor live session configuration shared by every role.
///
/// The state dir owns the sandbox profile, the harness scratch dirs, and the
/// harness socket directory; `toolchain_env`/`extra_ro_roots` carry the
/// read-only toolchain the confined session may use.
pub struct LiveSession {
    state_dir: PathBuf,
    profile_path: PathBuf,
    proxy_url: Option<String>,
    paid_proxy: bool,
    extra_ro_roots: Vec<PathBuf>,
    toolchain_env: BTreeMap<String, String>,
    turn_timeout: Duration,
    idle_timeout: Duration,
    /// Stop control polled while the session runs (`agalma-r2l`). A `STOP`
    /// sentinel or the durable kill latch cancels the turn and refuses to start
    /// a new one; the control is never cleared here.
    stop: Box<dyn StopControl>,
}

impl LiveSession {
    /// Assemble the live session config (materializing the sandbox profile).
    pub fn build(
        state_dir: &Path,
        proxy_url: Option<String>,
        paid_available: bool,
    ) -> Result<Self, ContractError> {
        let profile_path = state_dir.join("worker.sb");
        std::fs::write(&profile_path, agalma_sandbox::PRODUCT_PROFILE).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot write sandbox profile {}: {e}",
                profile_path.display()
            ))
        })?;
        let (extra_ro_roots, toolchain_env) = crate::composition::toolchain()?;
        Ok(LiveSession {
            state_dir: state_dir.to_path_buf(),
            profile_path,
            proxy_url,
            paid_proxy: paid_available && routing::allow_paid_from_env(),
            extra_ro_roots,
            toolchain_env,
            turn_timeout: crate::composition::DEFAULT_TURN_TIMEOUT,
            idle_timeout: crate::composition::resolve_idle_timeout(),
            stop: Box::new(FileStopControl::new(state_dir)),
        })
    }

    /// Install a custom stop control (tests embed a scripted one).
    pub fn with_stop(mut self, stop: Box<dyn StopControl>) -> Self {
        self.stop = stop;
        self
    }
}

/// One confined session to run through the shared harness seam.
pub struct SessionSpec {
    /// Role string handed to the harness (`designer`, `concierge`).
    pub role: String,
    /// Planner-route model pinned for the session.
    pub model: String,
    /// Target repository (read/write via the extra read/write root; never the
    /// sandbox ATTEMPT root, which holds runtime state).
    pub repo: PathBuf,
    /// Scratch subdirectory under the state dir (e.g. `sweep/<epic>`).
    pub dir_key: String,
    /// Harness socket file name under the state dir.
    pub socket_name: String,
    /// Operation id recorded on the attempt.
    pub operation_id: OperationId,
    /// The prompt file the adapter reads on the parent filesystem.
    pub prompt_path: PathBuf,
    /// Canonical input artifact for the turn.
    pub input_ref: ArtifactRef,
    /// Tool-policy artifact for the session.
    pub tool_policy_ref: ArtifactRef,
    /// Genome artifact for the attempt.
    pub genome_ref: ArtifactRef,
    /// Platform-constraints artifact for the attempt.
    pub constraints_ref: ArtifactRef,
    /// Optional attempt-root file the session writes its output contract to;
    /// when set, its content is returned on completion.
    pub output_path: Option<PathBuf>,
    /// Label used in the "ended without completion" error (role + scope).
    pub error_label: String,
}

/// Run one confined OpenCode session over `spec`, returning the optional output
/// captured from [`SessionSpec::output_path`] on a completed turn.
pub fn run_session(
    live: &LiveSession,
    spec: &SessionSpec,
) -> Result<Option<String>, ContractError> {
    // Dispatch gate (`agalma-r2l`): never start a session under an active stop.
    // This is the innermost admission check behind the sweep/intake gates; it
    // closes the window where a stop arrives between candidacy and dispatch.
    if live.stop.stopped() {
        return Err(ContractError::KnownFailure(format!(
            "{} not started: stop requested (STOP sentinel or kill latch set)",
            spec.error_label
        )));
    }

    let root = live.state_dir.join(&spec.dir_key);
    let protected = canonical_dir(&root.join("protected"))?;
    let sock = canonical_dir(&root.join("sock"))?;

    // The session authors tickets via `bd` in the target repo, so the repo must
    // be writable — but as the extra read/write root, not the sandbox ATTEMPT
    // root. ATTEMPT holds OpenCode's runtime state (XDG dirs, tmp, HOME, its git
    // snapshot store); inside the repo that state is snapshotted back into
    // itself and grows without bound (agalma-22f). It lives under the state dir.
    let attempt_dir = canonical_dir(&attempt_root(&live.state_dir, &spec.dir_key))?;
    let repo = canonical_dir(&spec.repo)?;

    let mut env = live.toolchain_env.clone();
    let cargo_home = ensure_cargo_home(&root)
        .map_err(|e| ContractError::KnownFailure(format!("cargo home: {e}")))?;
    env.insert(
        "CARGO_HOME".to_string(),
        cargo_home.to_string_lossy().into_owned(),
    );
    // `bd` must be reachable inside the sandbox: prepend its directory to PATH
    // and expose that directory read-only.
    let mut extra_ro_roots = live.extra_ro_roots.clone();
    if let Some(dir) = bd_bin_dir() {
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let base = env.get("PATH").cloned().unwrap_or_default();
        env.insert(
            "PATH".to_string(),
            format!("{}:{}", dir.to_string_lossy(), base),
        );
        extra_ro_roots.push(dir);
    }

    let config = build_config(OpenCodeConfigInit {
        profile_path: live.profile_path.clone(),
        attempt_dir,
        protected_dir: protected,
        sock_dir: sock,
        model: spec.model.clone(),
        proxy_url: live.proxy_url.clone(),
        paid_proxy: live.paid_proxy,
        extra_ro_roots,
        extra_rw_roots: vec![repo],
        extra_env: env,
    });

    let sandbox = SandboxSource::new(live.state_dir.clone());
    let sandbox_socket = sandbox.ensure()?;
    let harness_socket = live.state_dir.join(&spec.socket_name);
    let mut harness = harness_process::open_opencode_harness(OpenCodeHarnessInit {
        transport: HarnessTransport::from_env(),
        config,
        root: root.clone(),
        model: spec.model.clone(),
        generation: 1,
        sandbox_socket,
        harness_socket,
    })
    .map_err(ContractError::KnownFailure)?;

    let attempt = harness.start_attempt(StartAttemptRequest {
        workspace: spec.repo.to_string_lossy().into_owned(),
        role: spec.role.clone(),
        genome_ref: spec.genome_ref.to_string(),
        constraints_ref: spec.constraints_ref.to_string(),
        limits: Limits {
            wall_ms: live.turn_timeout.as_millis() as u64,
            tokens: 0,
            cost_micros: 0,
        },
        operation_id: spec.operation_id.clone(),
    })?;

    let session = harness.create_session(CreateSessionRequest {
        role: spec.role.clone(),
        model: spec.model.clone(),
        prompts_ref: spec.prompt_path.to_string_lossy().into_owned(),
        handoff_refs: Vec::new(),
        tool_policy_ref: spec.tool_policy_ref.to_string(),
    })?;
    let op = harness.run_turn(
        &session,
        RunTurnRequest {
            input_ref: spec.input_ref.clone(),
            bounded_turns: 1,
            deadline_ms: live.turn_timeout.as_millis() as u64,
        },
    )?;
    let outcome = poll_terminal(
        &mut harness,
        &op,
        live.idle_timeout,
        live.turn_timeout,
        Some(&*live.stop),
    );
    // Cancel before close: the interrupt must reach a live session. A stop
    // requested mid-turn is a non-terminal outcome, so it cancels here too.
    if matches!(
        outcome,
        PollOutcome::Stalled(_) | PollOutcome::TimedOut | PollOutcome::Stopped
    ) {
        let _ = harness.cancel_operation(&op);
    }
    let _ = harness.close_session(&session);
    let _ = harness.stop_attempt(&attempt);

    match outcome {
        PollOutcome::Terminal(OperationState::Completed { .. }) => {
            Ok(capture_output(spec.output_path.as_deref()))
        }
        PollOutcome::Stopped => Err(ContractError::KnownFailure(format!(
            "{} cancelled: stop requested (STOP sentinel or kill latch set)",
            spec.error_label
        ))),
        other => Err(ContractError::KnownFailure(format!(
            "{} ended without completion: {other:?}",
            spec.error_label
        ))),
    }
}

/// Read the optional attempt-root output file (best-effort: an absent file is
/// an empty contract, never a transport error).
fn capture_output(path: Option<&Path>) -> Option<String> {
    path.map(|path| std::fs::read_to_string(path).unwrap_or_default())
}

/// The sandbox ATTEMPT root for the session keyed `dir_key`: conductor-owned,
/// under the state dir, never inside the target repo. Session handoff files
/// the confined session reads or writes (e.g. the concierge goal/output) belong
/// here too. Matches the process-transport server's `<root>/attempt`.
pub fn attempt_root(state_dir: &Path, dir_key: &str) -> PathBuf {
    state_dir.join(dir_key).join("attempt")
}

/// Directory containing the `bd` binary (resolved from the conductor's PATH).
fn bd_bin_dir() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).find(|dir| dir.join("bd").is_file()))
}

/// Filesystem-safe name for a per-session directory or socket.
pub(crate) fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Create `path` (recursively) and return its canonical form.
fn canonical_dir(path: &Path) -> Result<PathBuf, ContractError> {
    std::fs::create_dir_all(path)
        .map_err(|e| ContractError::KnownFailure(format!("mkdir {}: {e}", path.display())))?;
    std::fs::canonicalize(path)
        .map_err(|e| ContractError::KnownFailure(format!("resolve {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_testkit::ScratchDir;

    fn scratch_dir(name: &str) -> ScratchDir {
        ScratchDir::new(&format!("session-{name}"))
    }

    fn spec() -> SessionSpec {
        SessionSpec {
            role: "designer".to_string(),
            model: "test/offline".to_string(),
            repo: PathBuf::from("."),
            dir_key: "sweep/stopped".to_string(),
            socket_name: "harness-stopped.sock".to_string(),
            operation_id: OperationId::new("op:stopped"),
            prompt_path: PathBuf::from("prompt.md"),
            input_ref: ArtifactRef::derive("designer-turn"),
            tool_policy_ref: ArtifactRef::derive("tool-policy-designer"),
            genome_ref: ArtifactRef::derive("genome"),
            constraints_ref: ArtifactRef::derive("platform-constraints"),
            output_path: None,
            error_label: "designer session".to_string(),
        }
    }

    #[test]
    fn sanitize_is_filesystem_safe() {
        assert_eq!(sanitize("agalma-i5h.4"), "agalma-i5h-4");
        assert_eq!(sanitize("concierge/goal#1"), "concierge-goal-1");
    }

    #[test]
    fn attempt_root_is_state_owned_never_the_repo() {
        // agalma-22f: the ATTEMPT root carries OpenCode runtime state; it must
        // sit under the state dir so it never lands in the target repo.
        let state = Path::new("/state");
        let root = attempt_root(state, "concierge/abc");
        assert_eq!(root, Path::new("/state/concierge/abc/attempt"));
        assert!(root.starts_with(state));
        assert!(!root.starts_with("/repo"));
    }

    #[test]
    fn stopped_session_refuses_to_dispatch() {
        // agalma-r2l: a preexisting STOP sentinel must prevent the session from
        // even being built (no harness, no paid call).
        let dir = scratch_dir("stopped");
        std::fs::write(dir.join("STOP"), b"stop").expect("write STOP");
        let live = LiveSession::build(&dir, None, false).expect("build live session");
        let err = run_session(&live, &spec()).expect_err("stopped must not dispatch");
        assert!(err.to_string().contains("stop"), "{err}");
        // Nothing session-scoped was materialized.
        assert!(
            !dir.join("sweep").exists(),
            "no session scratch created while stopped"
        );
    }
}
