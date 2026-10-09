//! OpenCode `HarnessApi` adapter.
//!
//! Every vendor detail lives here and nowhere else: the `opencode serve`
//! invocation, the isolated XDG/`TMPDIR` environment, Basic auth, the
//! hand-rolled HTTP client, endpoint paths, native `ses_*` identifiers, and the
//! free-model prompt text. The scenario driver and the contract never see any of
//! it.
//!
//! Transport choice: `std::net::TcpStream` with hand-rolled HTTP/1.1 (no
//! crates, no external `curl` process). Every call the adapter makes is a
//! request/response pair; the only streaming surface used is the durable
//! session log read *without* `follow`, which the server closes after the sync
//! marker. SSE is not parsed; reconciliation is done through the session
//! projection (`GET /api/session/{id}`) plus the durable log marker. See the
//! Results section of `docs/spikes/s0d-seam-freeze.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::contract::*;
use crate::json::Json;

const IMPL_NAME: &str = "opencode";
const DEFAULT_MODEL: &str = "mimo-v2.6-flash-free";
const DEFAULT_PROVIDER: &str = "opencode";
const DEFAULT_PORT: u16 = 39003;
const AUTH_USER: &str = "opencode";

fn bin() -> String {
    std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string())
}

fn detect_version() -> String {
    match Command::new(bin()).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.split_whitespace()
                .last()
                .unwrap_or("unknown")
                .trim()
                .to_string()
        }
        _ => "unknown".to_string(),
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn b64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subsequence(data, b"\r\n").ok_or("chunk size line missing")?;
        let size_line = std::str::from_utf8(&data[..line_end]).map_err(|_| "non-utf8 chunk size")?;
        let size_token = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_token, 16).map_err(|_| format!("bad chunk size {size_token:?}"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            out.extend_from_slice(data);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size.min(data.len())..];
        if data.len() >= 2 {
            data = &data[2..];
        } else {
            data = &[];
        }
    }
    Ok(out)
}

fn parse_response(raw: &[u8]) -> Result<(u16, String), String> {
    let sep = find_subsequence(raw, b"\r\n\r\n").ok_or("response has no header terminator")?;
    let head = std::str::from_utf8(&raw[..sep]).map_err(|_| "non-utf8 response head")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line {status_line:?}"))?;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("transfer-encoding:") {
            if value.contains("chunked") {
                chunked = true;
            }
        }
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let body_bytes = &raw[sep + 4..];
    let body = if chunked {
        dechunk(body_bytes)?
    } else if let Some(n) = content_length {
        body_bytes[..body_bytes.len().min(n)].to_vec()
    } else {
        body_bytes.to_vec()
    };
    Ok((code, String::from_utf8_lossy(&body).into_owned()))
}

struct HttpClient {
    host: String,
    port: u16,
    auth: String,
}

impl HttpClient {
    fn new(host: &str, port: u16, user: &str, password: &str) -> Self {
        let token = b64(format!("{user}:{password}").as_bytes());
        HttpClient { host: host.to_string(), port, auth: token }
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String), String> {
        let addr = format!("{}:{}", self.host, self.port);
        let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;

        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Basic {}\r\nAccept: application/json\r\nConnection: close\r\n",
            self.host, self.port, self.auth
        );
        if let Some(b) = body {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        if let Some(b) = body {
            req.push_str(b);
        }
        stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
        parse_response(&raw)
    }
}

fn group_alive(pgid: i32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(format!("-{pgid}"))
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pid_alive(pid: i32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn signal_group(pgid: i32, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg(format!("-{pgid}"))
        .stderr(Stdio::null())
        .status();
}

fn signal_pid(pid: i32, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status();
}

/// All descendants of `root` (depth-first via `pgrep -P`). Direct pid signals
/// are used alongside the process-group signal because macOS can reject a
/// group signal while the individual children remain signalable.
fn descendants(root: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(parent) = stack.pop() {
        if let Ok(output) = Command::new("pgrep").arg("-P").arg(parent.to_string()).output() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if let Ok(child) = line.trim().parse::<i32>() {
                    if !out.contains(&child) {
                        out.push(child);
                        stack.push(child);
                    }
                }
            }
        }
    }
    out
}

