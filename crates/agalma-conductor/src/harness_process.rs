//! Process-bound harness supervisor (B1.2, decisions D2/D3).
//!
//! [`ProcessBinding`] owns a trusted component-server child process: it spawns
//! the binary with `std::process::Command` (D2: trusted servers are supervised,
//! not sandboxed), waits for the `READY {json}` handshake with a timeout, runs
//! the existing bind gate ([`plan_from_describe`]) against the announced
//! identity, and connects a [`HarnessClient`]. `stop()` sends `SIGTERM`, waits,
//! and records pid-gone evidence; `restart()` respawns with the same binding
//! id/generation (an in-flight operation is never rebound).
//!
//! Transport selection is [`HarnessTransport`] (`AGALMA_HARNESS_TRANSPORT`):
//! `in-process` (default) or `process`. The build/verify phase logic is
//! unchanged (D5); the transport is exercised through the factory here.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use agalma_contracts::harness::{
    AttemptHandle, CancelAck, CreateSessionRequest, Describe, Event, HarnessApi, OperationHandle,
    OperationState, RunTurnRequest, SessionHandle, StartAttemptRequest, StopEvidence,
};
use agalma_contracts::{BindingRecord, ContractError};
use agalma_harness_opencode::{self as opencode, OpenCodeHarness, OpenCodeHarnessConfig};
use agalma_harness_reference::{self as reference, ReferenceProfile};
use agalma_ipc::{CommandSpec, HarnessClient, SandboxClient, ServerInfo};

use crate::harness_binding::{self, HarnessKind};

/// Environment variable selecting the harness transport.
pub const TRANSPORT_ENV: &str = "AGALMA_HARNESS_TRANSPORT";
/// Default handshake timeout for a component server.
pub const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Grace between `SIGTERM` and `SIGKILL` when stopping a server.
const TERM_GRACE: Duration = Duration::from_millis(2000);

/// How the conductor reaches a harness implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessTransport {
    /// Direct in-process trait call (unchanged phase logic).
    InProcess,
    /// Supervised component server over the framed transport.
    Process,
}

impl HarnessTransport {
    /// Read the transport from [`TRANSPORT_ENV`]; unknown/empty is `InProcess`.
    pub fn from_env() -> Self {
        match std::env::var(TRANSPORT_ENV).ok().as_deref() {
            Some("process") => HarnessTransport::Process,
            _ => HarnessTransport::InProcess,
        }
    }

    /// Stable selector string.
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessTransport::InProcess => "in-process",
            HarnessTransport::Process => "process",
        }
    }
}

/// Resolve a component server binary name relative to the conductor executable.
///
/// Handles both `target/debug/agalma` (sibling) and `target/debug/deps/<test>`
/// (walk up to the target dir), then falls back to `PATH`.
pub fn resolve_server_binary(program: &str) -> Result<PathBuf, String> {
    let direct = Path::new(program);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut dir = exe.parent();
    for _ in 0..4 {
        let Some(base) = dir else { break };
        let candidate = base.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = base.parent();
    }
    if let Some(path) = std::env::var_os("PATH") {
        for base in std::env::split_paths(&path) {
            let candidate = base.join(program);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "component server `{program}` not found relative to {}",
        exe.display()
    ))
}

/// Launch spec for the reference server bound to `profile`, listening on
/// `socket` (the caller supplies the `--socket` argument).
pub fn reference_server_spec(profile: ReferenceProfile, socket: &Path) -> CommandSpec {
    reference::server_command(profile)
        .arg("--socket")
        .arg(socket.to_string_lossy().into_owned())
}

/// A supervised harness component-server process.
pub struct ProcessBinding {
    socket: PathBuf,
    spec: CommandSpec,
    program: PathBuf,
    child: Child,
    info: ServerInfo,
    describe: Describe,
    binding: BindingRecord,
    client: HarnessClient,
    generation: u32,
    model: String,
    stopped: bool,
}

