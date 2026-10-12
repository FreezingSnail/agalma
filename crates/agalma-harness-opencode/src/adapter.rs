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
use crate::provider_lease::{self, ProviderLease};
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
    /// Extra read/write root (Seatbelt `EXTRA_RW`; at most one honoured), e.g.
    /// the target repo a planning session edits. Keeping the repo here instead
    /// of making it `attempt_dir` keeps OpenCode's runtime state (XDG dirs, its
    /// git snapshot store) out of the worktree it snapshots.
    pub extra_rw_roots: Vec<PathBuf>,
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
    /// Paid HTTP-forward mode (`agalma-csw.2`): when set, the isolated XDG
    /// config points `provider_id` at `provider_base_url` (the loopback paid
    /// proxy) with `provider_api_key` as a **dummy** credential. The real token
    /// never enters the sandbox; the parent proxy injects it upstream.
    pub provider_base_url: Option<String>,
    /// Provider id to override (e.g. `opencode-go`). Defaults to `opencode-go`
    /// when [`OpenCodeHarnessConfig::provider_base_url`] is set.
    pub provider_id: Option<String>,
    /// Dummy API key for the overridden provider (never a real credential).
    pub provider_api_key: Option<String>,
    /// How long `start_attempt` waits for `/api/info` to answer.
    pub readiness_timeout: Duration,
    /// Keep the attempt's runtime state (XDG dirs, tmp, HOME, `opencode.db`)
    /// after the attempt stops, for forensics. Defaults from
    /// [`KEEP_STATE_ENV`]; off means the state is removed on stop (the server
    /// log is always kept).
    pub keep_runtime_state: bool,
}

/// Environment switch (`1`) that keeps attempt runtime state after stop.
pub const KEEP_STATE_ENV: &str = "AGALMA_HARNESS_KEEP_STATE";

/// Config document handed to every server via `OPENCODE_CONFIG_CONTENT` (the
/// highest-precedence OpenCode v2 config source). Agalma never rewinds through
/// OpenCode — rollback is the workspace's git (candidate branch, CAS integrate,
/// revert) — so OpenCode's per-step git snapshots of the workspace are pure
/// cost: they re-capture the worktree every few seconds into an object store
/// that is never packed (`agalma-mlw`, `agalma-22f`).
const SERVER_CONFIG_CONTENT: &str = r#"{"snapshots":false}"#;

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
            extra_rw_roots: Vec::new(),
            extra_env: BTreeMap::new(),
            program: None,
            model: None,
            proxy_url: None,
            provider_base_url: None,
            provider_id: None,
            provider_api_key: None,
            readiness_timeout: Duration::from_secs(30),
            keep_runtime_state: std::env::var(KEEP_STATE_ENV).is_ok_and(|v| v == "1"),
        }
    }
}

/// Default provider id used when no explicit id is configured.
pub const DEFAULT_PAID_PROVIDER_ID: &str = "opencode-go";
/// Dummy credential handed to the confined provider in paid mode.
pub const DUMMY_PROVIDER_KEY: &str = "agalma-proxy";

/// Render the isolated OpenCode config that overrides a provider's base URL
/// (`agalma-csw.2`).
///
/// Shape verified against the OpenCode v2 provider config
/// (<https://opencode.ai/docs/providers/>, "Base URL"): the provider is keyed by
/// id under `provider`, and the endpoint is `options.baseURL`. The api key is
/// provided separately via the provider's env var (`OPENCODE_API_KEY`), so the
/// config carries no credential.
pub fn render_provider_config(provider_id: &str, base_url: &str) -> String {
    serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            provider_id: {
                "options": {
                    "baseURL": base_url,
                }
            }
        }
    })
    .to_string()
}

struct OcOp {
    native_session: String,
}

/// OpenCode-backed [`HarnessApi`].
pub struct OpenCodeHarness {
    sandbox: Box<dyn SandboxApi + Send>,
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
    provider_config: Option<ProviderLease>,
    /// `(attempt root, workspace)` of the launched server, for runtime-state
    /// cleanup once the server is gone.
    runtime: Option<(PathBuf, PathBuf)>,
    stopped: bool,
}