/// Terminate the attempt process tree: best-effort group signal, then explicit
/// TERM to every descendant, reap the direct child, wait for the group to
/// drain, and escalate to KILL. Returns whether the root and its group are gone.
fn terminate_tree(root: i32, child: &mut Option<Child>) -> bool {
    let targets = collect_targets(root);
    signal_targets(root, &targets, "-TERM");
    // Reap the direct child first: until it is waited for it lingers as a
    // zombie and `kill -0` reports it as alive.
    if let Some(mut c) = child.take() {
        let _ = c.wait();
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && (pid_alive(root) || group_alive(root)) {
        sleep(Duration::from_millis(100));
    }
    if pid_alive(root) || group_alive(root) {
        signal_targets(root, &targets, "-KILL");
        sleep(Duration::from_millis(300));
    }
    !pid_alive(root) && !group_alive(root)
}

fn collect_targets(root: i32) -> Vec<i32> {
    let mut targets = descendants(root);
    targets.push(root);
    targets
}

fn signal_targets(root: i32, targets: &[i32], signal: &str) {
    signal_group(root, signal);
    for pid in targets {
        signal_pid(*pid, signal);
    }
}

fn prompt_for(input_ref: &str) -> &'static str {
    match input_ref {
        "artifact:turn-2-input" => {
            "Count slowly from 1 to 1000, one number per line. Do not use any tools. Do not stop before 1000."
        }
        _ => "Reply with exactly: S0D_OK. Do not use any tools.",
    }
}

struct OcOp {
    native_session: String,
}

/// OpenCode-backed `HarnessAdapter`.
pub struct OpenCodeAdapter {
    run_dir: PathBuf,
    workspace: String,
    host: String,
    port: u16,
    password: String,
    model: String,
    provider: String,
    impl_version: String,
    child: Option<Child>,
    pgid: Option<i32>,
    http: Option<HttpClient>,
    attempt: Option<AttemptHandle>,
    sessions: Vec<SessionHandle>,
    session_native: BTreeMap<String, String>,
    ops: BTreeMap<String, OcOp>,
    session_seq: u32,
    turn_seq: u32,
    stopped: bool,
}