impl ProcessBinding {
    /// Spawn `program` from `spec` (which must include `--socket <socket>`),
    /// wait for `READY`, run the bind gate, and connect.
    pub fn spawn(
        spec: CommandSpec,
        program: PathBuf,
        socket: PathBuf,
        generation: u32,
        model: &str,
        timeout: Duration,
    ) -> Result<ProcessBinding, String> {
        // The cleanup guard owns the freshly spawned group for every fallible
        // startup stage (READY wait is inside `spawn_supervised`; bind gate and
        // client connect follow here). Ownership transfers to the binding only
        // once construction is complete, so no error path leaks the server tree.
        let (guard, info) = spawn_supervised(&spec, &program, &socket, timeout)?;

        let describe = describe_from_info(&info);
        let plan = harness_binding::plan_from_describe(
            &describe,
            generation,
            model,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .map_err(|e| format!("bind gate rejected {}: {e}", info.impl_name))?;

        let client = HarnessClient::connect(&socket).map_err(|e| e.to_string())?;

        let child = guard.into_child();

        Ok(ProcessBinding {
            socket,
            spec,
            program,
            child,
            info,
            describe,
            binding: plan.binding,
            client,
            generation,
            model: model.to_string(),
            stopped: false,
        })
    }

    /// The announced server identity.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// The bound record produced by the bind gate.
    pub fn binding(&self) -> &BindingRecord {
        &self.binding
    }

    /// The announced identity as a `Describe`.
    pub fn describe(&self) -> &Describe {
        &self.describe
    }

    /// The server child pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The connected harness client.
    pub fn client(&mut self) -> &mut HarnessClient {
        &mut self.client
    }

    /// Stop the server: `SIGTERM`, wait for exit, verify the pid is gone, and
    /// return cleanup evidence. Idempotent.
    pub fn stop(&mut self) -> StopEvidence {
        if self.stopped {
            return StopEvidence {
                process_group_gone: true,
                detail: "server already stopped".to_string(),
            };
        }
        let evidence = stop_supervised(&mut self.child);
        self.stopped = true;
        evidence
    }

    /// Restart the server in place, keeping the same binding id/generation.
    ///
    /// The bind gate is re-run against the same announced identity; an in-flight
    /// operation is never rebound (its pinned record is unchanged).
    pub fn restart(&mut self) -> Result<(), String> {
        let binding_id = self.binding.binding_id.clone();
        let generation = self.binding.generation;
        let _ = self.stop();
        let mut restarted = ProcessBinding::spawn(
            self.spec.clone(),
            self.program.clone(),
            self.socket.clone(),
            self.generation,
            &self.model,
            READY_TIMEOUT,
        )?;
        if restarted.binding.binding_id != binding_id || restarted.binding.generation != generation
        {
            let _ = restarted.stop();
            return Err(format!(
                "restart changed the binding id/generation: {} != {}",
                restarted.binding.binding_id, binding_id
            ));
        }
        restarted.stopped = false;
        *self = restarted;
        Ok(())
    }
}

impl Drop for ProcessBinding {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl HarnessApi for ProcessBinding {
    fn describe(&self) -> Describe {
        self.client.describe()
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, ContractError> {
        self.client.start_attempt(req)
    }

    fn create_session(
        &mut self,
        req: CreateSessionRequest,
    ) -> Result<SessionHandle, ContractError> {
        self.client.create_session(req)
    }

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError> {
        self.client.run_turn(session, req)
    }

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, ContractError> {
        self.client.inspect_operation(op)
    }

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
        self.client.read_events(op)
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, ContractError> {
        self.client.cancel_operation(op)
    }

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), ContractError> {
        self.client.close_session(session)
    }

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, ContractError> {
        self.client.stop_attempt(attempt)
    }
}

/// Open a reference harness under `transport` for a scenario.
///
/// `InProcess` builds a [`reference::ReferenceHarness`]; `Process` spawns the
/// reference component server (resolving its binary relative to this executable)
/// and returns the supervised [`ProcessBinding`].
pub fn open_reference_harness(
    kind: HarnessKind,
    transport: HarnessTransport,
    scenario_dir: PathBuf,
    socket: PathBuf,
    generation: u32,
    model: &str,
) -> Result<Box<dyn HarnessApi>, String> {
    let profile = reference_profile(kind);
    match transport {
        HarnessTransport::InProcess => Ok(Box::new(reference::ReferenceHarness::new(
            reference::ReferenceHarnessConfig {
                profile,
                scenario_dir,
                scenario: None,
            },
        ))),
        HarnessTransport::Process => {
            let spec = reference_server_spec(profile, &socket).env(
                reference::SCENARIO_DIR_ENV,
                scenario_dir.to_string_lossy().into_owned(),
            );
            let program = resolve_server_binary(&spec.program)?;
            let binding =
                ProcessBinding::spawn(spec, program, socket, generation, model, READY_TIMEOUT)?;
            Ok(Box::new(binding))
        }
    }
}

/// Map a binding kind to a reference profile.
pub fn reference_profile(kind: HarnessKind) -> ReferenceProfile {
    match kind {
        HarnessKind::ReferenceFail => ReferenceProfile::Red,
        _ => ReferenceProfile::Success,
    }
}

/// Inputs for [`open_opencode_harness`]: the attempt-scoped OpenCode config plus
/// the transport and boundary socket paths.
pub struct OpenCodeHarnessInit {
    /// Transport selected by `AGALMA_HARNESS_TRANSPORT` (default in-process).
    pub transport: HarnessTransport,
    /// Attempt-scoped OpenCode harness config (in-process branch).
    pub config: OpenCodeHarnessConfig,
    /// Attempt root handed to the OpenCode component server (`ROOT_ENV`).
    pub root: PathBuf,
    /// Resolved model (binding + server env).
    pub model: String,
    /// Binding generation for the process bind gate.
    pub generation: u32,
    /// Sandbox component server socket (client target or server env).
    pub sandbox_socket: PathBuf,
    /// Socket the OpenCode component server should bind (process branch).
    pub harness_socket: PathBuf,
}

/// A live build/verify harness bound either in-process or across the process
/// boundary. The phase path stays unchanged because both variants expose the
/// same `HarnessApi` surface plus [`LiveHarness::attempt_pid`].
pub enum LiveHarness {
    /// Direct in-process trait calls (default; unchanged phase logic).
    InProcess(OpenCodeHarness),
    /// Supervised OpenCode component server over the framed transport.
    Process(ProcessBinding),
}

impl LiveHarness {
    /// The confined server pid, once an attempt is active (evidence for orphan
    /// checks). For the process variant this is the harness-server pid.
    pub fn attempt_pid(&self) -> Option<u32> {
        match self {
            LiveHarness::InProcess(harness) => harness.attempt_pid(),
            LiveHarness::Process(binding) => Some(binding.pid()),
        }
    }
}

impl HarnessApi for LiveHarness {
    fn describe(&self) -> Describe {
        match self {
            LiveHarness::InProcess(harness) => harness.describe(),
            LiveHarness::Process(binding) => binding.describe().clone(),
        }
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.start_attempt(req),
            LiveHarness::Process(binding) => binding.start_attempt(req),
        }
    }

