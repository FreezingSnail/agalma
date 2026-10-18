//! Real loopback acceptance for the WEB.12 factory control HTTP transport and
//! private typed client (`agalma-rwn.12`).
//!
//! A real `127.0.0.1` listener serves the declared factory routes and is driven
//! by a raw HTTP/1.1 loopback client. A real Connect/UDS hub serves the
//! `factory.read/v1` and `factory.control/v1` capabilities through the private
//! capability-binding descriptor and records every call, so the test can prove:
//!
//! - the client uses the exact WEB.9 request shapes and the correct independent
//!   binding for each operation;
//! - the operator session/CSRF gates fail closed and never leak a grant;
//! - the body/origin/method/content-type limits are exact;
//! - a refused input causes no effect and a timed-out effect is never retried.
//!
//! Required command:
//! `make test-focused SELECTION='-p agalma-ipc --test factory_http'`.
//! No case is `ignore`d.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agalma_contracts::control_plane::ExactU64;
use agalma_contracts::factory::{
    FactoryActionRequest, FactoryControlActor, FactoryControlNotFoundResponse,
    FactoryControlOutcome, FactoryControlReceiptBody, FactoryControlReceiptResponse,
    FactoryControlResult, FactoryControlUnconfirmedResponse, FactoryLookupRequest,
    FactoryRuntimeRequest, FactoryUnconfirmedCode, FACTORY_CONTROL_CAPABILITY,
    FACTORY_CONTROL_FORMAT, FACTORY_READ_CAPABILITY, FACTORY_RUNTIME_FORMAT,
};
use agalma_contracts::{ContractError, FailureCode};
use agalma_ipc::connect::Wire;
use agalma_ipc::factory_client::{parse_capability_bindings, register_factory_routes};
use agalma_ipc::plugin_server::{register_hub, HubHandler};
use agalma_ipc::server::Server;
use agalma_ipc::{
    serve_wire, FactoryRuntimeClient, ListenerConfig, LoopbackListener, RevocableControl, WebApp,
    POST_BODY_MAX_BYTES,
};
use agalma_testkit::ScratchDir;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Distinctive sentinel grants (canonical 32-byte hex launch tokens) so any
/// leak is detectable across every response.
const READ_GRANT: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const CONTROL_GRANT: &str =
    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const PLUGIN_ID: &str = "agalma-web-dashboard";
const GENERATION: &str = "7";

const UNAVAILABLE_JSON: &str =
    include_str!("../../../fixtures/factory-runtime-v1/runtime-unavailable.json");
const BOUNDARIES_JSON: &str =
    include_str!("../../../fixtures/factory-runtime-v1/boundaries.json");
const STATUSES_JSON: &str =
    include_str!("../../../fixtures/factory-runtime-v1/runtime-statuses.json");
const CURSOR_JSON: &str = include_str!("../../../fixtures/factory-runtime-v1/cursor.json");

/// One recorded `hub.InvokeCapability` call, as the hub saw it.
#[derive(Clone, Debug, Deserialize)]
struct SeenCall {
    grant: String,
    plugin_id: String,
    generation: u64,
    method: String,
    capability: String,
    operation_id: String,
    input: Value,
    deadline_unix_ms: u64,
    depth: u32,
}

/// The fake hub state: recorded calls, applied-effect counts, the snapshot to
/// return, and the terminal/unconfirmed responses to replay for an action id.
struct FakeHub {
    seen: Mutex<Vec<SeenCall>>,
    effects: Mutex<BTreeMap<String, usize>>,
    runtime: Mutex<Value>,
    receipts: Mutex<BTreeMap<String, Value>>,
    force: Mutex<BTreeMap<String, ContractError>>,
    forced_response: Mutex<BTreeMap<String, Value>>,
    delay: Mutex<Duration>,
    read_delay: Mutex<Duration>,
}

impl FakeHub {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            effects: Mutex::new(BTreeMap::new()),
            runtime: Mutex::new(serde_json::from_str(UNAVAILABLE_JSON).expect("fixture parses")),
            receipts: Mutex::new(BTreeMap::new()),
            force: Mutex::new(BTreeMap::new()),
            forced_response: Mutex::new(BTreeMap::new()),
            delay: Mutex::new(Duration::ZERO),
            read_delay: Mutex::new(Duration::ZERO),
        }
    }

    fn set_runtime(&self, value: Value) {
        *self.runtime.lock().unwrap() = value;
    }

    fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    /// Delay every read capability response (a stalled read peer).
    fn set_read_delay(&self, delay: Duration) {
        *self.read_delay.lock().unwrap() = delay;
    }

    fn force_error(&self, operation: &str, error: ContractError) {
        self.force
            .lock()
            .unwrap()
            .insert(operation.to_string(), error);
    }

    /// Force an exact (possibly inconsistent) response for one operation,
    /// without applying an effect. Used to prove the client validates response
    /// identity instead of trusting the hub.
    fn force_response(&self, operation: &str, value: Value) {
        self.forced_response
            .lock()
            .unwrap()
            .insert(operation.to_string(), value);
    }

    fn calls(&self) -> Vec<SeenCall> {
        self.seen.lock().unwrap().clone()
    }

    /// Total applied effects across every action id.
    fn total_effects(&self) -> usize {
        self.effects.lock().unwrap().values().sum()
    }

    /// Applied effects for one action id.
    fn effect_count(&self, action_id: &str) -> usize {
        self.effects
            .lock()
            .unwrap()
            .get(action_id)
            .copied()
            .unwrap_or(0)
    }
}

impl HubHandler for FakeHub {
    fn invoke_capability(&self, request: Value) -> Result<Value, ContractError> {
        let call: SeenCall = serde_json::from_value(request).map_err(|e| {
            ContractError::KnownFailure(format!("fake hub: malformed capability call: {e}"))
        })?;
        if call.method != "hub.InvokeCapability" {
            return Err(ContractError::KnownFailure(format!(
                "fake hub: unexpected method {}",
                call.method
            )));
        }
        let operation = call
            .input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ContractError::KnownFailure("fake hub: request has no operation".to_string())
            })?
            .to_string();

        // Exact capability/plugin/generation validation for each private
        // binding. A read grant can never authorize control (and vice versa).
        let (expected_capability, expected_grant) = match operation.as_str() {
            "snapshot" | "lookup" => (FACTORY_READ_CAPABILITY, READ_GRANT),
            "pause" | "resume" => (FACTORY_CONTROL_CAPABILITY, CONTROL_GRANT),
            other => {
                return Err(ContractError::UnsupportedCapability(format!(
                    "fake hub: unknown operation {other}"
                )))
            }
        };
        self.seen.lock().unwrap().push(call.clone());
        if call.capability != expected_capability {
            return Err(FailureCode::AuthorityDenied.failure(
                "capability",
                format!(
                    "operation {operation} must use {expected_capability}, not {}",
                    call.capability
                ),
            ));
        }
        if call.grant != expected_grant {
            return Err(FailureCode::AuthorityDenied.failure(
                "grant",
                "binding grant does not authorize this operation",
            ));
        }
        if call.plugin_id != PLUGIN_ID || call.generation != GENERATION.parse::<u64>().unwrap() {
            return Err(FailureCode::StaleGeneration.failure(
                "binding",
                "plugin id or generation is not the pinned binding",
            ));
        }

        if let Some(error) = self.force.lock().unwrap().get(&operation).cloned() {
            return Err(error);
        }

        if let Some(forced) = self.forced_response.lock().unwrap().get(&operation).cloned() {
            return Ok(forced);
        }

        match operation.as_str() {
            "snapshot" => {
                // Independently validate the exact WEB.9 snapshot request.
                let request: FactoryRuntimeRequest = serde_json::from_value(call.input.clone())
                    .map_err(|e| {
                        ContractError::KnownFailure(format!("fake hub: bad snapshot request: {e}"))
                    })?;
                request.validate()?;
                let read_delay = *self.read_delay.lock().unwrap();
                if !read_delay.is_zero() {
                    std::thread::sleep(read_delay);
                }
                Ok(self.runtime.lock().unwrap().clone())
            }
            "lookup" => {
                let request: FactoryLookupRequest = serde_json::from_value(call.input.clone())
                    .map_err(|e| {
                        ContractError::KnownFailure(format!("fake hub: bad lookup request: {e}"))
                    })?;
                request.validate()?;
                let read_delay = *self.read_delay.lock().unwrap();
                if !read_delay.is_zero() {
                    std::thread::sleep(read_delay);
                }
                Ok(self.lookup_response(&request.action_id))
            }
            "pause" | "resume" => {
                // The delay models a slow control effect (never a slow read).
                let delay = *self.delay.lock().unwrap();
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let action: FactoryActionRequest = serde_json::from_value(call.input.clone())
                    .map_err(|e| {
                        ContractError::KnownFailure(format!("fake hub: bad action request: {e}"))
                    })?;
                action.validate()?;
                Ok(self.apply_control(&action))
            }
            _ => unreachable!(),
        }
    }
}