impl OpenCodeAdapter {
    pub fn new(run_dir: &str, workspace: &str) -> Self {
        let model = std::env::var("S0D_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let provider = std::env::var("S0D_PROVIDER").unwrap_or_else(|_| DEFAULT_PROVIDER.to_string());
        let port = std::env::var("S0D_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let password = std::env::var("OPENCODE_PASSWORD").unwrap_or_else(|_| "s0d-spike-password".to_string());
        OpenCodeAdapter {
            run_dir: PathBuf::from(run_dir),
            workspace: workspace.to_string(),
            host: "127.0.0.1".to_string(),
            port,
            password,
            model,
            provider,
            impl_version: detect_version(),
            child: None,
            pgid: None,
            http: None,
            attempt: None,
            sessions: Vec::new(),
            session_native: BTreeMap::new(),
            ops: BTreeMap::new(),
            session_seq: 0,
            turn_seq: 0,
            stopped: false,
        }
    }

    fn spawn_server(&mut self) -> Result<(), HarnessError> {
        // Refuse to run if something already owns the port: connecting to a
        // stale server would silently break isolation.
        if TcpStream::connect(format!("{}:{}", self.host, self.port)).is_ok() {
            return Err(HarnessError::KnownFailure(format!(
                "port {}:{} already in use; refusing to bind to a foreign server",
                self.host, self.port
            )));
        }
        for sub in ["xcfg", "xdata", "xcache", "xstate", "tmp"] {
            let dir = self.run_dir.join(sub);
            fs::create_dir_all(&dir).map_err(|e| HarnessError::KnownFailure(format!("mkdir {}: {e}", dir.display())))?;
        }
        let out_path = self.run_dir.join("server.out");
        let err_path = self.run_dir.join("server.err");
        let out = File::create(&out_path).map_err(|e| HarnessError::KnownFailure(format!("server.out: {e}")))?;
        let err = File::create(&err_path).map_err(|e| HarnessError::KnownFailure(format!("server.err: {e}")))?;

        let mut cmd = Command::new(bin());
        cmd.arg("serve")
            .arg("--hostname")
            .arg(&self.host)
            .arg("--port")
            .arg(self.port.to_string())
            .current_dir(&self.workspace)
            .env("XDG_CONFIG_HOME", self.run_dir.join("xcfg"))
            .env("XDG_DATA_HOME", self.run_dir.join("xdata"))
            .env("XDG_CACHE_HOME", self.run_dir.join("xcache"))
            .env("XDG_STATE_HOME", self.run_dir.join("xstate"))
            .env("TMPDIR", self.run_dir.join("tmp"))
            .env("OPENCODE_PASSWORD", &self.password)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        // New process group so StopAttempt can signal the whole tree (S0b).
        cmd.process_group(0);

        let child = cmd
            .spawn()
            .map_err(|e| HarnessError::KnownFailure(format!("spawn {}: {e}", bin())))?;
        let pgid = child.id() as i32;
        self.pgid = Some(pgid);
        self.child = Some(child);
        self.http = Some(HttpClient::new(&self.host, self.port, AUTH_USER, &self.password));
        self.wait_ready()?;
        Ok(())
    }

    fn wait_ready(&mut self) -> Result<(), HarnessError> {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(HarnessError::KnownFailure(format!(
                        "opencode serve exited early: {status}"
                    )));
                }
            }
            let ready = self
                .http
                .as_ref()
                .map(|http| matches!(http.request("GET", "/api/info", None), Ok((200, _))))
                .unwrap_or(false);
            if ready {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(HarnessError::KnownFailure("opencode serve not ready in 25s".to_string()));
            }
            sleep(Duration::from_millis(200));
        }
    }

    fn http(&self) -> Result<&HttpClient, HarnessError> {
        self.http
            .as_ref()
            .ok_or_else(|| HarnessError::KnownFailure("adapter has no HTTP client".to_string()))
    }

    fn native_for(&self, op: &OperationHandle) -> Result<String, HarnessError> {
        self.ops
            .get(&op.id)
            .map(|o| o.native_session.clone())
            .ok_or_else(|| HarnessError::KnownFailure(format!("unknown operation {}", op.id)))
    }

    fn session_json(&self, native: &str) -> Result<Json, HarnessError> {
        let path = format!("/api/session/{native}");
        let (code, body) = self
            .http()?
            .request("GET", &path, None)
            .map_err(|e| HarnessError::UnknownOutcome(e))?;
        if code != 200 {
            return Err(HarnessError::UnknownOutcome(format!("GET {path} -> {code}")));
        }
        Json::parse(&body).map_err(|e| HarnessError::UnknownOutcome(format!("session json: {e}")))
    }

    fn usage_from(&self, session: &Json) -> Usage {
        let data = session.get("data");
        let tokens_in = data.and_then(|d| d.path(&["tokens", "input"])).and_then(Json::as_u64).unwrap_or(0);
        let tokens_out = data.and_then(|d| d.path(&["tokens", "output"])).and_then(Json::as_u64).unwrap_or(0);
        let cost = data.and_then(|d| d.get("cost")).and_then(Json::as_f64).unwrap_or(0.0);
        Usage { tokens_in, tokens_out, cost_usd: cost, completeness: Completeness::Complete }
    }

    fn state_from(&self, session: &Json) -> OperationState {
        let outcome = session.str_at(&["data", "outcome"]).map(|s| s.to_string());
        match outcome.as_deref() {
            Some("succeeded") => OperationState::Completed {
                result_ref: artifact_ref("turn-result"),
                usage: self.usage_from(session),
            },
            Some(other) if !other.is_empty() => OperationState::Failed { reason: other.to_string() },
            _ => OperationState::Running,
        }
    }
}

// `Drop` below is the last-resort cleanup path.

impl Drop for OpenCodeAdapter {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        if let Some(root) = self.pgid {
            let targets = collect_targets(root);
            signal_targets(root, &targets, "-TERM");
            if let Some(mut child) = self.child.take() {
                let _ = child.wait();
            }
            if pid_alive(root) || group_alive(root) {
                signal_targets(root, &targets, "-KILL");
            }
        } else if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

impl HarnessAdapter for OpenCodeAdapter {
    fn describe(&self) -> Describe {
        Describe {
            api: HARNESS_API.to_string(),
            api_version: API_VERSION,
            impl_name: IMPL_NAME.to_string(),
            impl_version: self.impl_version.clone(),
            // No optional capability is advertised; S0-scope proof is required
            // before an OpenCode adapter may claim any of the six.
            capabilities: BTreeSet::new(),
        }
    }

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, HarnessError> {
        self.workspace = req.workspace.clone();
        self.spawn_server()?;
        let handle = AttemptHandle { id: attempt_id(1) };
        self.attempt = Some(handle.clone());
        Ok(handle)
    }