    fn create_session(
        &mut self,
        req: CreateSessionRequest,
    ) -> Result<SessionHandle, ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.create_session(req),
            LiveHarness::Process(binding) => binding.create_session(req),
        }
    }

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.run_turn(session, req),
            LiveHarness::Process(binding) => binding.run_turn(session, req),
        }
    }

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.inspect_operation(op),
            LiveHarness::Process(binding) => binding.inspect_operation(op),
        }
    }

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.read_events(op),
            LiveHarness::Process(binding) => binding.read_events(op),
        }
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.cancel_operation(op),
            LiveHarness::Process(binding) => binding.cancel_operation(op),
        }
    }

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.close_session(session),
            LiveHarness::Process(binding) => binding.close_session(session),
        }
    }

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, ContractError> {
        match self {
            LiveHarness::InProcess(harness) => harness.stop_attempt(attempt),
            LiveHarness::Process(binding) => binding.stop_attempt(attempt),
        }
    }
}

/// Build the live OpenCode harness under `transport`.
///
/// `InProcess` constructs an [`OpenCodeHarness`] whose confined launch goes
/// through a [`SandboxClient`] connected to `sandbox_socket`. `Process` spawns
/// the `agalma-harness-opencode-server` component server (which itself uses the
/// sandbox client via `AGALMA_SANDBOX_SOCKET`) and returns the supervised
/// [`ProcessBinding`]. The process branch is not exercised against a live
/// OpenCode install offline; the reference transport is proven by the boundary
/// tests.
pub fn open_opencode_harness(init: OpenCodeHarnessInit) -> Result<LiveHarness, String> {
    match init.transport {
        HarnessTransport::InProcess => {
            let sandbox =
                SandboxClient::connect(&init.sandbox_socket).map_err(|e| e.to_string())?;
            Ok(LiveHarness::InProcess(OpenCodeHarness::new(
                Box::new(sandbox),
                init.config,
            )))
        }
        HarnessTransport::Process => {
            let spec = opencode_server_spec(&init);
            let program = resolve_server_binary(&spec.program)?;
            let binding = ProcessBinding::spawn(
                spec,
                program,
                init.harness_socket,
                init.generation,
                &init.model,
                READY_TIMEOUT,
            )?;
            Ok(LiveHarness::Process(binding))
        }
    }
}

