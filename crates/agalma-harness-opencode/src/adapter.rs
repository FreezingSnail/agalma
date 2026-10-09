//! OpenCode `HarnessApi` adapter.
//!
//! Every vendor detail lives here and nowhere else: the `opencode serve`
//! invocation, the isolated XDG/`TMPDIR` environment, Basic auth, the
//! hand-rolled HTTP client, endpoint paths, native `ses_*` identifiers, and the
//! prompt text. The conductor and the contract never see any of it.
//!
//! Launch is not done directly: the adapter builds an attempt-scoped
//! [`agalma_contracts::sandbox::LaunchSpec`] and hands it to the injected
//! [`SandboxApi`] (the composition root supplies `agalma-sandbox`), so this crate
//! depends only on `agalma-contracts` in `src/`. The server's stdout/stderr are
//! inherited from the launcher; readiness is proved over loopback HTTP.
//!
//! The adapter is synchronous (the frozen `HarnessApi` is sync) but every method
//! that touches the sandbox must run inside a Tokio runtime, because
//! `SandboxApi::launch`/`terminate` drive the confined child through
//! `tokio::process`. The conductor calls it from its async dispatch loop.
//!
//! Transport: `std::net::TcpStream` with hand-rolled HTTP/1.1 (port of the S0d
//! client). Reconciliation uses the durable session projection
//! (`GET /api/session/{id}`) plus the durable-log sync marker; SSE is not parsed.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::error::ContractError;
use agalma_contracts::harness::{
    AttemptHandle, CancelAck, CreateSessionRequest, Describe, Event, HarnessApi, OperationHandle,
    OperationState, RunTurnRequest, SessionHandle, StartAttemptRequest, StopEvidence, API_VERSION,
    HARNESS_API,
};
use agalma_contracts::ids::{ArtifactRef, AttemptId, OperationId, SessionId};
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};
use serde_json::{json, Value};

use crate::http::HttpClient;
use crate::translate;

/// Reported implementation name (binding identity).
const IMPL_NAME: &str = "opencode";
/// Default model when no request/config/environment override is present.
pub const DEFAULT_MODEL: &str = "opencode/mimo-v2.6-flash-free";
/// Environment override for the default model.
const MODEL_ENV: &str = "AGALMA_MODEL";
/// Basic-auth user name the server requires (S0a).
const AUTH_USER: &str = "opencode";
/// Prompt for a normal turn.
const PROMPT_TURN: &str = "Reply with exactly: AGALMA_OK. Do not use any tools.";
/// Prompt for the long-running turn the cancellation scenario interrupts.
const PROMPT_CANCEL: &str =
    "Count slowly from 1 to 1000, one number per line. Do not use any tools. Do not stop before 1000.";
/// Hosts that must bypass the provider proxy (loopback control traffic).
const NO_PROXY: &str = "127.0.0.1,localhost";