    fn create_session(&mut self, req: CreateSessionRequest) -> Result<SessionHandle, HarnessError> {
        self.session_seq += 1;
        let body = format!(
            "{{\"title\":{},\"model\":{{\"id\":{},\"providerID\":{}}},\"location\":{{\"directory\":{}}}}}",
            json_string(&format!("s0d-{}", req.role)),
            json_string(&self.model),
            json_string(&self.provider),
            json_string(&self.workspace),
        );
        let (code, resp) = self
            .http()?
            .request("POST", "/api/session", Some(&body))
            .map_err(HarnessError::KnownFailure)?;
        if code != 200 {
            return Err(HarnessError::KnownFailure(format!("create session -> {code}: {resp}")));
        }
        let parsed = Json::parse(&resp).map_err(HarnessError::UnknownOutcome)?;
        let native = parsed
            .str_at(&["data", "id"])
            .ok_or_else(|| HarnessError::UnknownOutcome("session create missing id".to_string()))?
            .to_string();
        let handle = SessionHandle { id: session_id(1, self.session_seq) };
        self.session_native.insert(handle.id.clone(), native);
        self.sessions.push(handle.clone());
        Ok(handle)
    }

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, HarnessError> {
        let native = self
            .session_native
            .get(&session.id)
            .cloned()
            .ok_or_else(|| HarnessError::KnownFailure(format!("unknown session {}", session.id)))?;
        self.turn_seq += 1;
        let handle = OperationHandle { id: operation_id(1, self.turn_seq) };
        let body = format!("{{\"text\":{}}}", json_string(prompt_for(&req.input_ref)));
        let path = format!("/api/session/{native}/prompt");
        let (code, resp) = self
            .http()?
            .request("POST", &path, Some(&body))
            .map_err(HarnessError::KnownFailure)?;
        if code != 200 {
            return Err(HarnessError::KnownFailure(format!("prompt -> {code}: {resp}")));
        }
        self.ops.insert(handle.id.clone(), OcOp { native_session: native });
        Ok(handle)
    }

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, HarnessError> {
        let native = self.native_for(op)?;
        let session = self.session_json(&native)?;
        Ok(self.state_from(&session))
    }

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, HarnessError> {
        let native = self.native_for(op)?;
        let session = self.session_json(&native)?;
        let state = self.state_from(&session);
        let mut events = vec![Event::TurnStarted];
        match state {
            OperationState::Completed { result_ref, usage } => {
                events.push(Event::UsageSnapshot { usage });
                events.push(Event::TurnCompleted { result_ref });
            }
            OperationState::Failed { reason } => {
                let mut usage = self.usage_from(&session);
                usage.completeness = Completeness::Partial;
                events.push(Event::UsageSnapshot { usage });
                events.push(Event::TurnFailed { reason });
            }
            _ => {}
        }
        Ok(events)
    }

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, HarnessError> {
        let native = self.native_for(op)?;
        let path = format!("/api/session/{native}/interrupt");
        let (code, resp) = self
            .http()?
            .request("POST", &path, None)
            .map_err(HarnessError::KnownFailure)?;
        if code != 200 {
            return Err(HarnessError::KnownFailure(format!("interrupt -> {code}: {resp}")));
        }
        let parsed = Json::parse(&resp).map_err(HarnessError::UnknownOutcome)?;
        let acknowledged = parsed.get("interrupted").and_then(Json::as_bool).unwrap_or(false);
        Ok(CancelAck {
            acknowledged,
            // Acknowledgment is not termination; termination evidence comes from
            // StopAttempt once the process group is confirmed gone.
            terminated: false,
            detail: "opencode: interrupt request".to_string(),
        })
    }

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), HarnessError> {
        if let Some(native) = self.session_native.remove(&session.id) {
            let path = format!("/api/session/{native}");
            let _ = self.http()?.request("DELETE", &path, None);
        }
        self.sessions.retain(|s| s.id != session.id);
        Ok(())
    }

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, HarnessError> {
        let _ = attempt;
        let mut gone = true;
        if let Some(root) = self.pgid {
            gone = terminate_tree(root, &mut self.child);
        } else if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        self.stopped = true;
        self.attempt = None;
        Ok(StopEvidence {
            process_group_gone: gone,
            detail: if gone {
                "opencode-process-group-gone".to_string()
            } else {
                "opencode-process-group-alive".to_string()
            },
        })
    }
}