/// Build the process-bound launch spec for one OpenCode harness attempt.
///
/// Beyond the socket, attempt root, and sandbox socket, the supervised server is
/// spawned with the full explicit launch configuration
/// ([`opencode::LaunchConfig`]) in [`opencode::LAUNCH_CONFIG_ENV`], so the server
/// reconstructs the same sandbox `LaunchSpec` the in-process transport would use:
/// the extra child environment (toolchain homes, PATH additions, `CARGO_HOME`)
/// and the read-only/read-write roots included. Only this explicit,
/// conductor-built configuration crosses the boundary; the parent environment and
/// real provider credentials are never forwarded. Split out so the transmitted
/// config can be asserted without spawning a server.
pub fn opencode_server_spec(init: &OpenCodeHarnessInit) -> CommandSpec {
    let launch = opencode::LaunchConfig {
        profile_path: init.config.profile_path.clone(),
        attempt_dir: init.config.attempt_dir.clone(),
        protected_dir: init.config.protected_dir.clone(),
        sock_dir: init.config.sock_dir.clone(),
        model: init
            .config
            .model
            .clone()
            .unwrap_or_else(|| init.model.clone()),
        proxy_url: init.config.proxy_url.clone(),
        paid_proxy: init.config.provider_base_url.is_some(),
        program: init.config.program.clone(),
        extra_ro: init.config.extra_ro.clone(),
        extra_ro_roots: init.config.extra_ro_roots.clone(),
        extra_rw_roots: init.config.extra_rw_roots.clone(),
        extra_env: init.config.extra_env.clone(),
    };
    opencode::server_command()
        .arg("--socket")
        .arg(init.harness_socket.to_string_lossy().into_owned())
        .env(opencode::ROOT_ENV, init.root.to_string_lossy().into_owned())
        .env(
            opencode::SANDBOX_SOCKET_ENV,
            init.sandbox_socket.to_string_lossy().into_owned(),
        )
        .env(opencode::LAUNCH_CONFIG_ENV, launch.to_json())
}

fn describe_from_info(info: &ServerInfo) -> Describe {
    Describe {
        api: info.api.clone(),
        api_version: info.api_version,
        impl_name: info.impl_name.clone(),
        impl_version: info.impl_version.clone(),
        capabilities: info.capabilities.clone(),
    }
}

/// Read lines from `reader` until one carries the `READY {json}` handshake,
/// within `timeout`.
///
/// The marker is matched anywhere in a line, not only at the start: a re-exec'd
/// test-binary server prefixes the line with libtest's `test <name> ... `.
fn read_ready(
    reader: BufReader<std::process::ChildStdout>,
    timeout: Duration,
) -> Result<ServerInfo, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(json) = ready_json(&line) {
                        let _ = tx.send(json.to_string());
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    match rx.recv_timeout(timeout) {
        Ok(json) => {
            serde_json::from_str(&json).map_err(|e| format!("malformed READY line: {e} ({json})"))
        }
        Err(_) => Err(format!("server did not announce READY within {timeout:?}")),
    }
}

/// Extract the JSON after the first `READY ` marker in `line`.
pub fn ready_json(line: &str) -> Option<&str> {
    let marker = "READY ";
    let index = line.find(marker)?;
    Some(line[index + marker.len()..].trim_end())
}