/// Attempt-scoped configuration for one [`OpenCodeHarness`].
///
/// The four sandbox roots mirror [`LaunchSpec`]; the adapter never invents them.
/// `attempt_dir` is the Seatbelt `ATTEMPT` root: the isolated
/// `xdg-config`/`xdg-data`/`tmp`/`work` directories are created under it.
#[derive(Clone, Debug)]
pub struct OpenCodeHarnessConfig {
    /// Rendered, fail-closed sandbox profile (Seatbelt `-f` file).
    pub profile_path: PathBuf,
    /// Attempt-owned read/write root (Seatbelt `ATTEMPT`).
    pub attempt_dir: PathBuf,
    /// Protected-tests root, readable not writable (Seatbelt `PROTECTED`).
    pub protected_dir: PathBuf,
    /// Parent-owned Unix-socket directory (Seatbelt `SOCKDIR`).
    pub sock_dir: PathBuf,
    /// Extra read-only root (Seatbelt `EXTRA_RO`). When `None`, derived from the
    /// resolved opencode install tree.
    pub extra_ro: Option<PathBuf>,
    /// Additional read-only roots rendered into the remaining extra-root slots
    /// (Seatbelt `EXTRA_RO2`, `EXTRA_RO3`), e.g. toolchain homes the confined
    /// worker needs (`~/.cargo`, `~/.rustup`). At most two are honoured.
    pub extra_ro_roots: Vec<PathBuf>,
    /// Additional environment variables for the confined server, merged over
    /// the adapter's base environment (e.g. `RUSTUP_HOME`, `CARGO_HOME`,
    /// `PATH` additions for the toolchain).
    pub extra_env: BTreeMap<String, String>,
    /// Explicit opencode binary; when `None`, `OPENCODE_BIN` then `PATH`.
    pub program: Option<PathBuf>,
    /// Model override; when `None`, `AGALMA_MODEL` then [`DEFAULT_MODEL`].
    pub model: Option<String>,
    /// Optional parent-owned forward-proxy URL (e.g. `http://127.0.0.1:41234`).
    ///
    /// When set, the confined server receives `HTTPS_PROXY`/`HTTP_PROXY` (and
    /// their lower-case forms) plus `NO_PROXY=127.0.0.1,localhost`, so provider
    /// egress tunnels through the parent while loopback control traffic
    /// bypasses the proxy. When `None`, no proxy variables are injected (an
    /// unconfined or otherwise network-enabled launch stays reachable).
    pub proxy_url: Option<String>,
    /// How long `start_attempt` waits for `/api/info` to answer.
    pub readiness_timeout: Duration,
}

impl OpenCodeHarnessConfig {
    /// Build a config with the four sandbox roots and a 30 s readiness timeout.
    pub fn new(
        profile_path: impl Into<PathBuf>,
        attempt_dir: impl Into<PathBuf>,
        protected_dir: impl Into<PathBuf>,
        sock_dir: impl Into<PathBuf>,
    ) -> Self {
        OpenCodeHarnessConfig {
            profile_path: profile_path.into(),
            attempt_dir: attempt_dir.into(),
            protected_dir: protected_dir.into(),
            sock_dir: sock_dir.into(),
            extra_ro: None,
            extra_ro_roots: Vec::new(),
            extra_env: BTreeMap::new(),
            program: None,
            model: None,
            proxy_url: None,
            readiness_timeout: Duration::from_secs(30),
        }
    }
}

struct OcOp {
    native_session: String,
}

/// OpenCode-backed [`HarnessApi`].
pub struct OpenCodeHarness {
    sandbox: Box<dyn SandboxApi>,
    config: OpenCodeHarnessConfig,
    model: String,
    impl_version: String,
    http: Option<HttpClient>,
    child: Option<SandboxChild>,
    attempt_id: Option<AttemptId>,
    attempt: Option<AttemptHandle>,
    workspace: Option<String>,
    session_seq: u32,
    turn_seq: u32,
    sessions: Vec<SessionHandle>,
    session_native: BTreeMap<String, String>,
    /// Per-session prompt text resolved from `CreateSessionRequest::prompts_ref`
    /// when it names a readable file; absent entries fall back to the canned
    /// turn prompt.
    session_prompts: BTreeMap<String, String>,
    ops: BTreeMap<String, OcOp>,
    stopped: bool,
}

impl OpenCodeHarness {
    /// Construct an adapter around an injected sandbox backend.
    ///
    /// The sandbox implementation (the Seatbelt backend) is selected by the
    /// composition root, keeping this crate free of impl-to-impl edges.
    pub fn new(sandbox: Box<dyn SandboxApi>, config: OpenCodeHarnessConfig) -> Self {
        let model = config
            .model
            .clone()
            .or_else(|| std::env::var(MODEL_ENV).ok())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let version_source = config
            .program
            .clone()
            .or_else(|| std::env::var_os("OPENCODE_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("opencode"));
        OpenCodeHarness {
            sandbox,
            config,
            model,
            impl_version: detect_version(&version_source),
            http: None,
            child: None,
            attempt_id: None,
            attempt: None,
            workspace: None,
            session_seq: 0,
            turn_seq: 0,
            sessions: Vec::new(),
            session_native: BTreeMap::new(),
            session_prompts: BTreeMap::new(),
            ops: BTreeMap::new(),
            stopped: false,
        }
    }

    /// The pid of the confined server, once an attempt is active. Evidence for
    /// orphan checks in tests and recovery.
    pub fn attempt_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.pid)
    }