impl OpenCodeHarness {
    /// Construct an adapter around an injected sandbox backend.
    ///
    /// The sandbox implementation (the Seatbelt backend) is selected by the
    /// composition root, keeping this crate free of impl-to-impl edges.
    pub fn new(sandbox: Box<dyn SandboxApi + Send>, config: OpenCodeHarnessConfig) -> Self {
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
            provider_config: None,
            runtime: None,
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
        // xdg-state,tmp,work}. `work` is the attempt-scoped HOME. The guard runs
        // before any subdirectory is created so a refused launch leaves nothing
        // behind in the workspace.
        let root = canonical_dir(&self.config.attempt_dir)?;
        let workspace_abs = canonical_dir(Path::new(workspace))?;
        ensure_runtime_outside_workspace(&root, &workspace_abs)?;
        // Recover a provider override leaked by a dead attempt before anything
        // reads the workspace config; a live foreign lease is a conflict.
        provider_lease::reconcile(&workspace_abs)?;
        // A crashed earlier attempt in this root never ran its cleanup; start
        // from empty runtime state rather than inheriting its database/caches.
        clear_runtime_state(&root, &workspace_abs);
        self.runtime = Some((root.clone(), workspace_abs.clone()));
        let mut dirs = BTreeMap::new();
        for sub in RUNTIME_SUBDIRS {
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
        let extra_rw_roots = self
            .config
            .extra_rw_roots
            .iter()
            .map(|dir| canonical_dir(dir))
            .collect::<Result<Vec<_>, _>>()?;

        // Allocate a loopback port for the child. `allocate_port` probes the
        // port after releasing it and retries if a concurrent process grabbed
        // it, so a launch never targets an already-owned port and a transient
        // sibling listener cannot flip this call's error classification
        // (agalma-9me).
        let port = allocate_port()?;
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

        // Paid HTTP-forward mode: point the provider at the loopback proxy with
        // a dummy key. The provider config must live in `OPENCODE_CONFIG_DIR`
        // (== the workspace; see the short-circuit note above), so it is written
        // there under a lease (restored on stop, recovered after a crash) and
        // locally excluded from the candidate commit.
        if let Some(base_url) = self
            .config
            .provider_base_url
            .clone()
            .filter(|url| !url.is_empty())
        {
            let provider_id = self
                .config
                .provider_id
                .clone()
                .unwrap_or_else(|| DEFAULT_PAID_PROVIDER_ID.to_string());
            let lease = provider_lease::acquire(
                &workspace_abs,
                render_provider_config(&provider_id, &base_url),
            )?;
            self.provider_config = Some(lease);
            env.insert(
                "OPENCODE_API_KEY".to_string(),
                self.config
                    .provider_api_key
                    .clone()
                    .unwrap_or_else(|| DUMMY_PROVIDER_KEY.to_string()),
            );
        }

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
        // Apply this after extra_env so every launch disables snapshots.
        env.insert(
            "OPENCODE_CONFIG_CONTENT".to_string(),
            SERVER_CONFIG_CONTENT.to_string(),
        );

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
            extra_rw_roots: extra_rw_roots
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

    fn cleanup_provider_config(&mut self) {
        if let Some(lease) = self.provider_config.take() {
            lease.release();
        }
    }

    /// Remove the stopped server's runtime state unless kept for forensics.
    /// Skipped while the process group may still be alive (it could still be
    /// writing); the next launch in the same root clears it instead.
    fn cleanup_runtime_state(&mut self, evidence: &StopEvidence) {
        if self.config.keep_runtime_state || !evidence.process_group_gone {
            return;
        }
        if let Some((root, workspace)) = self.runtime.take() {
            clear_runtime_state(&root, &workspace);
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

    /// `GET /api/session/{id}/message`: the live projected messages. Best-effort
    /// progress input (a running turn's tool parts move through `state.status`).
    fn session_messages(&self, native: &str) -> Result<String, ContractError> {
        let path = format!("/api/session/{native}/message");
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
        let evidence = self.terminate_child();
        self.cleanup_provider_config();
        self.cleanup_runtime_state(&evidence);
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
        self.stopped = false;
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
        let prompt = posted_prompt(
            self.session_prompts
                .get(session.id.as_str())
                .map(String::as_str),
            &req.input_ref,
        );
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
        // The live message list carries a running turn's tool-part state; it is
        // pure progress, never authoritative, so a failure just drops it.
        let messages = self.session_messages(&native).ok();
        Ok(translate::events_from(
            &session,
            synced,
            messages.as_deref(),
        ))
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, ContractError> {
        let native = self.native_for(op)?;
        let path = format!("/api/session/{native}/interrupt");
        let (code, resp) = self
            .http()?
            .request("POST", &path, None)
            .map_err(ContractError::KnownFailure)?;
        cancel_ack_from_response(code, &resp)
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
        self.cleanup_provider_config();
        self.cleanup_runtime_state(&evidence);
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

/// Attempt-root subdirectories the adapter renders as the server's runtime
/// state (`HOME`, `TMPDIR`, `XDG_*`).
const RUNTIME_SUBDIRS: [&str; 6] = [
    "xdg-config",
    "xdg-data",
    "xdg-cache",
    "xdg-state",
    "tmp",
    "work",
];

/// Refuse a launch whose attempt root sits inside the workspace unless git
/// ignores it (`agalma-22f`).
///
/// The attempt root holds OpenCode's runtime state, including the git snapshot
/// store it keeps of the workspace. OpenCode snapshots every unignored file in
/// the workspace, so an unignored store inside the worktree snapshots itself and
/// grows without bound (observed: 3.8 GB, 272k files, `git add --all` pinned at
/// ~900% CPU, every OpenCode session in the repo hung). An attempt root equal to
/// the workspace can never be ignored and is always refused; a non-git
/// workspace has no ignore rules, so a nested root is refused there too.
fn ensure_runtime_outside_workspace(root: &Path, workspace: &Path) -> Result<(), ContractError> {
    if !root.starts_with(workspace) {
        return Ok(());
    }
    let ignored = root != workspace
        && Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["check-ignore", "-q", "--"])
            .arg(root)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    if ignored {
        return Ok(());
    }
    Err(ContractError::KnownFailure(format!(
        "attempt root {} is inside workspace {} and not git-ignored; the harness \
         runtime state ({}) would be snapshotted into the worktree. Use an attempt \
         root outside the workspace and grant the workspace as an extra read/write root.",
        root.display(),
        workspace.display(),
        RUNTIME_SUBDIRS.join(", ")
    )))
}

/// Remove the per-attempt runtime subdirectories under `root` (`agalma-mlw`).
///
/// The server's log is kept: `xdg-data/opencode/log` moves to
/// `<root>/opencode-log-<unique-id>` (small, and the evidence stall analysis
/// reads). A subdirectory that is or contains `workspace` is never touched —
/// some layouts nest the worktree under the attempt root (e.g. `work`).
/// Symlinks are unlinked without traversing them, including when finding the
/// log directory. If preserving a log fails, its runtime directory is retained.
/// Best-effort: failures leave state for the next launch to clear.
fn clear_runtime_state(root: &Path, workspace: &Path) {
    for sub in RUNTIME_SUBDIRS {
        let dir = root.join(sub);
        if workspace.starts_with(&dir) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if !metadata.is_dir() {
            let _ = fs::remove_file(&dir);
            continue;
        }
        if sub == "xdg-data" {
            let data = dir.join("opencode");
            // Do not move another directory's logs via a symlink left by a
            // worker (remove_dir_all below will only unlink the symlink).
            if fs::symlink_metadata(&data).is_ok_and(|m| m.is_dir()) {
                let log = data.join("log");
                if fs::symlink_metadata(&log).is_ok_and(|m| m.is_dir())
                    && fs::rename(
                        &log,
                        root.join(format!("opencode-log-{}", random_password())),
                    )
                    .is_err()
                {
                    continue;
                }
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Decide the outcome of a cancel from the `interrupt` response.
///
/// A `404` means the session is already gone: it either ended on its own or a
/// sibling closed it. That is benign — the conductor cancels *before* closing
/// the session, so a 404 here must never surface as a false
/// `SessionNotFoundError`. A non-200/404 status is a real error.
fn cancel_ack_from_response(code: u16, resp: &str) -> Result<CancelAck, ContractError> {
    if code == 404 {
        return Ok(CancelAck {
            acknowledged: false,
            terminated: false,
            detail: "opencode: session already gone (404)".to_string(),
        });
    }
    if code != 200 {
        return Err(ContractError::KnownFailure(format!(
            "interrupt -> {code}: {resp}"
        )));
    }
    let acknowledged = serde_json::from_str::<Value>(resp)
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

/// The exact text a turn posts to the vendor: the session prompt resolved from
/// `CreateSessionRequest::prompts_ref` when the file was readable, else the
/// canned fallback keyed by the canonical input artifact.
///
/// A role that composes real per-task input (e.g. the designer) always supplies
/// a readable prompt file, so its composed text — never an opaque artifact id —
/// is what reaches the vendor. This is the single seam the payload tests pin.
pub fn posted_prompt(session_prompt: Option<&str>, input_ref: &ArtifactRef) -> String {
    session_prompt
        .map(str::to_string)
        .unwrap_or_else(|| prompt_for(input_ref))
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

/// Bind an ephemeral loopback port, release it, and confirm the child can take
/// it: the port is re-probed after release and skipped when a concurrent
/// process grabbed it.
///
/// Under parallel test load a sibling process can bind a just-released
/// ephemeral port in the window between `bind` and the liveness probe; the old
/// single-shot probe surfaced that as a spurious "already in use" start
/// failure (agalma-9me). Retrying a bounded number of times keeps a transient
/// occupant from flipping the caller's error classification.
fn allocate_port() -> Result<u16, ContractError> {
    allocate_port_with(|port| TcpStream::connect(("127.0.0.1", port)).is_ok())
}

/// `allocate_port` with an injectable occupancy probe so tests can drive the
/// retry path deterministically instead of racing a real listener.
fn allocate_port_with<F>(mut occupied: F) -> Result<u16, ContractError>
where
    F: FnMut(u16) -> bool,
{
    for _ in 0..64 {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| ContractError::KnownFailure(format!("allocate port: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| ContractError::KnownFailure(format!("local_addr: {e}")))?
            .port();
        drop(listener);
        if !occupied(port) {
            return Ok(port);
        }
    }
    Err(ContractError::KnownFailure(
        "no free loopback port after 64 attempts".to_string(),
    ))
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
    use crate::config::{build_config, OpenCodeConfigInit};
    use crate::server::{config_from_lookup, LaunchConfig, LAUNCH_CONFIG_ENV};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn cancel_404_is_already_gone_not_an_error() {
        // Our own close before a cancel, or a session that ended on its own,
        // must never surface as a false SessionNotFoundError.
        let ack = cancel_ack_from_response(404, "{\"_tag\":\"SessionNotFoundError\"}")
            .expect("404 tolerates as already gone");
        assert!(!ack.acknowledged);
        assert!(!ack.terminated);
        assert!(ack.detail.contains("already gone"), "{ack:?}");
    }

    #[test]
    fn cancel_200_reports_acknowledgment() {
        let ack = cancel_ack_from_response(200, "{\"interrupted\":true}").expect("200 ok");
        assert!(ack.acknowledged);
        assert!(!ack.terminated, "ack is not termination");
        let ack = cancel_ack_from_response(200, "{\"interrupted\":false}").expect("200 ok");
        assert!(!ack.acknowledged);
    }

    #[test]
    fn cancel_other_status_is_an_error() {
        assert!(cancel_ack_from_response(500, "boom").is_err());
    }

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
    fn posted_prompt_prefers_the_resolved_session_prompt() {
        // A composed per-task prompt (epic id + repository) is posted verbatim;
        // only a missing session prompt falls back to the canned text.
        let input = ArtifactRef::derive("designer-turn");
        let composed = "## Input (this epic)\n\nepic: agalma-aaa\nrepository: /repos/alpha\n";
        assert_eq!(posted_prompt(Some(composed), &input), composed);
        assert_eq!(posted_prompt(None, &input), PROMPT_TURN);
        assert_eq!(
            posted_prompt(None, &ArtifactRef::derive("cancel-turn")),
            PROMPT_CANCEL
        );
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
    fn render_provider_config_uses_options_baseurl_shape() {
        let json = render_provider_config("opencode-go", "http://127.0.0.1:4444");
        let value: Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            value
                .pointer("/provider/opencode-go/options/baseURL")
                .and_then(Value::as_str),
            Some("http://127.0.0.1:4444")
        );
        // No credential is ever embedded in the config.
        assert!(!json.contains("Bearer"));
        assert!(!json.contains("apiKey"));
    }

    /// Records launches and reports the child dead, so `start_attempt` fails
    /// fast at readiness without a real server.
    #[derive(Clone, Default)]
    struct RecordingSandbox {
        launches: std::sync::Arc<std::sync::Mutex<Vec<LaunchSpec>>>,
    }

    impl RecordingSandbox {
        fn launches(&self) -> Vec<LaunchSpec> {
            self.launches.lock().expect("launch log").clone()
        }
    }

    impl SandboxApi for RecordingSandbox {
        fn launch(&mut self, spec: LaunchSpec) -> Result<SandboxChild, ContractError> {
            let attempt = spec.attempt.clone();
            self.launches.lock().expect("launch log").push(spec);
            Ok(SandboxChild {
                pid: 0,
                pgid: 0,
                attempt,
                started_at_unix_ms: 0,
            })
        }

        fn alive(&self, _child: &SandboxChild) -> Result<bool, ContractError> {
            Ok(false)
        }

        fn terminate(&mut self, _child: &SandboxChild) -> Result<StopEvidence, ContractError> {
            Ok(StopEvidence {
                process_group_gone: true,
                detail: "fake".to_string(),
            })
        }
    }

    /// `<target>/test-runs/adapter-<name>-<pid>/` with a git workspace `repo/`.
    fn layout(name: &str) -> (PathBuf, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-runs")
            .join(format!("adapter-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("repo")).expect("layout");
        let root = fs::canonicalize(&root).expect("canonical root");
        fs::write(root.join("worker.sb"), "(version 1)\n(deny default)\n").expect("profile");
        let workspace = root.join("repo");
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status()
            .expect("git init");
        assert!(status.success(), "git init");
        (root, workspace)
    }

    fn harness(root: &Path, attempt: &Path, sandbox: &RecordingSandbox) -> OpenCodeHarness {
        let mut config = OpenCodeHarnessConfig::new(
            root.join("worker.sb"),
            attempt,
            root.join("protected"),
            root.join("sock"),
        );
        config.program = Some(PathBuf::from("/usr/bin/true"));
        config.extra_ro = Some(PathBuf::from("/usr/bin"));
        OpenCodeHarness::new(Box::new(sandbox.clone()), config)
    }

    fn start(harness: &mut OpenCodeHarness, workspace: &Path) -> ContractError {
        harness
            .start_attempt(StartAttemptRequest {
                workspace: workspace.to_string_lossy().into_owned(),
                role: "designer".to_string(),
                genome_ref: ArtifactRef::derive("genome").to_string(),
                constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
                limits: agalma_contracts::harness::Limits {
                    wall_ms: 1_000,
                    tokens: 0,
                    cost_micros: 0,
                },
                operation_id: OperationId::new("op:adapter-test"),
            })
            .expect_err("the fake child is never ready")
    }

    fn untracked(workspace: &Path) -> String {
        let out = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(workspace)
            .output()
            .expect("git status");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn dead_pid() -> u32 {
        let mut child = Command::new("/usr/bin/true").spawn().expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap true");
        pid
    }

    #[test]
    fn runtime_state_stays_outside_a_workspace_granted_read_write() {
        // agalma-22f: a planning session edits the repo (EXTRA_RW) while
        // OpenCode's XDG/tmp/HOME state — its git snapshot store included —
        // lives in a conductor-owned attempt root, never in the worktree.
        let (root, workspace) = layout("runtime-outside");
        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &root.join("attempt"), &sandbox);
        h.config.extra_rw_roots = vec![workspace.clone()];
        let err = start(&mut h, &workspace);
        assert!(err.to_string().contains("readiness"), "{err}");

        let launches = sandbox.launches();
        assert_eq!(launches.len(), 1, "one launch");
        let spec = &launches[0];
        assert_eq!(spec.attempt_dir, root.join("attempt").to_string_lossy());
        assert_eq!(
            spec.extra_rw_roots,
            vec![workspace.to_string_lossy().into_owned()]
        );
        for key in [
            "HOME",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
        ] {
            let value = Path::new(&spec.env[key]);
            assert!(
                !value.starts_with(&workspace),
                "{key}={} must not be inside the workspace",
                value.display()
            );
        }
        for sub in RUNTIME_SUBDIRS {
            assert!(!workspace.join(sub).exists(), "{sub} leaked into the repo");
        }
        assert_eq!(untracked(&workspace), "", "worktree stays clean");
        drop(h);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unignored_attempt_root_inside_workspace_is_refused() {
        let (root, workspace) = layout("runtime-refused");
        for attempt in [workspace.clone(), workspace.join(".agalma-runtime")] {
            let sandbox = RecordingSandbox::default();
            let mut h = harness(&root, &attempt, &sandbox);
            let err = start(&mut h, &workspace);
            assert!(
                matches!(&err, ContractError::KnownFailure(m) if m.contains("not git-ignored")),
                "attempt {}: {err:?}",
                attempt.display()
            );
            assert!(sandbox.launches().is_empty(), "refused before launch");
            for sub in RUNTIME_SUBDIRS {
                assert!(
                    !workspace.join(sub).exists(),
                    "{sub} created before refusal"
                );
                assert!(!attempt.join(sub).exists(), "{sub} created before refusal");
            }
        }
        assert_eq!(
            untracked(&workspace),
            "",
            "a refused launch leaves no files"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ignored_attempt_root_inside_workspace_is_allowed() {
        let (root, workspace) = layout("runtime-ignored");
        fs::write(workspace.join(".git/info/exclude"), ".agalma-runtime/\n").expect("exclude");
        let sandbox = RecordingSandbox::default();
        let attempt = workspace.join(".agalma-runtime");
        let mut h = harness(&root, &attempt, &sandbox);
        let err = start(&mut h, &workspace);
        assert!(err.to_string().contains("readiness"), "{err}");
        assert_eq!(sandbox.launches().len(), 1, "guard passes an ignored root");
        assert_eq!(
            untracked(&workspace),
            "",
            "ignored runtime state stays invisible"
        );
        drop(h);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_start_releases_the_paid_override_on_drop() {
        // A user's own loopback config (local model server) must come back
        // byte-exact; the old heuristic would have deleted it.
        let (root, workspace) = layout("paid-release");
        let user = br#"{"provider":{"ollama":{"options":{"baseURL":"http://127.0.0.1:11434"}}}}"#;
        fs::write(workspace.join(provider_lease::CONFIG_FILE), user).expect("user config");
        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &root.join("attempt"), &sandbox);
        h.config.provider_base_url = Some("http://127.0.0.1:9".to_string());
        start(&mut h, &workspace);

        let spec = &sandbox.launches()[0];
        assert_eq!(spec.env["OPENCODE_API_KEY"], DUMMY_PROVIDER_KEY);
        let held = fs::read_to_string(workspace.join(provider_lease::CONFIG_FILE)).unwrap();
        assert!(
            held.contains("127.0.0.1:9\""),
            "override held while leased: {held}"
        );

        drop(h);
        assert_eq!(
            fs::read(workspace.join(provider_lease::CONFIG_FILE)).unwrap(),
            user
        );
        assert!(!workspace.join(provider_lease::LEASE_FILE).exists());
        assert_eq!(
            untracked(&workspace),
            "",
            "override and lease record stay locally excluded"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn any_later_launch_recovers_an_override_leaked_by_a_dead_owner() {
        // The SIGKILL case: no Drop ran. The next launch in the workspace — even
        // a free-route one with no override of its own — restores the user file.
        let (root, workspace) = layout("leak-recovery");
        let user = "{\"theme\":\"user\"}";
        let leaked = render_provider_config("opencode-go", "http://127.0.0.1:59163");
        fs::write(workspace.join(provider_lease::CONFIG_FILE), &leaked).unwrap();
        fs::write(
            workspace.join(provider_lease::LEASE_FILE),
            json!({"owner_pid": dead_pid(), "generated": leaked, "previous": user}).to_string(),
        )
        .unwrap();

        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &root.join("attempt"), &sandbox);
        start(&mut h, &workspace);
        assert_eq!(
            fs::read_to_string(workspace.join(provider_lease::CONFIG_FILE)).unwrap(),
            user
        );
        assert!(!workspace.join(provider_lease::LEASE_FILE).exists());
        drop(h);
        let _ = fs::remove_dir_all(&root);
    }

    /// Simulate what a server leaves behind: log, snapshot store, db, bun libs.
    fn seed_runtime(attempt: &Path) {
        let data = attempt.join("xdg-data").join("opencode");
        fs::create_dir_all(data.join("log")).unwrap();
        fs::write(data.join("log").join("opencode.log"), "server log\n").unwrap();
        fs::create_dir_all(data.join("snapshot").join("objects")).unwrap();
        fs::write(data.join("snapshot").join("objects").join("blob"), "x").unwrap();
        fs::write(data.join("opencode.db"), "db").unwrap();
        fs::create_dir_all(attempt.join("tmp")).unwrap();
        fs::write(attempt.join("tmp").join(".bun-lib.dylib"), "lib").unwrap();
    }

    /// Preserved `opencode-log-*` dirs under `attempt` holding the server log.
    fn preserved_logs(attempt: &Path) -> Vec<PathBuf> {
        fs::read_dir(attempt)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("opencode-log-"))
                    && p.join("opencode.log").is_file()
            })
            .collect()
    }

    #[test]
    fn every_launch_disables_opencode_snapshots() {
        // agalma-mlw: Agalma never rewinds through OpenCode, so its per-step git
        // snapshots of the workspace are pure cost (the 3.8 GB store).
        let (root, workspace) = layout("no-snapshots");
        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &root.join("attempt"), &sandbox);
        h.config.extra_env.insert(
            "OPENCODE_CONFIG_CONTENT".to_string(),
            r#"{"snapshots":true}"#.to_string(),
        );
        start(&mut h, &workspace);
        let spec = &sandbox.launches()[0];
        let content: Value =
            serde_json::from_str(&spec.env["OPENCODE_CONFIG_CONTENT"]).expect("config json");
        assert_eq!(content["snapshots"], Value::Bool(false), "{content}");
        drop(h);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_runtime_state_is_cleared_before_launch() {
        // A crashed attempt never cleaned up; the next launch must not inherit
        // its database, snapshot store, or temp files — but keeps its log.
        let (root, workspace) = layout("stale-runtime");
        let attempt = root.join("attempt");
        seed_runtime(&attempt);
        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &attempt, &sandbox);
        h.config.keep_runtime_state = true; // isolate the pre-launch clear
        start(&mut h, &workspace);
        let data = attempt.join("xdg-data").join("opencode");
        assert!(
            !data.join("snapshot").exists(),
            "stale snapshot store removed"
        );
        assert!(!data.join("opencode.db").exists(), "stale db removed");
        assert!(!attempt.join("tmp").join(".bun-lib.dylib").exists());
        assert!(
            attempt.join("xdg-data").is_dir(),
            "fresh runtime dirs created"
        );
        assert_eq!(preserved_logs(&attempt).len(), 1, "prior server log kept");
        drop(h);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drop_and_stop_remove_runtime_state_but_keep_the_log() {
        let (root, workspace) = layout("runtime-cleanup");
        for via_stop in [false, true] {
            let attempt = root.join(format!("attempt-{via_stop}"));
            let sandbox = RecordingSandbox::default();
            let mut h = harness(&root, &attempt, &sandbox);
            h.config.keep_runtime_state = false;
            start(&mut h, &workspace);
            seed_runtime(&attempt);
            if via_stop {
                h.stop_attempt(&AttemptHandle {
                    id: AttemptId::derive(1),
                })
                .expect("stop");
            }
            drop(h);
            for sub in RUNTIME_SUBDIRS {
                assert!(
                    !attempt.join(sub).exists(),
                    "{sub} survived cleanup (via_stop={via_stop})"
                );
            }
            assert_eq!(
                preserved_logs(&attempt).len(),
                1,
                "log kept (via_stop={via_stop})"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_never_touches_a_workspace_nested_in_a_runtime_dir() {
        // The live smoke layout nests the worktree at `<attempt>/work` (== HOME).
        let (root, _) = layout("nested-workspace");
        let attempt = root.join("attempt");
        let workspace = attempt.join("work");
        fs::create_dir_all(&workspace).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status()
            .expect("git init")
            .success());
        fs::write(workspace.join("src.rs"), "fn main() {}\n").unwrap();
        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &attempt, &sandbox);
        h.config.keep_runtime_state = false;
        start(&mut h, &workspace);
        seed_runtime(&attempt);
        drop(h);
        assert!(workspace.join(".git").is_dir(), "workspace repo survives");
        assert!(
            workspace.join("src.rs").is_file(),
            "workspace files survive"
        );
        assert!(
            !attempt.join("xdg-data").exists(),
            "other runtime state removed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn keep_flag_retains_runtime_state_for_forensics() {
        let (root, workspace) = layout("keep-runtime");
        let attempt = root.join("attempt");
        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &attempt, &sandbox);
        h.config.keep_runtime_state = true;
        start(&mut h, &workspace);
        seed_runtime(&attempt);
        drop(h);
        assert!(attempt.join("xdg-data/opencode/opencode.db").is_file());
        assert!(attempt.join("xdg-data/opencode/log/opencode.log").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repeated_cleanup_preserves_each_log() {
        let (root, workspace) = layout("repeated-cleanup");
        let attempt = root.join("attempt");
        for content in ["first", "second"] {
            seed_runtime(&attempt);
            fs::write(attempt.join("xdg-data/opencode/log/opencode.log"), content).unwrap();
            clear_runtime_state(&attempt, &workspace);
        }
        let mut contents: Vec<_> = preserved_logs(&attempt)
            .into_iter()
            .map(|p| fs::read_to_string(p.join("opencode.log")).unwrap())
            .collect();
        contents.sort();
        assert_eq!(contents, ["first", "second"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_unlinks_runtime_symlinks_without_moving_external_logs() {
        use std::os::unix::fs::symlink;
        let (root, workspace) = layout("symlink-cleanup");
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("opencode/log")).unwrap();
        let log = outside.join("opencode/log/opencode.log");
        fs::write(&log, "external log").unwrap();
        for (name, link_path, target) in [
            ("data", "xdg-data", outside.clone()),
            ("opencode", "xdg-data/opencode", outside.join("opencode")),
            ("log", "xdg-data/opencode/log", outside.join("opencode/log")),
            ("broken", "tmp", root.join("missing")),
        ] {
            let attempt = root.join(name);
            let link = attempt.join(link_path);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(target, &link).unwrap();
            clear_runtime_state(&attempt, &workspace);
            assert!(fs::symlink_metadata(&link).is_err());
            assert_eq!(fs::read_to_string(&log).unwrap(), "external log");
            assert!(preserved_logs(&attempt).is_empty());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drop_cleans_runtime_after_a_stopped_harness_is_restarted() {
        let (root, workspace) = layout("restart-cleanup");
        let attempt = root.join("attempt");
        let sandbox = RecordingSandbox::default();
        let mut h = harness(&root, &attempt, &sandbox);
        h.config.keep_runtime_state = false;
        start(&mut h, &workspace);
        h.stop_attempt(&AttemptHandle {
            id: AttemptId::derive(1),
        })
        .unwrap();
        start(&mut h, &workspace);
        seed_runtime(&attempt);
        drop(h);
        assert!(!attempt.join("xdg-data").exists());
        assert_eq!(preserved_logs(&attempt).len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn random_password_is_hex_and_unique() {
        let a = random_password();
        let b = random_password();
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn allocate_port_skips_a_transiently_occupied_port() {
        // Regression for agalma-9me: a sibling test can hold a just-released
        // ephemeral port, which used to surface as a spurious
        // "127.0.0.1:<port> already in use" start failure under parallel
        // workspace load (the never-ready test's message then lacked
        // "readiness"). The allocator must skip an occupied candidate instead
        // of failing, so the caller's readiness classification stays stable.
        let mut first: Option<u16> = None;
        let mut probes = 0;
        let port = allocate_port_with(|candidate| {
            probes += 1;
            if first.is_none() {
                first = Some(candidate);
                true // pretend a sibling grabbed the first candidate
            } else {
                false
            }
        })
        .expect("allocator must find a free port");
        assert_eq!(probes, 2, "occupied candidate is re-probed then skipped");
        // The returned port is the one the probe reported free (the second
        // candidate). Comparing port numbers to `first` would be flaky: the
        // kernel may hand back the same ephemeral port after release.
        assert!(port > 0, "a concrete port is returned");
    }

    /// Drive one launch through the adapter and capture the sandbox `LaunchSpec`
    /// it produced (the fake child is never ready, so `start_attempt` fails after
    /// recording the launch).
    fn launch_spec_of(config: OpenCodeHarnessConfig, workspace: &Path) -> LaunchSpec {
        let sandbox = RecordingSandbox::default();
        let mut harness = OpenCodeHarness::new(Box::new(sandbox.clone()), config);
        let err = start(&mut harness, workspace);
        assert!(
            err.to_string().contains("readiness"),
            "expected a recorded launch then a readiness failure: {err}"
        );
        drop(harness);
        sandbox
            .launches()
            .into_iter()
            .next()
            .expect("exactly one recorded launch")
    }

    /// Resolve `bin` on a `PATH` value, mimicking the conductor's `bd_bin_dir`.
    fn find_on_path(path: &str, bin: &str) -> Option<PathBuf> {
        std::env::split_paths(path)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    }

    /// agalma-tqf: the in-process and process transports must produce the same
    /// sandbox `LaunchSpec` for the same explicit launch config — the custom `bd`
    /// PATH addition, toolchain homes, `CARGO_HOME`, and read roots included — and
    /// a worker must be able to locate and execute `bd` and the toolchain from
    /// the transmitted PATH. Paths contain spaces to match real host layouts.
    #[test]
    fn process_transport_preserves_bd_toolchain_env_and_read_roots() {
        let (root, workspace) = layout("launch-parity");

        // A toolchain/bd bin directory whose name contains a space.
        let bin_dir = root.join("tool chain/bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        for (name, out) in [("bd", "bd-mock-ok"), ("cargo", "cargo-mock-ok")] {
            let path = bin_dir.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {out}\n")).expect("write mock");
            let mut perms = fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("chmod");
        }
        let cargo_home = root.join("cargo home");
        fs::create_dir_all(&cargo_home).expect("cargo home");

        let path_value = format!(
            "{}:/usr/bin:/bin:/opt/homebrew/bin",
            bin_dir.to_string_lossy()
        );
        let mut extra_env = BTreeMap::new();
        extra_env.insert("PATH".to_string(), path_value.clone());
        extra_env.insert(
            "CARGO_HOME".to_string(),
            cargo_home.to_string_lossy().into_owned(),
        );
        extra_env.insert(
            "RUSTUP_HOME".to_string(),
            root.join("rustup home").to_string_lossy().into_owned(),
        );

        let mut in_config = build_config(OpenCodeConfigInit {
            profile_path: root.join("worker.sb"),
            attempt_dir: root.join("attempt"),
            protected_dir: root.join("protected"),
            sock_dir: root.join("sock"),
            model: "opencode-go/deepseek".to_string(),
            proxy_url: Some("http://127.0.0.1:41234".to_string()),
            paid_proxy: true,
            extra_ro_roots: vec![bin_dir.clone()],
            extra_rw_roots: vec![workspace.clone()],
            extra_env,
        });
        in_config.program = Some(PathBuf::from("/usr/bin/true"));
        let in_spec = launch_spec_of(in_config.clone(), &workspace);

        // The conductor's process transport serializes exactly these fields; the
        // server reconstructs its config from them.
        let launch = LaunchConfig {
            profile_path: in_config.profile_path.clone(),
            attempt_dir: in_config.attempt_dir.clone(),
            protected_dir: in_config.protected_dir.clone(),
            sock_dir: in_config.sock_dir.clone(),
            model: in_config.model.clone().expect("model"),
            proxy_url: in_config.proxy_url.clone(),
            paid_proxy: in_config.provider_base_url.is_some(),
            program: in_config.program.clone(),
            extra_ro: in_config.extra_ro.clone(),
            extra_ro_roots: in_config.extra_ro_roots.clone(),
            extra_rw_roots: in_config.extra_rw_roots.clone(),
            extra_env: in_config.extra_env.clone(),
        };
        let lookup = |key: &str| -> Option<String> {
            match key {
                LAUNCH_CONFIG_ENV => Some(launch.to_json()),
                // Parent secrets must never be consulted for the child env.
                "AWS_SECRET_ACCESS_KEY" => Some("parent-secret".to_string()),
                _ => None,
            }
        };
        let proc_config = config_from_lookup(root.clone(), lookup);
        let proc_spec = launch_spec_of(proc_config, &workspace);

        // LaunchSpec parity (modulo the per-attempt random password).
        let mut in_env = in_spec.env.clone();
        let mut proc_env = proc_spec.env.clone();
        in_env.remove("OPENCODE_PASSWORD");
        proc_env.remove("OPENCODE_PASSWORD");
        assert_eq!(in_env, proc_env, "child environment parity");
        assert_eq!(
            in_spec.extra_ro_roots, proc_spec.extra_ro_roots,
            "read-root parity"
        );
        assert_eq!(
            in_spec.extra_rw_roots, proc_spec.extra_rw_roots,
            "read/write-root parity"
        );
        assert_eq!(in_spec.profile, proc_spec.profile, "profile parity");
        assert_eq!(
            in_spec.attempt_dir, proc_spec.attempt_dir,
            "attempt-root parity"
        );

        // The custom bd/toolchain PATH, CARGO_HOME, and read root survived.
        assert_eq!(proc_env["PATH"], path_value);
        assert!(proc_env["PATH"].contains(&bin_dir.to_string_lossy().to_string()));
        assert_eq!(proc_env["CARGO_HOME"], cargo_home.to_string_lossy());
        assert!(
            proc_spec
                .extra_ro_roots
                .iter()
                .any(|r| r == &bin_dir.to_string_lossy().to_string()),
            "bd root must travel as a read-only root: {:?}",
            proc_spec.extra_ro_roots
        );
        assert!(!proc_env.contains_key("AWS_SECRET_ACCESS_KEY"));

        // A sandboxed worker can locate and execute bd and the toolchain.
        let bd = find_on_path(&proc_env["PATH"], "bd").expect("bd on transmitted PATH");
        assert_eq!(
            String::from_utf8_lossy(&Command::new(&bd).output().expect("run bd").stdout).trim(),
            "bd-mock-ok"
        );
        let cargo = find_on_path(&proc_env["PATH"], "cargo").expect("cargo on transmitted PATH");
        assert_eq!(
            String::from_utf8_lossy(&Command::new(&cargo).output().expect("run cargo").stdout)
                .trim(),
            "cargo-mock-ok"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