/// Signal a process group (`-pgid`). Tolerates the macOS `EPERM` on group
/// signals; the leader reap and pid-gone check are the real evidence.
fn signal_group(pgid: u32, sig: &str) {
    let _ = Command::new("/bin/kill")
        .arg(format!("-{sig}"))
        .arg(format!("-{pgid}"))
        .status();
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Spawn a supervised component-server child (shared by every boundary).
///
/// Creates the socket's parent directory, drops any stale socket, spawns
/// `program` from `spec` as a process-group leader (so `stop_supervised` can
/// signal the whole tree), and waits for the `READY {json}` handshake.
///
/// The returned [`SupervisedChild`] owns cleanup: if any startup stage up to
/// and including the READY wait fails, its `Drop` terminates and reaps the
/// process group before the error propagates, so a failed start never leaks the
/// server or its descendants. The caller relinquishes ownership with
/// [`SupervisedChild::into_child`] only after the component is fully built.
pub(crate) fn spawn_supervised(
    spec: &CommandSpec,
    program: &Path,
    socket: &Path,
    timeout: Duration,
) -> Result<(SupervisedChild, ServerInfo), String> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create socket dir {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(socket);

    let mut command = Command::new(program);
    command
        .args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", program.display()))?;

    // Retain cleanup ownership from the instant of successful spawn: every
    // fallible stage below (stdout capture, READY wait) drops this guard on
    // error and tears the group down.
    let mut guard = SupervisedChild::new(child);

    let stdout = guard
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| "server stdout not captured".to_string())?;
    let reader = BufReader::new(stdout);
    let info = read_ready(reader, timeout)?;
    Ok((guard, info))
}

/// Cleanup-owning handle for a supervised child during startup.
///
/// `std::process::Child` does not terminate the process on drop, so an error
/// returned between spawn and full construction would otherwise leak the server
/// and its descendants. The guard closes that gap: on `Drop` (i.e. any early
/// return) it runs [`stop_supervised`] — `SIGTERM` the process group, escalate
/// to `SIGKILL` if needed, reap the leader — while never touching process
/// groups or sockets it does not own (the group is the one created at spawn).
pub(crate) struct SupervisedChild {
    child: Option<Child>,
}

impl SupervisedChild {
    fn new(child: Child) -> Self {
        SupervisedChild { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("supervised child already relinquished")
    }

    /// Relinquish cleanup ownership once the component is fully constructed,
    /// handing the child to its permanent owner.
    pub(crate) fn into_child(mut self) -> Child {
        self.child
            .take()
            .expect("supervised child already relinquished")
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = stop_supervised(child);
        }
    }
}