impl FakeHub {
    fn lookup_response(&self, action_id: &str) -> Value {
        match self.receipts.lock().unwrap().get(action_id).cloned() {
            Some(response) => response,
            None => serde_json::to_value(FactoryControlNotFoundResponse {
                format: FACTORY_CONTROL_FORMAT.to_string(),
                result: FactoryControlResult::NotFound,
                action_id: action_id.to_string(),
                observed_stop: None,
            })
            .expect("not_found serializes"),
        }
    }

    fn apply_control(&self, action: &FactoryActionRequest) -> Value {
        // Replay a previously recorded terminal/unconfirmed response exactly.
        if let Some(existing) = self.receipts.lock().unwrap().get(&action.action_id).cloned() {
            return existing;
        }
        if action.action_id.starts_with("unconfirmed") {
            let response = serde_json::to_value(FactoryControlUnconfirmedResponse {
                format: FACTORY_CONTROL_FORMAT.to_string(),
                result: FactoryControlResult::Unconfirmed,
                action_id: action.action_id.clone(),
                request: action.clone(),
                request_digest: action.request_digest().expect("digest"),
                code: FactoryUnconfirmedCode::EffectUnconfirmed,
                reason: "durable intent recorded; effect not confirmed".to_string(),
                observed_stop: None,
            })
            .expect("unconfirmed serializes");
            self.receipts
                .lock()
                .unwrap()
                .insert(action.action_id.clone(), response.clone());
            return response;
        }
        // The existing trimmed confirmation guard: resume requires the literal
        // "resume"; pause is accepted. A denied outcome applies no effect.
        let accepted = match action.operation.as_str() {
            "resume" => action
                .confirmation
                .as_deref()
                .map(str::trim)
                .map(|value| value == "resume")
                .unwrap_or(false),
            _ => true,
        };
        let (outcome, reason) = if accepted {
            (FactoryControlOutcome::Applied, None)
        } else {
            (
                FactoryControlOutcome::Denied,
                Some("confirmation did not match the required phrase".to_string()),
            )
        };
        if accepted {
            *self
                .effects
                .lock()
                .unwrap()
                .entry(action.action_id.clone())
                .or_insert(0) += 1;
        }
        let response = serde_json::to_value(FactoryControlReceiptResponse {
            format: FACTORY_CONTROL_FORMAT.to_string(),
            result: FactoryControlResult::Receipt,
            receipt: FactoryControlReceiptBody {
                action_id: action.action_id.clone(),
                request: action.clone(),
                request_digest: action.request_digest().expect("digest"),
                outcome,
                reason,
                ledger_action_id: format!("ledger:{}", action.action_id),
                actor: FactoryControlActor::Human,
                recorded_at_unix_ms: ExactU64::from(1_791_820_805_000u64),
            },
            observed_stop: None,
        })
        .expect("receipt serializes");
        self.receipts
            .lock()
            .unwrap()
            .insert(action.action_id.clone(), response.clone());
        response
    }
}

// --- socket + loopback scaffolding -----------------------------------------

/// A short repository-local socket root whose physical bytes are retained under
/// the shared `target/test-runs` root (macOS `SUN_LEN` is short).
struct SocketRoot {
    _physical: ScratchDir,
    alias: PathBuf,
}

impl SocketRoot {
    fn new(tag: &str) -> Self {
        let physical = ScratchDir::new(tag);
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let worktree = manifest
            .parent()
            .and_then(Path::parent)
            .expect("crate lives under <root>/crates/");
        let is_worktree = worktree
            .parent()
            .and_then(Path::file_name)
            .map(|name| name == "worktrees")
            .unwrap_or(false);
        let alias_base = if is_worktree {
            worktree
                .parent()
                .and_then(Path::parent)
                .expect("shared cargo target")
                .join("test-runs")
        } else {
            physical
                .path()
                .parent()
                .expect("scratch root has a parent")
                .to_path_buf()
        };
        fs::create_dir_all(&alias_base).expect("create socket alias base");
        let pid = std::process::id();
        let alias = (0..64u32)
            .find_map(|attempt| {
                let candidate = alias_base.join(format!("w12{pid}-{attempt}"));
                match symlink(physical.path(), &candidate) {
                    Ok(()) => Some(candidate),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => panic!("create socket alias {}: {error}", candidate.display()),
                }
            })
            .unwrap_or_else(|| panic!("socket alias allocation exhausted"));
        Self {
            _physical: physical,
            alias,
        }
    }
}

impl Drop for SocketRoot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.alias);
    }
}

/// The running fake hub, listener and root of one dashboard test.
struct Dashboard {
    listener: LoopbackListener,
    hub: Arc<FakeHub>,
    authority: Arc<RevocableControl>,
    _root: SocketRoot,
}

fn spawn_hub(socket: PathBuf, hub: Arc<FakeHub>) {
    let info =
        agalma_ipc::plugin_server::plugin_server_info("hub", "test-hub", Vec::<String>::new());
    let mut server = Server::new(info);
    register_hub(&mut server, hub);
    std::thread::spawn(move || {
        if let Err(error) = serve_wire(server, &socket, Wire::Connect) {
            eprintln!("factory_http test hub stopped: {error}");
        }
    });
}

fn descriptor_json(socket: &Path, include_control: bool) -> String {
    let socket = socket.display();
    let mut bindings = vec![format!(
        r#"{{"capability":"{FACTORY_READ_CAPABILITY}","hub_socket":"{socket}","grant":"{READ_GRANT}","plugin_id":"{PLUGIN_ID}","generation":"{GENERATION}"}}"#
    )];
    if include_control {
        bindings.push(format!(
            r#"{{"capability":"{FACTORY_CONTROL_CAPABILITY}","hub_socket":"{socket}","grant":"{CONTROL_GRANT}","plugin_id":"{PLUGIN_ID}","generation":"{GENERATION}"}}"#
        ));
    }
    format!(
        r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{}]}}"#,
        bindings.join(",")
    )
}

/// Start a fake hub + listener around a fresh typed client.
async fn dashboard_with(hub: FakeHub, control: bool, deadline: Option<Duration>) -> Dashboard {
    dashboard_from(hub, control, deadline, true).await
}

async fn dashboard_from(
    hub: FakeHub,
    control: bool,
    deadline: Option<Duration>,
    include_control_binding: bool,
) -> Dashboard {
    let root = SocketRoot::new("factory-http");
    let socket = root.alias.join("hub.sock");
    let hub = Arc::new(hub);
    spawn_hub(socket.clone(), hub.clone());

    let raw = descriptor_json(&socket, include_control_binding);
    let bindings = parse_capability_bindings(&raw).expect("descriptor parses");
    let mut client = None;
    for _ in 0..500 {
        match FactoryRuntimeClient::connect_from_bindings(&bindings).await {
            Ok(connected) => {
                client = Some(connected);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    let mut client = client.expect("hub accepts a connection");
    if let Some(deadline) = deadline {
        client = client.with_deadline(deadline);
    }
    let shared = Arc::new(tokio::sync::Mutex::new(client));
    let authority = Arc::new(RevocableControl::new(control));
    let app = register_factory_routes(
        WebApp::builder(ListenerConfig::loopback(0)),
        shared,
        authority.clone(),
    )
    .build()
    .expect("factory app builds");
    let listener = LoopbackListener::start(app)
        .await
        .expect("loopback listener starts");
    Dashboard {
        listener,
        hub,
        authority,
        _root: root,
    }
}

// --- raw HTTP client --------------------------------------------------------

#[derive(Debug)]
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    text: String,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("response body is JSON")
    }

    fn error_code(&self) -> Option<String> {
        self.json()
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

fn host_of(addr: SocketAddr) -> String {
    format!("127.0.0.1:{}", addr.port())
}

/// Issue one raw HTTP/1.1 request over a fresh loopback TCP connection.
async fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    extra: &[(&str, &str)],
) -> RawResponse {
    try_raw_request(addr, method, path, host, extra)
        .await
        .expect("raw loopback request")
}

async fn try_raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    extra: &[(&str, &str)],
) -> Result<RawResponse, String> {
    let mut stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .map_err(|_| "connect timed out".to_string())?
        .map_err(|error| format!("connect: {error}"))?;
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (name, value) in extra {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| format!("write request: {error}"))?;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes))
        .await
        .map_err(|_| "read timed out".to_string())?
        .map_err(|error| format!("read response: {error}"))?;
    parse_response(bytes)
}