    fn http(&self) -> Result<&HttpClient, ContractError> {
        self.http
            .as_ref()
            .ok_or_else(|| ContractError::KnownFailure("adapter has no HTTP client".to_string()))
    }

    fn resolve_program(&self) -> Result<PathBuf, ContractError> {
        if let Some(program) = &self.config.program {
            return Ok(program.clone());
        }
        if let Some(program) = std::env::var_os("OPENCODE_BIN") {
            return Ok(PathBuf::from(program));
        }
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join("opencode");
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(ContractError::KnownFailure(
            "opencode binary not found; set OPENCODE_BIN or OpenCodeHarnessConfig::program"
                .to_string(),
        ))
    }

    fn spawn_server(&mut self, workspace: &str) -> Result<(), ContractError> {
        let attempt_id = self
            .attempt_id
            .clone()
            .ok_or_else(|| ContractError::KnownFailure("attempt id not allocated".to_string()))?;
        let program = self.resolve_program()?;
        let opencode_root = self
            .config
            .extra_ro
            .clone()
            .unwrap_or_else(|| derive_extra_ro(&program));
        let mut extra_ro_roots = vec![opencode_root];
        extra_ro_roots.extend(self.config.extra_ro_roots.iter().cloned());

        // Isolated attempt tree: <attempt>/{xdg-config,xdg-data,xdg-cache,
        // xdg-state,tmp,work}. `work` is the attempt-scoped HOME.
        let root = canonical_dir(&self.config.attempt_dir)?;
        let mut dirs = BTreeMap::new();
        for sub in [
            "xdg-config",
            "xdg-data",
            "xdg-cache",
            "xdg-state",
            "tmp",
            "work",
        ] {
            dirs.insert(sub, canonical_dir(&root.join(sub))?);
        }
        let protected = canonical_dir(&self.config.protected_dir)?;
        let sock = canonical_dir(&self.config.sock_dir)?;
        let profile = fs::canonicalize(&self.config.profile_path).map_err(|e| {
            ContractError::KnownFailure(format!(
                "sandbox profile {:?} cannot be resolved: {e}",
                self.config.profile_path
            ))
        })?;
        let workspace_abs = canonical_dir(Path::new(workspace))?;

        // Allocate a loopback port, then refuse to launch if something already
        // owns it: connecting to a stale server would silently break isolation.
        let port = allocate_port()?;
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Err(ContractError::KnownFailure(format!(
                "127.0.0.1:{port} already in use; refusing to bind to a foreign server"
            )));
        }
        let password = random_password();

        let mut env = BTreeMap::new();
        env.insert(
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin".to_string(),
        );
        for (key, dir) in [
            ("HOME", &dirs["work"]),
            ("TMPDIR", &dirs["tmp"]),
            ("XDG_CONFIG_HOME", &dirs["xdg-config"]),
            ("XDG_DATA_HOME", &dirs["xdg-data"]),
            ("XDG_CACHE_HOME", &dirs["xdg-cache"]),
            ("XDG_STATE_HOME", &dirs["xdg-state"]),
        ] {
            env.insert(key.to_string(), dir.to_string_lossy().into_owned());
        }
        env.insert("OPENCODE_PASSWORD".to_string(), password.clone());
        // Point OpenCode's global-config directory at the session worktree.
        // This makes `ConfigDiscovery.discover` short-circuit its ancestor walk
        // (`fs.resolve(location.directory) == global config root`), which the
        // M0 confine profile cannot satisfy: it allows only `file-read-metadata`
        // on ancestors of the attempt dir, while `fs.realPath` reads each one.
        // Vendor workaround, confined here; the checkout carries no
        // `opencode.json` so nothing is loaded from it.
        //
        // TODO(M0 workaround): this is an explicit short-circuit, not a real
        // fix. OpenCode still walks ancestor directories for project config, and
        // the profile's global `file-read-metadata` cannot satisfy a content
        // read of those ancestors. Retire when either (a) the confine profile
        // renders read-only ancestor roots, or (b) the attempt/state-dir layout
        // places every ancestor inside an already-permitted path. Tracked with
        // the config-discovery caveat in `docs/spikes/s0b-confinement.md`.
        env.insert(
            "OPENCODE_CONFIG_DIR".to_string(),
            workspace_abs.to_string_lossy().into_owned(),
        );