/// Stop a supervised child: `SIGTERM` the process group, wait, escalate to
/// `SIGKILL`, and report pid-gone evidence (shared by every boundary).
pub(crate) fn stop_supervised(child: &mut Child) -> StopEvidence {
    let pid = child.id();
    let mut actions = Vec::new();
    // Signal the process group (pgid == pid, set at spawn) so descendants are
    // torn down with the leader.
    signal_group(pid, "TERM");
    actions.push(format!("SIGTERM -> pgid {pid}"));

    let deadline = std::time::Instant::now() + TERM_GRACE;
    let mut exited = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !exited {
        signal_group(pid, "KILL");
        let _ = child.kill();
        let _ = child.wait();
        actions.push("escalated to SIGKILL".to_string());
    }
    let gone = !pid_alive(pid);
    actions.push(format!("pid {pid} gone={gone}"));
    StopEvidence {
        process_group_gone: gone,
        detail: actions.join("; "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn transport_from_env_defaults_to_in_process() {
        // Absent -> InProcess; explicit `process` -> Process.
        std::env::remove_var(TRANSPORT_ENV);
        assert_eq!(HarnessTransport::from_env(), HarnessTransport::InProcess);
    }

    #[test]
    fn reference_spec_includes_socket_argument() {
        let spec = reference_server_spec(ReferenceProfile::Success, Path::new("/run/h.sock"));
        assert_eq!(spec.program, reference::SERVER_BIN);
        assert_eq!(spec.args, vec!["--socket", "/run/h.sock"]);
    }

    #[test]
    fn resolve_server_binary_finds_sibling_of_current_exe() {
        // The test binary lives under target/debug/deps; the workspace bins are
        // siblings two directories up. `agalma` is always built for the
        // conductor tests, so it must resolve.
        let resolved = resolve_server_binary("agalma").expect("agalma binary resolves");
        assert!(resolved.is_file(), "{}", resolved.display());
    }

    /// The process branch must transmit the explicit launch config — the extra
    /// child environment (bd/toolchain PATH, `CARGO_HOME`) and the read-only /
    /// read-write roots — not just root/profile/model (`agalma-tqf`).
    #[test]
    fn opencode_server_spec_forwards_the_explicit_launch_config() {
        let mut config = OpenCodeHarnessConfig::new(
            "/state/run/worker.sb",
            "/state/run",
            "/state/run/protected",
            "/state/run/sock",
        );
        config.extra_env = BTreeMap::from([
            (
                "PATH".to_string(),
                "/Users/a/.local/bin:/cargo home/bin:/usr/bin:/bin".to_string(),
            ),
            (
                "CARGO_HOME".to_string(),
                "/state/run/cargo-home".to_string(),
            ),
            ("RUSTUP_HOME".to_string(), "/Users/a/.rustup".to_string()),
        ]);
        config.extra_ro_roots = vec![
            PathBuf::from("/Users/a/.local/bin"),
            PathBuf::from("/read only/tool chain"),
        ];
        config.extra_rw_roots = vec![PathBuf::from("/repo with spaces")];
        config.proxy_url = Some("http://127.0.0.1:41234".to_string());
        config.provider_base_url = Some("http://127.0.0.1:41234".to_string());
        config.model = Some("opencode-go/deepseek".to_string());

        let init = OpenCodeHarnessInit {
            transport: HarnessTransport::Process,
            config,
            root: PathBuf::from("/state/run"),
            model: "opencode-go/deepseek".to_string(),
            generation: 1,
            sandbox_socket: PathBuf::from("/state/run/sandbox.sock"),
            harness_socket: PathBuf::from("/state/run/harness.sock"),
        };
        let spec = opencode_server_spec(&init);

        // The server still needs the argv socket and root/sandbox env.
        assert_eq!(spec.program, opencode::SERVER_BIN);
        assert_eq!(spec.args, vec!["--socket", "/state/run/harness.sock"]);
        assert_eq!(
            spec.env.get(opencode::ROOT_ENV).map(String::as_str),
            Some("/state/run")
        );
        assert_eq!(
            spec.env
                .get(opencode::SANDBOX_SOCKET_ENV)
                .map(String::as_str),
            Some("/state/run/sandbox.sock")
        );

        // The full launch config travels in one structured variable.
        let json = spec
            .env
            .get(opencode::LAUNCH_CONFIG_ENV)
            .expect("launch config env present");
        let transferred =
            opencode::LaunchConfig::from_json(json).expect("launch config json decodes");
        assert_eq!(transferred.profile_path, init.config.profile_path);
        assert_eq!(transferred.attempt_dir, init.config.attempt_dir);
        assert_eq!(transferred.protected_dir, init.config.protected_dir);
        assert_eq!(transferred.sock_dir, init.config.sock_dir);
        assert_eq!(transferred.model, "opencode-go/deepseek");
        assert_eq!(transferred.proxy_url, init.config.proxy_url);
        assert!(transferred.paid_proxy, "provider_base_url set => paid path");
        assert_eq!(transferred.program, init.config.program);
        assert_eq!(transferred.extra_ro_roots, init.config.extra_ro_roots);
        assert_eq!(transferred.extra_rw_roots, init.config.extra_rw_roots);
        assert_eq!(transferred.extra_env, init.config.extra_env);

        // The parent environment is never forwarded wholesale.
        for leaked in ["AWS_SECRET_ACCESS_KEY", "OPENCODE_API_KEY", "HOME", "USER"] {
            assert!(
                !spec.env.contains_key(leaked),
                "parent env `{leaked}` must not be forwarded"
            );
        }
    }

    /// The build path's in-process `attempt_dir` is the run root itself; the
    /// transmitted config must preserve it instead of re-deriving `<root>/attempt`.
    #[test]
    fn opencode_server_spec_preserves_attempt_dir_not_re_derived() {
        let attempt_dir = PathBuf::from("/state/run/build-attempt");
        let mut config = OpenCodeHarnessConfig::new(
            "/state/run/worker.sb",
            &attempt_dir,
            "/state/run/protected",
            "/state/run/sock",
        );
        config.model = Some("opencode/mimo".to_string());
        let init = OpenCodeHarnessInit {
            transport: HarnessTransport::Process,
            config,
            root: PathBuf::from("/state/run"),
            model: "opencode/mimo".to_string(),
            generation: 1,
            sandbox_socket: PathBuf::from("/state/run/sandbox.sock"),
            harness_socket: PathBuf::from("/state/run/harness.sock"),
        };
        let spec = opencode_server_spec(&init);
        let json = spec.env.get(opencode::LAUNCH_CONFIG_ENV).expect("config");
        let transferred = opencode::LaunchConfig::from_json(json).expect("decodes");
        assert_eq!(transferred.attempt_dir, attempt_dir);
        assert_ne!(transferred.attempt_dir, PathBuf::from("/state/run/attempt"));
    }
}