/// Issue one POST with an explicit body and `Content-Length`.
async fn raw_post(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut request = format!("POST {path} HTTP/1.1\r\nConnection: close\r\n");
    let mut has_host = false;
    let mut has_length = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            has_host = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_host {
        request.push_str(&format!("Host: {}\r\n", host_of(addr)));
    }
    if !has_length {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    // A server may answer (and close) before the whole body is consumed, so a
    // write or a post-response reset must not fail the harness.
    let _ = stream.write_all(request.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let bytes = read_tolerant(&mut stream).await;
    parse_lenient(bytes)
}

/// Issue one chunked POST (no `Content-Length`).
async fn raw_chunked_post(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    chunks: &[&[u8]],
) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut request = format!("POST {path} HTTP/1.1\r\nConnection: close\r\n");
    request.push_str(&format!("Host: {}\r\n", host_of(addr)));
    request.push_str("Transfer-Encoding: chunked\r\n");
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    let _ = stream.write_all(request.as_bytes()).await;
    for chunk in chunks {
        let _ = stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await;
        let _ = stream.write_all(chunk).await;
        let _ = stream.write_all(b"\r\n").await;
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
    let bytes = read_tolerant(&mut stream).await;
    parse_lenient(bytes)
}

/// Write one POST and drop the connection without reading the response,
/// modeling a real client disconnect after the request was written. The server
/// may or may not have admitted the request before the connection closed.
async fn disconnect_post(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut request = format!("POST {path} HTTP/1.1\r\nConnection: close\r\n");
    let mut has_host = false;
    let mut has_length = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            has_host = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_host {
        request.push_str(&format!("Host: {}\r\n", host_of(addr)));
    }
    if !has_length {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    let _ = stream.write_all(request.as_bytes()).await;
    let _ = stream.write_all(body).await;
    // Drop immediately: the response is never read.
    drop(stream);
}

/// Percent-encode every byte that is not an RFC 3986 unreserved character, so
/// the test can prove the server decodes a query value exactly once.
fn percent_encode(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Read from `stream` until EOF, error, or the timeout, retaining whatever was
/// received. A reset after the response bytes arrived is tolerated.
async fn read_tolerant(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                Err(_) => break,
            }
        }
    })
    .await;
    bytes
}

/// Parse a response, falling back to a status-0 placeholder when no complete
/// header block arrived (a hard reset before any response).
fn parse_lenient(bytes: Vec<u8>) -> RawResponse {
    match parse_response(bytes.clone()) {
        Ok(response) => response,
        Err(_) => RawResponse {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            text: String::from_utf8_lossy(&bytes).to_string(),
        },
    }
}

fn parse_response(bytes: Vec<u8>) -> Result<RawResponse, String> {
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "response has no header terminator".to_string())?;
    let head = String::from_utf8_lossy(&bytes[..split]).to_string();
    let body = bytes[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line {status_line:?}"))?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    let text = String::from_utf8_lossy(&bytes).to_string();
    Ok(RawResponse {
        status,
        headers,
        body,
        text,
    })
}

/// Bootstrap a real operator session through a same-origin factory GET.
async fn bootstrap(addr: SocketAddr) -> (String, String) {
    let host = host_of(addr);
    let response = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(response.status, 200, "bootstrap body: {}", response.text);
    let cookie = response
        .header("set-cookie")
        .expect("session Set-Cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = response
        .header("x-agalma-csrf")
        .expect("CSRF header")
        .to_string();
    (cookie, csrf)
}

fn assert_no_secret(response: &RawResponse) {
    for secret in [READ_GRANT, CONTROL_GRANT] {
        assert!(
            !response.text.contains(secret),
            "response disclosed a grant: {}",
            response.text
        );
        for (_, value) in &response.headers {
            assert!(!value.contains(secret), "response header disclosed a grant");
        }
    }
    assert!(
        !response.text.to_ascii_lowercase().contains("authorization"),
        "response disclosed an authorization header"
    );
}

/// A deterministic linear congruential generator for property cases.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    fn index(&mut self, len: usize) -> usize {
        (self.next() >> 33) as usize % len
    }
}

fn pause_body(action_id: &str, confirmation: Option<&str>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "format": "FactoryControl/v1",
        "operation": "pause",
        "action_id": action_id,
        "confirmation": confirmation,
    }))
    .unwrap()
}

fn resume_body(action_id: &str, confirmation: Option<&str>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "format": "FactoryControl/v1",
        "operation": "resume",
        "action_id": action_id,
        "confirmation": confirmation,
    }))
    .unwrap()
}

fn post_headers<'a>(
    host: &'a str,
    cookie: &'a str,
    csrf: &'a str,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![
        ("Host", host),
        ("Origin", ""),
        ("Cookie", cookie),
        ("X-Agalma-CSRF", csrf),
        ("Content-Type", "application/json"),
    ];
    // The Origin value is derived from the host.
    let origin = format!("http://{host}");
    headers[1].1 = Box::leak(origin.into_boxed_str());
    headers.extend_from_slice(extra);
    headers
}