        // Provider egress: when the composition root supplies a parent-owned
        // loopback proxy, the confined server must route provider traffic
        // through it (the S0b profile denies arbitrary network). Loopback
        // control traffic (this server's own API, the nerve bridge) must bypass
        // the proxy, hence NO_PROXY. Both cases are set because runtimes differ
        // on which spelling they read; bun honours the upper-case pair.
        if let Some(proxy_url) = self.config.proxy_url.as_deref().filter(|u| !u.is_empty()) {
            for key in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
                env.insert(key.to_string(), proxy_url.to_string());
            }
            env.insert("NO_PROXY".to_string(), NO_PROXY.to_string());
            env.insert("no_proxy".to_string(), NO_PROXY.to_string());
        }

        // Caller-supplied environment (toolchain homes, PATH additions) merges
        // over the base environment. This is how the confined worker gets a
        // usable `cargo`/`rustc` without the parent environment leaking in.
        for (key, value) in &self.config.extra_env {
            env.insert(key.clone(), value.clone());
        }

        let spec = LaunchSpec {
            program: program.to_string_lossy().into_owned(),
            args: vec![
                "serve".to_string(),
                "--hostname".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                port.to_string(),
            ],
            cwd: workspace_abs.to_string_lossy().into_owned(),
            env,
            profile: profile.to_string_lossy().into_owned(),
            attempt: attempt_id,
            attempt_dir: root.to_string_lossy().into_owned(),
            protected_dir: protected.to_string_lossy().into_owned(),
            sock_dir: sock.to_string_lossy().into_owned(),
            extra_ro_roots: extra_ro_roots
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect(),
        };

        let child = self.sandbox.launch(spec)?;
        self.http = Some(HttpClient::new("127.0.0.1", port, AUTH_USER, &password));
        self.child = Some(child);
        self.wait_ready()
    }

    fn wait_ready(&mut self) -> Result<(), ContractError> {
        let deadline = Instant::now() + self.config.readiness_timeout;
        loop {
            if let Some(child) = self.child.as_ref() {
                if !self.sandbox.alive(child)? {
                    return Err(ContractError::KnownFailure(
                        "opencode serve exited before readiness".to_string(),
                    ));
                }
            }
            // `/api/info` accepting a connection is the readiness signal; 401
            // (no auth) is the documented and expected answer.
            if let Some(http) = self.http.as_ref() {
                if let Ok((code, _)) = http.request_unauth("GET", "/api/info", None) {
                    if code == 200 || code == 401 {
                        return Ok(());
                    }
                }
            }
            if Instant::now() >= deadline {
                let evidence = self.terminate_child();
                return Err(ContractError::KnownFailure(format!(
                    "opencode serve not ready within {:?}: {}",
                    self.config.readiness_timeout, evidence.detail
                )));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn terminate_child(&mut self) -> StopEvidence {
        match self.child.take() {
            Some(child) => self.sandbox.terminate(&child).unwrap_or(StopEvidence {
                process_group_gone: false,
                detail: format!("sandbox terminate failed for pid {}", child.pid),
            }),
            None => StopEvidence {
                process_group_gone: true,
                detail: "no live attempt process".to_string(),
            },
        }
    }

    fn native_for(&self, op: &OperationHandle) -> Result<String, ContractError> {
        self.ops
            .get(op.id.as_str())
            .map(|o| o.native_session.clone())
            .ok_or_else(|| ContractError::KnownFailure(format!("unknown operation {}", op.id)))
    }

    /// `GET /api/session/{id}` decoded as JSON.
    fn session_json(&self, native: &str) -> Result<Value, ContractError> {
        let path = format!("/api/session/{native}");
        let (code, body) = self
            .http()?
            .request("GET", &path, None)
            .map_err(ContractError::UnknownOutcome)?;
        if code != 200 {
            return Err(ContractError::UnknownOutcome(format!(
                "GET {path} -> {code}"
            )));
        }
        serde_json::from_str(&body)
            .map_err(|e| ContractError::UnknownOutcome(format!("session json: {e}")))
    }

    /// `GET /api/experimental/session/{id}/log` (no `follow`): the durable log
    /// closes after the sync marker. Best-effort reconciliation input.
    fn session_log(&self, native: &str) -> Result<String, ContractError> {
        let path = format!("/api/experimental/session/{native}/log");
        let (code, body) = self
            .http()?
            .request("GET", &path, None)
            .map_err(ContractError::UnknownOutcome)?;
        if code != 200 {
            return Err(ContractError::UnknownOutcome(format!(
                "GET {path} -> {code}"
            )));
        }
        Ok(body)
    }

    fn resolve_model(&self, requested: &str) -> String {
        if !requested.is_empty() && requested != "role-default" {
            requested.to_string()
        } else {
            self.model.clone()
        }
    }
}

impl Drop for OpenCodeHarness {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = self.terminate_child();
    }
}

impl HarnessApi for OpenCodeHarness {
    fn describe(&self) -> Describe {
        Describe {
            api: HARNESS_API.to_string(),
            api_version: API_VERSION,
            impl_name: IMPL_NAME.to_string(),
            impl_version: self.impl_version.clone(),
            // No optional capability is advertised; S0-scope proof is required
            // before this adapter may claim any of the six.
            capabilities: Default::default(),
        }
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, ContractError> {
        if self.child.is_some() {
            return Err(ContractError::Conflict(
                "an attempt is already active".to_string(),
            ));
        }
        self.attempt_id = Some(AttemptId::derive(1));
        self.workspace = Some(req.workspace.clone());
        self.spawn_server(&req.workspace)?;
        let handle = AttemptHandle {
            id: self.attempt_id.clone().expect("attempt id allocated above"),
        };
        self.attempt = Some(handle.clone());
        Ok(handle)
    }

    fn create_session(
        &mut self,
        req: CreateSessionRequest,
    ) -> Result<SessionHandle, ContractError> {
        let attempt = self
            .attempt_id
            .clone()
            .ok_or_else(|| ContractError::KnownFailure("start_attempt first".to_string()))?;
        let workspace = self
            .workspace
            .clone()
            .ok_or_else(|| ContractError::KnownFailure("start_attempt first".to_string()))?;
        self.session_seq += 1;
        let model = self.resolve_model(&req.model);
        let (provider, model_id) = split_model(&model);
        let body = json!({
            "title": format!("agalma-{}", req.role),
            "model": {"id": model_id, "providerID": provider},
            "location": {"directory": workspace},
        })
        .to_string();

        let (code, resp) = self
            .http()?
            .request("POST", "/api/session", Some(&body))
            .map_err(ContractError::KnownFailure)?;
        if code != 200 {
            return Err(ContractError::KnownFailure(format!(
                "create session -> {code}: {resp}"
            )));
        }
        let parsed: Value = serde_json::from_str(&resp)
            .map_err(|e| ContractError::UnknownOutcome(format!("session create: {e}")))?;
        let native = parsed
            .pointer("/data/id")
            .and_then(Value::as_str)
            .ok_or_else(|| ContractError::UnknownOutcome("session create missing id".to_string()))?
            .to_string();

        let handle = SessionHandle {
            id: SessionId::derive(&attempt, self.session_seq),
        };
        if let Some(prompt) = resolve_prompt(&req.prompts_ref) {
            self.session_prompts.insert(handle.id.to_string(), prompt);
        }
        self.session_native.insert(handle.id.to_string(), native);
        self.sessions.push(handle.clone());
        Ok(handle)
    }

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError> {
        let native = self
            .session_native
            .get(session.id.as_str())
            .cloned()
            .ok_or_else(|| {
                ContractError::KnownFailure(format!("unknown session {}", session.id))
            })?;
        let attempt = self
            .attempt_id
            .clone()
            .ok_or_else(|| ContractError::KnownFailure("start_attempt first".to_string()))?;
        self.turn_seq += 1;
        let prompt = self
            .session_prompts
            .get(session.id.as_str())
            .cloned()
            .unwrap_or_else(|| prompt_for(&req.input_ref));
        let body = json!({ "text": prompt }).to_string();
        let path = format!("/api/session/{native}/prompt");
        let (code, resp) = self
            .http()?
            .request("POST", &path, Some(&body))
            .map_err(ContractError::KnownFailure)?;
        if code != 200 {
            return Err(ContractError::KnownFailure(format!(
                "prompt -> {code}: {resp}"
            )));
        }
        let handle = OperationHandle {
            id: OperationId::new(format!("op:{}:turn/{}", attempt.as_str(), self.turn_seq)),
        };
        self.ops.insert(
            handle.id.to_string(),
            OcOp {
                native_session: native,
            },
        );
        Ok(handle)
    }

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, ContractError> {
        let native = self.native_for(op)?;
        let session = self.session_json(&native)?;
        Ok(translate::state_from_session(&session))
    }

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
        let native = self.native_for(op)?;
        let session = self.session_json(&native)?;
        // The durable log is best-effort *and* only trusted once its sync marker
        // is present; a missing/altered log must not fail a turn whose projection
        // is authoritative.
        let log = self.session_log(&native).ok();
        let synced = log.as_deref().filter(|body| translate::log_synced(body));
        Ok(translate::events_from(&session, synced))
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, ContractError> {
        let native = self.native_for(op)?;
        let path = format!("/api/session/{native}/interrupt");
        let (code, resp) = self
            .http()?
            .request("POST", &path, None)
            .map_err(ContractError::KnownFailure)?;
        if code != 200 {
            return Err(ContractError::KnownFailure(format!(
                "interrupt -> {code}: {resp}"
            )));
        }
        let acknowledged = serde_json::from_str::<Value>(&resp)
            .ok()
            .and_then(|v| v.get("interrupted").and_then(Value::as_bool))
            .unwrap_or(false);
        Ok(CancelAck {
            acknowledged,
            // Acknowledgment is not termination; termination evidence comes from
            // StopAttempt once the process tree is confirmed gone.
            terminated: false,
            detail: "opencode: interrupt request".to_string(),
        })
    }

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), ContractError> {
        if let Some(native) = self.session_native.remove(session.id.as_str()) {
            let path = format!("/api/session/{native}");
            let _ = self.http()?.request("DELETE", &path, None);
        }
        self.session_prompts.remove(session.id.as_str());
        self.sessions.retain(|s| s.id != session.id);
        Ok(())
    }

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, ContractError> {
        let _ = attempt;
        let evidence = self.terminate_child();
        self.stopped = true;
        self.attempt = None;
        self.attempt_id = None;
        self.http = None;
        Ok(evidence)
    }
}

/// Derive the read-only harness install tree from the resolved binary, matching
/// the S0b p08 derivation: the `packages/cli/` prefix for a source install, else
/// the binary's parent directory.
fn derive_extra_ro(program: &Path) -> PathBuf {
    let real = fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let text = real.to_string_lossy();
    if let Some(idx) = text.find("/packages/cli/") {
        let end = idx + "/packages/cli/".len();
        return PathBuf::from(&text[..end]);
    }
    real.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| real.clone())
}

/// Split `provider/model` into provider and model id.
fn split_model(model: &str) -> (String, String) {
    match model.split_once('/') {
        Some((provider, id)) => (provider.to_string(), id.to_string()),
        None => ("opencode".to_string(), model.to_string()),
    }
}

/// Map a canonical input artifact to prompt text (vendor prompt confined here).
fn prompt_for(input_ref: &ArtifactRef) -> String {
    if input_ref.as_str().contains("cancel") {
        PROMPT_CANCEL.to_string()
    } else {
        PROMPT_TURN.to_string()
    }
}

/// Resolve a `prompts_ref` that names a readable file on the parent filesystem
/// into its text. The conductor points this at its builder prompt; the live
/// smoke tests pass a canonical artifact name that does not resolve, so the
/// canned turn prompt is used instead.
fn resolve_prompt(prompts_ref: &str) -> Option<String> {
    let path = Path::new(prompts_ref);
    if path.is_file() {
        fs::read_to_string(path)
            .ok()
            .filter(|text| !text.is_empty())
    } else {
        None
    }
}

/// Create `path` if needed and return its canonical form (Seatbelt params must
/// be absolute and resolved).
fn canonical_dir(path: &Path) -> Result<PathBuf, ContractError> {
    fs::create_dir_all(path)
        .map_err(|e| ContractError::KnownFailure(format!("mkdir {}: {e}", path.display())))?;
    fs::canonicalize(path)
        .map_err(|e| ContractError::KnownFailure(format!("resolve {}: {e}", path.display())))
}