// ---------------------------------------------------------------------------
// Real factory reads and controls
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_and_lookup_use_the_read_binding_and_bootstrap_a_session() {
    let hub = FakeHub::new();
    hub.set_runtime(serde_json::from_str(UNAVAILABLE_JSON).unwrap());
    let dashboard = dashboard_with(hub, true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let snapshot = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(snapshot.status, 200, "body: {}", snapshot.text);
    assert_eq!(snapshot.header("content-type"), Some("application/json"));
    assert_eq!(snapshot.header("cache-control"), Some("no-store"));
    // A same-origin factory GET bootstraps the session and CSRF.
    let set_cookie = snapshot.header("set-cookie").expect("Set-Cookie");
    assert!(
        set_cookie.contains("agalma_operator=")
            && set_cookie.contains("HttpOnly")
            && set_cookie.contains("SameSite=Strict")
            && set_cookie.contains("Path=/api/v1/factory"),
        "cookie flags: {set_cookie}"
    );
    assert!(!set_cookie.contains("Secure"), "loopback HTTP omits Secure");
    let csrf = snapshot.header("x-agalma-csrf").expect("CSRF header");
    assert_eq!(csrf.len(), 64, "CSRF is a 32-byte hex secret");
    assert!(
        !set_cookie.contains(csrf),
        "session and CSRF tokens are independent"
    );

    let body = snapshot.json();
    assert_eq!(body["format"], FACTORY_RUNTIME_FORMAT);
    assert_eq!(body["status"], "Unavailable");
    assert_eq!(body["runtime_instance_id"], Value::Null);
    assert_eq!(body["observed_at_unix_ms"], "9223372036854775807");
    assert_no_secret(&snapshot);

    // Authoritative lookup of an absent action id.
    let lookup = raw_request(
        addr,
        "GET",
        "/api/v1/factory/actions/absent-action",
        &host,
        &[],
    )
    .await;
    assert_eq!(lookup.status, 404, "body: {}", lookup.text);
    assert_eq!(lookup.json()["result"], "not_found");
    assert_no_secret(&lookup);

    let calls = dashboard.hub.calls();
    assert_eq!(calls.len(), 2, "one call per read: {calls:?}");
    assert_eq!(calls[0].capability, FACTORY_READ_CAPABILITY);
    assert_eq!(calls[0].grant, READ_GRANT, "reads use the read grant");
    assert_eq!(calls[0].input["operation"], "snapshot");
    assert_eq!(calls[1].capability, FACTORY_READ_CAPABILITY);
    assert_eq!(calls[1].input["operation"], "lookup");
    assert_eq!(calls[1].input["action_id"], "absent-action");
    for call in &calls {
        assert_eq!(call.method, "hub.InvokeCapability");
        assert_eq!(call.plugin_id, PLUGIN_ID);
        assert_eq!(call.generation, GENERATION.parse::<u64>().unwrap());
        assert_eq!(call.depth, 0);
        assert!(call.deadline_unix_ms > 0);
    }
    assert_eq!(dashboard.hub.total_effects(), 0, "reads invoke no effect");
    let unique_ids: std::collections::BTreeSet<&str> = calls
        .iter()
        .map(|call| call.operation_id.as_str())
        .collect();
    assert_eq!(unique_ids.len(), calls.len(), "operation ids are unique");

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_and_resume_map_receipts_and_denied_confirmation() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    // Applied pause (no confirmation required).
    let pause = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("op-pause-1", None),
    )
    .await;
    assert_eq!(pause.status, 200, "body: {}", pause.text);
    assert_eq!(pause.json()["result"], "receipt");
    assert_eq!(pause.json()["receipt"]["outcome"], "applied");
    assert_no_secret(&pause);

    // Applied resume with the exact confirmation.
    let resume = raw_post(
        addr,
        "/api/v1/factory/resume",
        &post_headers(&host, &cookie, &csrf, &[]),
        &resume_body("op-resume-1", Some("resume")),
    )
    .await;
    assert_eq!(resume.status, 200, "body: {}", resume.text);
    assert_eq!(resume.json()["receipt"]["outcome"], "applied");

    // Denied resume: wrong confirmation is a terminal denied receipt (200).
    let denied = raw_post(
        addr,
        "/api/v1/factory/resume",
        &post_headers(&host, &cookie, &csrf, &[]),
        &resume_body("op-resume-2", Some("nope")),
    )
    .await;
    assert_eq!(denied.status, 200, "body: {}", denied.text);
    assert_eq!(denied.json()["receipt"]["outcome"], "denied");
    assert_no_secret(&denied);

    assert_eq!(dashboard.hub.effect_count("op-pause-1"), 1);
    assert_eq!(dashboard.hub.effect_count("op-resume-1"), 1);
    assert_eq!(
        dashboard.hub.effect_count("op-resume-2"),
        0,
        "a denied confirmation applies no effect"
    );

    // Controls use the independent control binding/grant.
    let controls: Vec<SeenCall> = dashboard
        .hub
        .calls()
        .into_iter()
        .filter(|call| call.capability == FACTORY_CONTROL_CAPABILITY)
        .collect();
    assert_eq!(controls.len(), 3);
    for call in &controls {
        assert_eq!(call.grant, CONTROL_GRANT, "controls use the control grant");
    }

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_cookie_csrf_gates_fail_closed_and_rotate() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    let body = pause_body("op-gate", None);
    let baseline = dashboard.hub.total_effects();

    // Missing Origin.
    let no_origin = raw_post(
        addr,
        "/api/v1/factory/pause",
        &[
            ("Host", &host),
            ("Cookie", &cookie),
            ("X-Agalma-CSRF", &csrf),
            ("Content-Type", "application/json"),
        ],
        &body,
    )
    .await;
    assert_eq!(no_origin.status, 403, "body: {}", no_origin.text);

    // Foreign Origin.
    let foreign = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[("Origin", "http://evil.example")]),
        &body,
    )
    .await;
    assert_eq!(foreign.status, 403);
    assert!(!foreign.text.contains("evil.example"));

    // Missing cookie.
    let no_cookie = raw_post(
        addr,
        "/api/v1/factory/pause",
        &[
            ("Host", &host),
            ("Origin", &format!("http://{host}")),
            ("X-Agalma-CSRF", &csrf),
            ("Content-Type", "application/json"),
        ],
        &body,
    )
    .await;
    assert_eq!(no_cookie.status, 403, "body: {}", no_cookie.text);

    // Wrong cookie and wrong CSRF.
    let wrong_cookie = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, "agalma_operator=deadbeef", &csrf, &[]),
        &body,
    )
    .await;
    assert_eq!(wrong_cookie.status, 403);
    let wrong_csrf = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, "0000", &[]),
        &body,
    )
    .await;
    assert_eq!(wrong_csrf.status, 403);

    // A valid control still works.
    let ok = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &body,
    )
    .await;
    assert_eq!(ok.status, 200, "body: {}", ok.text);

    // Rotated tokens: a second listener has a different session; the first
    // session's cookie is now invalid.
    let second = dashboard_with(FakeHub::new(), true, None).await;
    let second_addr = second.listener.local_addr();
    let rotated = raw_post(
        second_addr,
        "/api/v1/factory/pause",
        &post_headers(&host_of(second_addr), &cookie, &csrf, &[]),
        &pause_body("op-rotated", None),
    )
    .await;
    assert_eq!(rotated.status, 403, "old session accepted after restart");
    assert_eq!(second.hub.total_effects(), 0);

    assert_eq!(dashboard.hub.total_effects(), baseline + 1);
    second.listener.shutdown().await;
    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_limits_content_type_and_strict_json_are_exact() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    // 8192 bytes is within the transport bound (the strict handler then rejects
    // it as invalid input, but never with 413).
    let at_limit = vec![b'a'; 8192];
    let accepted = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &at_limit,
    )
    .await;
    assert_ne!(accepted.status, 413, "8192-byte body must not be 413");
    assert_eq!(accepted.status, 400, "body: {}", accepted.text);

    // 8193 bytes is refused with 413.
    let over = vec![b'a'; 8193];
    let too_large = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &over,
    )
    .await;
    assert_eq!(too_large.status, 413, "body: {}", too_large.text);

    // A chunked body over the limit is refused regardless of Content-Length.
    let chunked_over = raw_chunked_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &[&vec![b'b'; 5000], &vec![b'b'; 3193]],
    )
    .await;
    assert_eq!(chunked_over.status, 413, "body: {}", chunked_over.text);

    // A lying Content-Length cannot smuggle an oversized admitted body.
    let lying = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(
            &host,
            &cookie,
            &csrf,
            &[("Content-Length", "100")],
        ),
        &vec![b'c'; 9000],
    )
    .await;
    assert_ne!(lying.status, 200, "lying length admitted: {}", lying.text);

    // Wrong content type -> 415.
    let wrong_type = raw_post(
        addr,
        "/api/v1/factory/pause",
        &[
            ("Host", &host),
            ("Origin", &format!("http://{host}")),
            ("Cookie", &cookie),
            ("X-Agalma-CSRF", &csrf),
            ("Content-Type", "text/plain"),
        ],
        &pause_body("op-type", None),
    )
    .await;
    assert_eq!(wrong_type.status, 415, "body: {}", wrong_type.text);

    // Optional charset=utf-8 is accepted.
    let charset = raw_post(
        addr,
        "/api/v1/factory/pause",
        &[
            ("Host", &host),
            ("Origin", &format!("http://{host}")),
            ("Cookie", &cookie),
            ("X-Agalma-CSRF", &csrf),
            ("Content-Type", "application/json; charset=utf-8"),
        ],
        &pause_body("op-charset", None),
    )
    .await;
    assert_eq!(charset.status, 200, "body: {}", charset.text);

    // Malformed JSON, wrong schema version, operation mismatch, unknown field
    // and an `actor` field are all 400 before any RPC.
    let cases: Vec<Vec<u8>> = vec![
        b"{not json".to_vec(),
        serde_json::to_vec(&json!({
            "format": "FactoryControl/v2",
            "operation": "pause",
            "action_id": "op-v2",
            "confirmation": null
        }))
        .unwrap(),
        serde_json::to_vec(&json!({
            "format": "FactoryControl/v1",
            "operation": "resume",
            "action_id": "op-mismatch",
            "confirmation": "resume"
        }))
        .unwrap(),
        serde_json::to_vec(&json!({
            "format": "FactoryControl/v1",
            "operation": "pause",
            "action_id": "op-extra",
            "confirmation": null,
            "extra": 1
        }))
        .unwrap(),
        serde_json::to_vec(&json!({
            "format": "FactoryControl/v1",
            "operation": "pause",
            "action_id": "op-actor",
            "confirmation": null,
            "actor": "human"
        }))
        .unwrap(),
    ];
    let before = dashboard.hub.calls().len();
    for case in cases {
        let response = raw_post(
            addr,
            "/api/v1/factory/pause",
            &post_headers(&host, &cookie, &csrf, &[]),
            &case,
        )
        .await;
        assert_eq!(response.status, 400, "body: {}", response.text);
        assert_no_secret(&response);
    }
    assert_eq!(
        dashboard.hub.calls().len(),
        before,
        "a malformed control body reached the hub"
    );
    assert_eq!(dashboard.hub.total_effects(), 1, "only the charset pause applied");
    assert_no_secret(&accepted);
    assert_no_secret(&too_large);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_and_method_matrix_is_exact() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    // POST to a GET route -> 405 Allow: GET.
    let post_get = raw_post(
        addr,
        "/api/v1/factory",
        &[("Host", &host), ("Origin", &format!("http://{host}"))],
        b"{}",
    )
    .await;
    assert_eq!(post_get.status, 405, "body: {}", post_get.text);
    assert_eq!(post_get.header("allow"), Some("GET"));

    // GET to a POST route -> 405 Allow: POST.
    let get_post = raw_request(addr, "GET", "/api/v1/factory/pause", &host, &[]).await;
    assert_eq!(get_post.status, 405);
    assert_eq!(get_post.header("allow"), Some("POST"));

    // Unknown route -> 404.
    let unknown = raw_post(
        addr,
        "/api/v1/factory/cancel",
        &[("Host", &host), ("Origin", &format!("http://{host}"))],
        b"{}",
    )
    .await;
    assert_eq!(unknown.status, 404, "body: {}", unknown.text);

    // Other mutation methods on a declared route -> 405.
    for method in ["PUT", "DELETE", "PATCH", "OPTIONS"] {
        let response = raw_request(addr, method, "/api/v1/factory", &host, &[]).await;
        assert_eq!(response.status, 405, "{method}: {}", response.text);
    }

    // A GET lookup with a malformed action id -> 400.
    let bad_id = raw_request(addr, "GET", "/api/v1/factory/actions/bad%20id", &host, &[]).await;
    assert_eq!(bad_id.status, 400, "body: {}", bad_id.text);

    // The preserved GET surface still serves.
    let snapshot = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(snapshot.status, 200);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_identities_survive_as_exact_decimal_strings() {
    let hub = FakeHub::new();
    hub.set_runtime(serde_json::from_str(BOUNDARIES_JSON).unwrap());
    let dashboard = dashboard_with(hub, true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let response = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(response.status, 200, "body: {}", response.text);
    let body = response.json();
    assert_eq!(body["observed_at_unix_ms"], "18446744073709551615");
    assert_eq!(
        body["current_execution"]["generation"],
        "9007199254740993"
    );
    assert_eq!(body["eligible"]["total"], "18446744073709551615");
    assert_no_secret(&response);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_http_mapping_is_reachable() {
    // 200 applied/denied is covered elsewhere; here we drive the rest.
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    // 202 unconfirmed.
    let unconfirmed = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("unconfirmed-1", None),
    )
    .await;
    assert_eq!(unconfirmed.status, 202, "body: {}", unconfirmed.text);
    assert_eq!(unconfirmed.json()["result"], "unconfirmed");

    // 409 identity conflict.
    dashboard.hub.force_error(
        "pause",
        FailureCode::IdentityConflict.failure("action_id", "input conflict"),
    );
    let conflict = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("conflict-1", None),
    )
    .await;
    assert_eq!(conflict.status, 409, "body: {}", conflict.text);
    assert_no_secret(&conflict);

    // 403 authority denied.
    dashboard.hub.force_error(
        "pause",
        FailureCode::AuthorityDenied.failure("grant", "expired"),
    );
    let authority = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("authority-1", None),
    )
    .await;
    assert_eq!(authority.status, 403, "body: {}", authority.text);

    // 503 unavailable.
    dashboard.hub.force_error(
        "snapshot",
        FailureCode::StateUnavailable.failure("runtime", "not attached"),
    );
    let unavailable = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(unavailable.status, 503, "body: {}", unavailable.text);
    assert_no_secret(&unavailable);

    dashboard.listener.shutdown().await;

    // 504 deadline on a read: a stalled read peer with a short admitted budget.
    // A read that cannot complete in time is a plain deadline failure.
    let hub = FakeHub::new();
    hub.set_read_delay(Duration::from_millis(500));
    let slow = dashboard_with(hub, true, Some(Duration::from_millis(80))).await;
    let addr = slow.listener.local_addr();
    let host = host_of(addr);
    let deadline = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(deadline.status, 504, "body: {}", deadline.text);
    assert_eq!(deadline.error_code().as_deref(), Some("deadline_exceeded"));
    assert_no_secret(&deadline);
    slow.listener.shutdown().await;

    // A control deadline that elapses after dispatch is 202 unconfirmed with the
    // original action id (a durable, truthful unknown), never a 504 that would
    // claim no effect.
    let hub = FakeHub::new();
    let slow = dashboard_with(hub, true, Some(Duration::from_millis(80))).await;
    let addr = slow.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    // Delay only the control effect, after the session bootstrap has succeeded.
    slow.hub.set_delay(Duration::from_millis(500));
    let unconfirmed = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("deadline-control-1", None),
    )
    .await;
    assert_eq!(unconfirmed.status, 202, "body: {}", unconfirmed.text);
    assert_eq!(unconfirmed.json()["result"], "unconfirmed");
    assert_eq!(unconfirmed.json()["action_id"], "deadline-control-1");
    assert_eq!(unconfirmed.json()["code"], "EffectUnconfirmed");
    assert_no_secret(&unconfirmed);
    slow.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_response_is_reconciled_by_same_id_lookup_and_never_retried() {
    let hub = FakeHub::new();
    hub.set_delay(Duration::from_millis(300));
    let dashboard = dashboard_with(hub, true, Some(Duration::from_millis(60))).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    let lost = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("op-lost-1", None),
    )
    .await;
    // The effect may have been dispatched, so the deadline is reported as a
    // durable unconfirmed result for the original id (202), not a 504.
    assert_eq!(lost.status, 202, "body: {}", lost.text);
    assert_eq!(lost.json()["result"], "unconfirmed");
    assert_eq!(lost.json()["action_id"], "op-lost-1");
    assert_eq!(lost.json()["code"], "EffectUnconfirmed");

    // The admitted hub effect still completes; the client did not retry.
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(
        dashboard.hub.effect_count("op-lost-1"),
        1,
        "the effect must not be replayed"
    );

    // Same-ID lookup reconciles the durable receipt.
    let lookup = raw_request(
        addr,
        "GET",
        "/api/v1/factory/actions/op-lost-1",
        &host,
        &[],
    )
    .await;
    assert_eq!(lookup.status, 200, "body: {}", lookup.text);
    assert_eq!(lookup.json()["receipt"]["action_id"], "op-lost-1");
    assert_eq!(lookup.json()["receipt"]["outcome"], "applied");

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_ceiling_returns_503_without_queueing() {
    let hub = FakeHub::new();
    hub.set_delay(Duration::from_millis(200));
    let dashboard = dashboard_with(hub, true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    let mut tasks = Vec::new();
    for index in 0..16 {
        let host = host.clone();
        let cookie = cookie.clone();
        let csrf = csrf.clone();
        let body = pause_body(&format!("busy-{index}"), None);
        tasks.push(tokio::spawn(async move {
            raw_post(
                addr,
                "/api/v1/factory/pause",
                &post_headers(&host, &cookie, &csrf, &[]),
                &body,
            )
            .await
        }));
    }
    // Give the admitted requests time to acquire every slot.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let refused = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("busy-overflow", None),
    )
    .await;
    assert_eq!(refused.status, 503, "body: {}", refused.text);
    assert_eq!(refused.error_code().as_deref(), Some("overloaded"));

    for task in tasks {
        let response = task.await.expect("busy task");
        assert_eq!(response.status, 200, "body: {}", response.text);
    }
    assert_eq!(
        dashboard.hub.effect_count("busy-overflow"),
        0,
        "the refused request applied no effect"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_package_has_no_session_and_denies_control() {
    // No control binding: the client cannot pause/resume at all.
    let hub = FakeHub::new();
    let dashboard = dashboard_from(hub, false, None, false).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let snapshot = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(snapshot.status, 200, "body: {}", snapshot.text);
    assert_eq!(snapshot.header("set-cookie"), None, "no session cookie");
    assert_eq!(snapshot.header("x-agalma-csrf"), None, "no CSRF token");
    assert_no_secret(&snapshot);

    // Control disabled: every POST is refused without an effect.
    let post = raw_post(
        addr,
        "/api/v1/factory/pause",
        &[("Host", &host), ("Origin", &format!("http://{host}"))],
        &pause_body("readonly-1", None),
    )
    .await;
    assert_eq!(post.status, 403, "body: {}", post.text);
    assert_eq!(dashboard.hub.total_effects(), 0);

    // The typed client holds no control authority.
    let raw = descriptor_json(&PathBuf::from("/nonexistent"), false);
    let bindings = parse_capability_bindings(&raw).unwrap();
    assert!(bindings.control().is_none());
    assert_eq!(dashboard.hub.total_effects(), 0);

    dashboard.listener.shutdown().await;
}

// ---------------------------------------------------------------------------
// Deterministic property cases
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_authorization_and_header_combinations() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    let mut rng = Lcg::new(0x5745_4212);
    let mut effects = 0usize;

    for case in 0..128u32 {
        // Randomize each authorization axis; admitted only when every axis is
        // exactly the configured/valid value.
        let host_ok = rng.next() % 4 != 0;
        let origin_mode = rng.index(6); // 0 valid; 1..=5 defective
        let cookie_ok = rng.next() % 3 != 0;
        let csrf_ok = rng.next() % 3 != 0;
        let content_ok = rng.next() % 5 != 0;

        let request_host = if host_ok {
            host.clone()
        } else {
            format!("evil{case}.example:1")
        };
        let origin = match origin_mode {
            0 => Some(format!("http://{host}")),
            1 => None,
            2 => Some(format!("http://evil{case}.example")),
            3 => Some(format!("https://{host}")),
            4 => Some(format!("http://127.0.0.1:{}", addr.port().wrapping_add(1))),
            _ => Some(format!("http://{host}")), // duplicated below
        };
        let duplicate_origin = origin_mode == 5;
        let cookie_header = if cookie_ok {
            cookie.clone()
        } else {
            "agalma_operator=sentinel-deadbeef".to_string()
        };
        let csrf_header = if csrf_ok {
            csrf.clone()
        } else {
            "sentinel-wrong".to_string()
        };
        let content_type = if content_ok {
            "application/json"
        } else {
            "text/plain"
        };

        let mut headers: Vec<(&str, &str)> = vec![
            ("Host", request_host.as_str()),
            ("Cookie", cookie_header.as_str()),
            ("X-Agalma-CSRF", csrf_header.as_str()),
            ("Content-Type", content_type),
        ];
        if let Some(origin) = &origin {
            headers.push(("Origin", origin.as_str()));
        }
        if duplicate_origin {
            if let Some(origin) = &origin {
                headers.push(("Origin", origin.as_str()));
            }
        }
        let body = pause_body(&format!("authz-{case}"), None);
        let response = raw_post(addr, "/api/v1/factory/pause", &headers, &body).await;

        let auth_valid = host_ok && origin_mode == 0 && cookie_ok && csrf_ok;
        if auth_valid && content_ok {
            assert_eq!(response.status, 200, "case {case}: {}", response.text);
            effects += 1;
        } else if auth_valid {
            assert_eq!(response.status, 415, "case {case}: {}", response.text);
        } else {
            assert_eq!(response.status, 403, "case {case}: {}", response.text);
        }
        assert_no_secret(&response);
    }

    assert_eq!(
        dashboard.hub.total_effects(),
        effects,
        "only fully-authorized controls applied an effect"
    );

    dashboard.listener.shutdown().await;
}

/// The independently-derived admission oracle for one generated body case. The
/// expectation is a property of how the case was constructed, never a result of
/// invoking the production request parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyAdmission {
    /// A well-formed, in-bounds `FactoryControl/v1` pause for this route:
    /// HTTP 200 and exactly one applied effect.
    AppliedPause,
    /// Refused before any hub dispatch (malformed/version/operation/reason
    /// class): HTTP 400 and no effect.
    RefusedBadRequest,
    /// Refused at the 8192-byte body bound: HTTP 413 and no effect.
    RefusedTooLarge,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_body_and_boundary_inputs() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    let mut rng = Lcg::new(0x5745_4212);

    // Every case carries its own independently-generated admission class; the
    // oracle never calls `FactoryControlRequest::parse`.
    let mut cases: Vec<(Vec<u8>, BodyAdmission)> = Vec::new();
    for size in [0usize, 1, 2, 16, 4096, 8191, 8192, 8193, 8194, 16384] {
        // Non-JSON filler: accepted (<= 8192) is refused as malformed; larger
        // bodies are refused at the limit before any parse.
        let admission = if size > POST_BODY_MAX_BYTES {
            BodyAdmission::RefusedTooLarge
        } else {
            BodyAdmission::RefusedBadRequest
        };
        cases.push((vec![b'x'; size], admission));
    }
    for _ in 0..(128 - cases.len()) {
        match rng.index(4) {
            // Well-formed pause: admitted with one effect.
            0 => cases.push((
                pause_body(&format!("gen-{}", rng.next()), None),
                BodyAdmission::AppliedPause,
            )),
            // Well-formed resume sent to the pause route: operation mismatch.
            1 => cases.push((
                resume_body(&format!("gen-{}", rng.next()), Some("resume")),
                BodyAdmission::RefusedBadRequest,
            )),
            // Well-formed pause plus an unknown field: malformed.
            2 => {
                let body = serde_json::to_vec(&json!({
                    "format": "FactoryControl/v1",
                    "operation": "pause",
                    "action_id": format!("gen-{}", rng.next()),
                    "confirmation": null,
                    "extra": rng.next(),
                }))
                .unwrap();
                cases.push((body, BodyAdmission::RefusedBadRequest));
            }
            // A pause missing the required-nullable `confirmation`: malformed.
            _ => {
                let body = serde_json::to_vec(&json!({
                    "format": "FactoryControl/v1",
                    "operation": "pause",
                    "action_id": format!("gen-{}", rng.next()),
                }))
                .unwrap();
                cases.push((body, BodyAdmission::RefusedBadRequest));
            }
        }
    }

    let expected_effects = cases
        .iter()
        .filter(|(_, admission)| *admission == BodyAdmission::AppliedPause)
        .count();

    let before = dashboard.hub.total_effects();
    let mut observed_effects = 0usize;
    for (body, admission) in &cases {
        let response = raw_post(
            addr,
            "/api/v1/factory/pause",
            &post_headers(&host, &cookie, &csrf, &[]),
            body,
        )
        .await;
        let (expected_status, applies) = match admission {
            BodyAdmission::AppliedPause => (200u16, true),
            BodyAdmission::RefusedBadRequest => (400, false),
            BodyAdmission::RefusedTooLarge => (413, false),
        };
        assert_eq!(
            response.status,
            expected_status,
            "{admission:?} for {} bytes: {}",
            body.len(),
            response.text
        );
        if applies {
            observed_effects += 1;
        }
        assert_no_secret(&response);
    }

    assert_eq!(
        observed_effects, expected_effects,
        "observed admissions disagreed with the independent oracle"
    );
    assert_eq!(
        dashboard.hub.total_effects(),
        before + expected_effects,
        "only independently-valid pause actions applied an effect"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_disconnect_retry_and_lookup_sequences() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    let mut rng = Lcg::new(0x5745_4212);
    // A deliberate, generated mix of applied actions, repeated ids, real client
    // disconnects, same-id retries and lookups. An action id must never apply
    // more than one effect, regardless of duplicates or lost responses.
    let mut touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for case in 0..64u32 {
        // Keep pause and resume ids in separate namespaces: an action id
        // identifies one exact request, so reusing one across operations would
        // (correctly) be an identity conflict rather than a replay.
        let id = rng.index(8);
        match rng.index(5) {
            0 => {
                let action_id = format!("seqp-{id}");
                touched.insert(action_id.clone());
                // Applied pause; repeating the id must not apply again.
                let response = raw_post(
                    addr,
                    "/api/v1/factory/pause",
                    &post_headers(&host, &cookie, &csrf, &[]),
                    &pause_body(&action_id, None),
                )
                .await;
                assert_eq!(response.status, 200, "case {case}: {}", response.text);
            }
            1 => {
                let action_id = format!("seqr-{id}");
                touched.insert(action_id.clone());
                let response = raw_post(
                    addr,
                    "/api/v1/factory/resume",
                    &post_headers(&host, &cookie, &csrf, &[]),
                    &resume_body(&action_id, Some("resume")),
                )
                .await;
                assert_eq!(response.status, 200, "case {case}: {}", response.text);
            }
            2 => {
                // A denied resume applies no effect.
                let response = raw_post(
                    addr,
                    "/api/v1/factory/resume",
                    &post_headers(&host, &cookie, &csrf, &[]),
                    &resume_body(&format!("deny-{case}"), Some("wrong")),
                )
                .await;
                assert_eq!(response.status, 200, "case {case}: {}", response.text);
            }
            3 => {
                let action_id = format!("seqp-{id}");
                touched.insert(action_id.clone());
                // A real client disconnect: write an admitted pause and drop the
                // connection without reading the response. The same id is then
                // retried explicitly and must not create a second effect.
                disconnect_post(
                    addr,
                    "/api/v1/factory/pause",
                    &post_headers(&host, &cookie, &csrf, &[]),
                    &pause_body(&action_id, None),
                )
                .await;
                let retry = raw_post(
                    addr,
                    "/api/v1/factory/pause",
                    &post_headers(&host, &cookie, &csrf, &[]),
                    &pause_body(&action_id, None),
                )
                .await;
                assert_eq!(retry.status, 200, "case {case}: {}", retry.text);
                assert_eq!(
                    retry.json()["receipt"]["action_id"], action_id,
                    "retry must preserve the original action id"
                );
            }
            _ => {
                // Lookup while paused/unavailable must never create an effect.
                let action_id = format!("seqp-{id}");
                let response = raw_request(
                    addr,
                    "GET",
                    &format!("/api/v1/factory/actions/{action_id}"),
                    &host,
                    &[],
                )
                .await;
                assert!(response.status == 200 || response.status == 404);
                assert_no_secret(&response);
            }
        }
    }

    // Reconcile every touched id through an authoritative lookup: a receipt
    // decides the expected effect, independently of the POST responses. A
    // missing receipt means no effect, and no id may ever apply twice.
    for action_id in &touched {
        let lookup = raw_request(
            addr,
            "GET",
            &format!("/api/v1/factory/actions/{action_id}"),
            &host,
            &[],
        )
        .await;
        let effect = dashboard.hub.effect_count(action_id);
        assert!(effect <= 1, "action {action_id} applied more than once");
        match lookup.status {
            200 => {
                let outcome = lookup.json()["receipt"]["outcome"]
                    .as_str()
                    .map(str::to_string);
                if outcome.as_deref() == Some("applied") {
                    assert_eq!(
                        effect, 1,
                        "applied receipt for {action_id} must show exactly one effect"
                    );
                } else {
                    assert_eq!(effect, 0, "denied receipt for {action_id} must show no effect");
                }
            }
            404 => assert_eq!(effect, 0, "no receipt for {action_id} but an effect applied"),
            other => panic!("unexpected lookup status {other} for {action_id}"),
        }
    }

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_admitted_control_and_closes_keep_alive() {
    let hub = FakeHub::new();
    hub.set_delay(Duration::from_millis(200));
    let dashboard = dashboard_with(hub, true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    // Keep-alive: two GETs on one connection.
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    for _ in 0..2 {
        let request = format!(
            "GET /api/v1/factory HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until the declared Content-Length is satisfied.
        loop {
            let read = stream.read(&mut chunk).await.expect("read");
            assert!(read > 0, "connection closed early");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..split]).to_string();
                let length = head
                    .split("\r\n")
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buffer.len() >= split + 4 + length {
                    break;
                }
            }
        }
        let parsed = parse_response(buffer).expect("parse");
        assert_eq!(parsed.status, 200);
    }

    // Shutdown drains an admitted control.
    let host2 = host.clone();
    let cookie2 = cookie.clone();
    let csrf2 = csrf.clone();
    let request = tokio::spawn(async move {
        raw_post(
            addr,
            "/api/v1/factory/pause",
            &post_headers(&host2, &cookie2, &csrf2, &[]),
            &pause_body("shutdown-1", None),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let shutdown = tokio::spawn(dashboard.listener.shutdown());
    shutdown.await.expect("shutdown");
    let drained = request.await.expect("admitted control task");
    assert!(
        drained.status == 200 || drained.status == 503,
        "admitted control did not drain cleanly: {}",
        drained.text
    );

    let after = try_raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert!(after.is_err(), "listener accepted after shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_snapshot_and_typed_read_only_client() {
    // The typed client exposes pause/lookup/snapshot and never falls back to a
    // read grant for control.
    let statuses: Vec<Value> = serde_json::from_str(STATUSES_JSON).unwrap();
    let paused = statuses
        .into_iter()
        .find(|value| value["status"] == "Paused")
        .expect("a Paused fixture");
    let hub = FakeHub::new();
    hub.set_runtime(paused);
    let dashboard = dashboard_with(hub, true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let snapshot = raw_request(addr, "GET", "/api/v1/factory?limit=5", &host, &[]).await;
    assert_eq!(snapshot.status, 200, "body: {}", snapshot.text);
    assert_eq!(snapshot.json()["status"], "Paused");
    assert!(snapshot.json()["permissions"]["pause"].is_boolean());
    assert_no_secret(&snapshot);

    let calls = dashboard.hub.calls();
    assert_eq!(calls[0].input["operation"], "snapshot");
    assert_eq!(calls[0].input["limit"], 5);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_limit_and_cursor_are_validated_before_rpc() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    for path in [
        "/api/v1/factory?limit=0",
        "/api/v1/factory?limit=101",
        "/api/v1/factory?limit=abc",
        "/api/v1/factory?limit=-1",
    ] {
        let response = raw_request(addr, "GET", path, &host, &[]).await;
        assert_eq!(response.status, 400, "{path}: {}", response.text);
    }
    assert_eq!(
        dashboard.hub.calls().len(),
        0,
        "a malformed snapshot request reached the hub"
    );

    // A huge action id is refused before the hub.
    let huge = "a".repeat(200);
    let response = raw_request(
        addr,
        "GET",
        &format!("/api/v1/factory/actions/{huge}"),
        &host,
        &[],
    )
    .await;
    assert_eq!(response.status, 400, "body: {}", response.text);
    assert_eq!(dashboard.hub.calls().len(), 0);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_body_is_bounded_by_the_total_budget() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    // Send a valid head, then trickle the body; the total budget must bound the
    // wait instead of buffering an unbounded slow client.
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "POST /api/v1/factory/pause HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: {cookie}\r\nX-Agalma-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(b"{\"format\":\"Factory").await.expect("write part");

    let started = Instant::now();
    let mut buffer = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(8), stream.read_to_end(&mut buffer)).await;
    let elapsed = started.elapsed();
    assert!(read.is_ok(), "server never answered a slow body");
    let response = parse_response(buffer).expect("parse");
    assert_eq!(response.status, 504, "body: {}", response.text);
    assert!(
        elapsed < Duration::from_secs(6),
        "slow body was not bounded: {elapsed:?}"
    );

    dashboard.listener.shutdown().await;
}

// ---------------------------------------------------------------------------
// Sol review regressions: revocation, permission narrowing, cursor identity,
// response identity and strict credentials
// ---------------------------------------------------------------------------

/// The `FactoryRuntime/v1` fixture record for one status name.
fn status_fixture(status: &str) -> Value {
    let statuses: Vec<Value> = serde_json::from_str(STATUSES_JSON).expect("statuses fixture");
    statuses
        .into_iter()
        .find(|value| value["status"] == status)
        .expect("fixture status")
}

/// A well-formed `FactoryControl/v1` receipt for `action_id`.
fn receipt_json(action_id: &str, outcome: FactoryControlOutcome) -> Value {
    let request = FactoryActionRequest {
        format: FACTORY_CONTROL_FORMAT.to_string(),
        operation: "pause".to_string(),
        action_id: action_id.to_string(),
        confirmation: None,
    };
    let request_digest = request.request_digest().expect("digest");
    serde_json::to_value(FactoryControlReceiptResponse {
        format: FACTORY_CONTROL_FORMAT.to_string(),
        result: FactoryControlResult::Receipt,
        receipt: FactoryControlReceiptBody {
            action_id: action_id.to_string(),
            request,
            request_digest,
            outcome,
            reason: None,
            ledger_action_id: format!("ledger:{action_id}"),
            actor: FactoryControlActor::Human,
            recorded_at_unix_ms: ExactU64::from(1_791_820_805_000u64),
        },
        observed_stop: None,
    })
    .expect("receipt serializes")
}

/// A well-formed `FactoryControl/v1` not-found response for `action_id`.
fn not_found_json(action_id: &str) -> Value {
    serde_json::to_value(FactoryControlNotFoundResponse {
        format: FACTORY_CONTROL_FORMAT.to_string(),
        result: FactoryControlResult::NotFound,
        action_id: action_id.to_string(),
        observed_stop: None,
    })
    .expect("not_found serializes")
}

/// Issue one raw byte request (for headers that cannot be a Rust `String`).
async fn raw_bytes_request(addr: SocketAddr, raw: &[u8]) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let _ = stream.write_all(raw).await;
    let bytes = read_tolerant(&mut stream).await;
    parse_lenient(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_are_intersected_with_live_local_control() {
    // The hub reports resume=permitted (Paused fixture), but with no explicit
    // control binding on this process the browser-visible permission must be
    // false: a hub grant alone is never enough.
    let hub = FakeHub::new();
    hub.set_runtime(status_fixture("Paused"));
    let dashboard = dashboard_from(hub, true, None, false).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let snapshot = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(snapshot.status, 200, "body: {}", snapshot.text);
    let body = snapshot.json();
    assert_eq!(
        body["permissions"]["resume"], false,
        "a hub permission was promoted without a live local control binding"
    );
    assert_eq!(body["permissions"]["pause"], false);
    // Session bootstrap requires both the live authority predicate and an
    // actual control binding: an authorized predicate over a read-only client
    // must issue no operator session or CSRF secret.
    assert_eq!(
        snapshot.header("set-cookie"),
        None,
        "an authorized predicate over a read-only client issued a session"
    );
    assert_eq!(
        snapshot.header("x-agalma-csrf"),
        None,
        "an authorized predicate over a read-only client issued a CSRF secret"
    );
    assert_no_secret(&snapshot);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_denies_control_even_with_a_valid_cookie() {
    let hub = FakeHub::new();
    hub.set_runtime(status_fixture("Paused"));
    let dashboard = dashboard_with(hub, true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    // Before revocation the live authority reports the hub permission.
    let before = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(before.status, 200, "body: {}", before.text);
    assert_eq!(before.json()["permissions"]["resume"], true);

    // A valid control applies.
    let ok = raw_post(
        addr,
        "/api/v1/factory/resume",
        &post_headers(&host, &cookie, &csrf, &[]),
        &resume_body("revoke-before", Some("resume")),
    )
    .await;
    assert_eq!(ok.status, 200, "body: {}", ok.text);

    // Revoke; the cookie and CSRF remain syntactically valid but are denied.
    dashboard.authority.revoke();
    let denied = raw_post(
        addr,
        "/api/v1/factory/resume",
        &post_headers(&host, &cookie, &csrf, &[]),
        &resume_body("revoke-after", Some("resume")),
    )
    .await;
    assert_eq!(denied.status, 403, "body: {}", denied.text);
    assert_eq!(dashboard.hub.effect_count("revoke-after"), 0);

    // Reads still work, but no longer bootstrap a session and report false
    // permissions.
    let after = raw_request(addr, "GET", "/api/v1/factory", &host, &[]).await;
    assert_eq!(after.status, 200, "body: {}", after.text);
    assert_eq!(after.header("set-cookie"), None, "revoked GET bootstrapped");
    assert_eq!(after.header("x-agalma-csrf"), None);
    assert_eq!(after.json()["permissions"]["resume"], false);
    assert_no_secret(&after);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_during_blocked_body_dispatches_no_effect() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    let calls_before = dashboard.hub.calls().len();
    let body = pause_body("blocked-body-1", None);

    // Send a complete POST head with the declared length but withhold the body,
    // so the server blocks in body acquisition.
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "POST /api/v1/factory/pause HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: {cookie}\r\nX-Agalma-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Revoke while the body is still blocked, then deliver the rest.
    dashboard.authority.revoke();
    let _ = stream.write_all(&body).await;

    let mut buffer = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(8), stream.read_to_end(&mut buffer)).await;
    assert!(read.is_ok(), "server never answered the blocked body");
    let response = parse_response(buffer).expect("parse response");
    assert_eq!(response.status, 403, "body: {}", response.text);

    // The post-body recheck denied the request before the handler: no control
    // call reached the hub and no effect was applied.
    assert_eq!(
        dashboard.hub.calls().len(),
        calls_before,
        "a body acquired under revocation reached the hub"
    );
    assert_eq!(dashboard.hub.effect_count("blocked-body-1"), 0);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_while_queued_on_the_client_dispatches_no_effect() {
    let hub = FakeHub::new();
    hub.set_delay(Duration::from_millis(600));
    let dashboard = dashboard_with(hub, true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    let calls_before = dashboard.hub.calls().len();

    // The first control holds the serialized client while the hub delays.
    let first = {
        let (host, cookie, csrf) = (host.clone(), cookie.clone(), csrf.clone());
        tokio::spawn(async move {
            raw_post(
                addr,
                "/api/v1/factory/pause",
                &post_headers(&host, &cookie, &csrf, &[]),
                &pause_body("queued-first", None),
            )
            .await
        })
    };
    // Wait until the first control has reached the hub and holds the lock.
    let mut waited = Duration::ZERO;
    while dashboard.hub.calls().len() <= calls_before && waited < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += Duration::from_millis(10);
    }
    assert!(
        dashboard.hub.calls().len() > calls_before,
        "the first control never dispatched"
    );

    // The second control queues behind the first on the client mutex.
    let second = {
        let (host, cookie, csrf) = (host.clone(), cookie.clone(), csrf.clone());
        tokio::spawn(async move {
            raw_post(
                addr,
                "/api/v1/factory/resume",
                &post_headers(&host, &cookie, &csrf, &[]),
                &resume_body("queued-second", Some("resume")),
            )
            .await
        })
    };
    // Let the second acquire admission, read its body and block on the client.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Revoke while the second waits: its post-mutex recheck must deny the effect.
    dashboard.authority.revoke();

    let first_response = first.await.expect("first task");
    assert_eq!(first_response.status, 200, "body: {}", first_response.text);
    let second_response = second.await.expect("second task");
    assert_eq!(second_response.status, 403, "body: {}", second_response.text);
    assert_eq!(
        dashboard.hub.effect_count("queued-second"),
        0,
        "a request queued at revocation dispatched an effect"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_is_percent_decoded_exactly_once() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let cursor: Value = serde_json::from_str(CURSOR_JSON).expect("cursor fixture");
    let token = cursor["encoded"]
        .as_str()
        .expect("encoded cursor")
        .to_string();
    let once = percent_encode(&token);
    let twice = percent_encode(&once);
    assert_ne!(once, token, "the token must need decoding");
    assert_ne!(twice, once);

    let decoded_once = raw_request(
        addr,
        "GET",
        &format!("/api/v1/factory?cursor={once}"),
        &host,
        &[],
    )
    .await;
    assert_eq!(decoded_once.status, 200, "body: {}", decoded_once.text);
    let decoded_twice = raw_request(
        addr,
        "GET",
        &format!("/api/v1/factory?cursor={twice}"),
        &host,
        &[],
    )
    .await;
    assert_eq!(decoded_twice.status, 200, "body: {}", decoded_twice.text);

    let calls = dashboard.hub.calls();
    let cursors: Vec<&str> = calls
        .iter()
        .map(|call| call.input["cursor"].as_str().expect("cursor forwarded"))
        .collect();
    assert_eq!(cursors.len(), 2);
    assert_eq!(
        cursors[0], token,
        "the percent-encoded cursor must be decoded exactly once"
    );
    assert_eq!(
        cursors[1], once,
        "a twice-encoded cursor must be decoded exactly once, not twice"
    );

    // A malformed escape is refused before any RPC.
    let before = dashboard.hub.calls().len();
    let malformed = raw_request(addr, "GET", "/api/v1/factory?cursor=%zz", &host, &[]).await;
    assert_eq!(malformed.status, 400, "body: {}", malformed.text);
    assert_eq!(dashboard.hub.calls().len(), before);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_identity_is_validated() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;

    // Lookup returning another action's receipt is refused.
    dashboard
        .hub
        .force_response("lookup", receipt_json("other-action", FactoryControlOutcome::Applied));
    let lookup = raw_request(
        addr,
        "GET",
        "/api/v1/factory/actions/wanted-action",
        &host,
        &[],
    )
    .await;
    assert_eq!(lookup.status, 400, "body: {}", lookup.text);
    assert_eq!(lookup.error_code().as_deref(), Some("invalid_request"));
    assert_no_secret(&lookup);

    // A control returning another action's receipt is refused.
    dashboard
        .hub
        .force_response("pause", receipt_json("other-action", FactoryControlOutcome::Applied));
    let control = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("wanted-action", None),
    )
    .await;
    assert_eq!(control.status, 400, "body: {}", control.text);
    assert_no_secret(&control);

    // A control answering `not_found` is an inappropriate variant and refused.
    dashboard
        .hub
        .force_response("pause", not_found_json("wanted-action"));
    let not_found = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[]),
        &pause_body("wanted-action", None),
    )
    .await;
    assert_eq!(not_found.status, 400, "body: {}", not_found.text);
    assert_no_secret(&not_found);

    // None of the refused responses applied an effect.
    assert_eq!(dashboard.hub.total_effects(), 0);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_duplicate_credentials_are_refused() {
    let dashboard = dashboard_with(FakeHub::new(), true, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let (cookie, csrf) = bootstrap(addr).await;
    let body = pause_body("cred-op", None);
    let origin = format!("http://{host}");

    // Duplicate Origin on GET is refused (previously reduced to the first).
    let get_dup = raw_request(
        addr,
        "GET",
        "/api/v1/factory",
        &host,
        &[("Origin", &origin), ("Origin", &origin)],
    )
    .await;
    assert_eq!(get_dup.status, 403, "body: {}", get_dup.text);
    assert!(!get_dup.text.contains(&origin));

    // Duplicate Origin on POST is refused even when both values are valid.
    let post_dup = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[("Origin", &origin)]),
        &body,
    )
    .await;
    assert_eq!(post_dup.status, 403, "body: {}", post_dup.text);

    // Duplicate correct CSRF header is refused.
    let dup_csrf = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &cookie, &csrf, &[("X-Agalma-CSRF", &csrf)]),
        &body,
    )
    .await;
    assert_eq!(dup_csrf.status, 403, "body: {}", dup_csrf.text);

    // Two operator cookies (same value) are refused as duplicates.
    let dup_cookie_value = format!("{cookie}; {cookie}");
    let dup_cookie = raw_post(
        addr,
        "/api/v1/factory/pause",
        &post_headers(&host, &dup_cookie_value, &csrf, &[]),
        &body,
    )
    .await;
    assert_eq!(dup_cookie.status, 403, "body: {}", dup_cookie.text);

    // A malformed (non-UTF-8) Origin byte is refused, never silently dropped.
    let raw = {
        let mut raw = format!(
            "GET /api/v1/factory HTTP/1.1\r\nHost: {host}\r\nOrigin: http://"
        )
        .into_bytes();
        raw.extend_from_slice(host.as_bytes());
        raw.extend_from_slice(b"\xFF\r\nConnection: close\r\n\r\n");
        raw
    };
    let malformed = raw_bytes_request(addr, &raw).await;
    assert_eq!(malformed.status, 403, "body: {}", malformed.text);

    assert_eq!(
        dashboard.hub.total_effects(),
        0,
        "a refused credential applied an effect"
    );

    dashboard.listener.shutdown().await;
}