/// Bind an ephemeral loopback port and release it for the child to take.
fn allocate_port() -> Result<u16, ContractError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| ContractError::KnownFailure(format!("allocate port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| ContractError::KnownFailure(format!("local_addr: {e}")))?
        .port();
    drop(listener);
    Ok(port)
}

/// A per-attempt `OPENCODE_PASSWORD`, from `/dev/urandom` (fallback: time+pid).
fn random_password() -> String {
    let mut buf = [0u8; 24];
    if let Ok(mut file) = File::open("/dev/urandom") {
        if file.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:032x}{:08x}", std::process::id())
}

/// Best-effort `opencode --version`; `unknown` on any failure.
fn detect_version(program: &Path) -> String {
    match Command::new(program).arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .last()
            .unwrap_or("unknown")
            .trim()
            .to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_model_handles_provider_prefix() {
        assert_eq!(
            split_model("opencode/mimo-v2.6-flash-free"),
            ("opencode".to_string(), "mimo-v2.6-flash-free".to_string())
        );
        assert_eq!(
            split_model("bare-model"),
            ("opencode".to_string(), "bare-model".to_string())
        );
    }

    #[test]
    fn extra_ro_uses_packages_cli_prefix_for_source_install() {
        let derived = derive_extra_ro(Path::new(
            "/Users/x/code/opencode/packages/cli/dist/bin/opencode",
        ));
        assert_eq!(
            derived,
            PathBuf::from("/Users/x/code/opencode/packages/cli/")
        );
    }

    #[test]
    fn extra_ro_falls_back_to_parent_for_standalone_binary() {
        // A non-existent path cannot be canonicalized; the parent is used.
        let derived = derive_extra_ro(Path::new("/opt/tools/bin/opencode"));
        assert_eq!(derived, PathBuf::from("/opt/tools/bin"));
    }

    #[test]
    fn prompt_selects_cancel_prompt_for_cancel_input() {
        assert_eq!(
            prompt_for(&ArtifactRef::derive("cancel-turn")),
            PROMPT_CANCEL
        );
        assert_eq!(prompt_for(&ArtifactRef::derive("turn-1")), PROMPT_TURN);
    }

    #[test]
    fn resolve_prompt_reads_existing_file_and_rejects_artifact_names() {
        assert_eq!(resolve_prompt("artifact:builder-prompts"), None);
        assert_eq!(resolve_prompt("/definitely/not/a/file"), None);
        // Run dirs live under `target/test-runs/` (never /tmp).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-runs");
        fs::create_dir_all(&dir).expect("create run dir");
        let path = dir.join(format!("harness-prompt-{}.txt", std::process::id()));
        fs::write(&path, "fix it").expect("write prompt");
        assert_eq!(
            resolve_prompt(&path.to_string_lossy()),
            Some("fix it".into())
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn random_password_is_hex_and_unique() {
        let a = random_password();
        let b = random_password();
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
