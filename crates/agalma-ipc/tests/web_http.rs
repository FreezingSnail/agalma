//! Real loopback acceptance for the WEB.3 read transport (`agalma-rwn.3`).
//!
//! A real `127.0.0.1` listener is driven by a raw HTTP/1.1 loopback client. A
//! real Connect/UDS hub (the existing locked IPC stack) serves the
//! `control-plane.read/v1` capability and records every call, so the test can
//! prove the transport issues only read observations, forwards only the
//! server-side grant, and never discloses a credential to the browser.
//!
//! Required command:
//! `cargo test --offline -p agalma-ipc --test web_http`. No case is `ignore`d.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::control_plane::{
    BrowserWire, ExactU64, ListComponents, ReadArtifactChunk, ReadArtifactChunkRequest,
    ReadComponent, ReadCursor, ReadOverview, ReadPageRequest, CONTROL_PLANE_READ_CAPABILITY,
};
use agalma_contracts::ContractError;
use agalma_ipc::connect::Wire;
use agalma_ipc::plugin_server::{register_hub, HubHandler};
use agalma_ipc::server::Server;
use agalma_ipc::web_http::{MAX_OUTSTANDING_READS, READ_DEADLINE as WEB_READ_DEADLINE};
use agalma_ipc::{
    serve_wire, ControlPlaneReadClient, ListenerConfig, LoopbackListener, WebApp, WebRequest,
    WebResponse,
};
use agalma_testkit::ScratchDir;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

/// The scoped grant the server process holds. It must never reach a browser.
const GRANT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// The plugin id the read client declares to the hub.
const PLUGIN_ID: &str = "agalma-web-dashboard";
/// Its binding generation.
const GENERATION: u64 = 7;

const OVERVIEW_JSON: &str = include_str!("../../../fixtures/control-plane-read-v1/overview.json");
const COMPONENT_JSON: &str =
    include_str!("../../../fixtures/control-plane-read-v1/component-page.json");
const COMPONENTS_PAGE_JSON: &str =
    include_str!("../../../fixtures/control-plane-read-v1/components-page.json");
const ARTIFACT_JSON: &str =
    include_str!("../../../fixtures/control-plane-read-v1/artifact-chunk.json");
const WIRE_IDENTITIES_GOLDEN: &str =
    include_str!("../../../fixtures/control-plane-read-v1/wire-identities-golden.json");

/// The artifact digest in `ARTIFACT_JSON`.
const ARTIFACT_DIGEST: &str =
    "sha256:5f78c33274e43fa9de5659265c1d917e25c03722dcb0b8d27db8d5feaa813953";
/// The artifact bytes in `ARTIFACT_JSON` (`deadbeef`).
const ARTIFACT_BYTES: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

/// One recorded `hub.InvokeCapability` call, as the hub saw it.
///
/// `input` is the raw canonical WEB.2 `ControlPlaneReadRequest` JSON, so a test
/// can assert the exact flattened request shape independently of the client.
#[derive(Clone, Debug, Deserialize)]
pub struct SeenCall {
    pub grant: String,
    pub plugin_id: String,
    pub generation: u64,
    pub method: String,
    pub capability: String,
    pub operation_id: String,
    pub input: Value,
    pub deadline_unix_ms: u64,
    pub depth: u32,
}

/// The WEB.2 `ControlPlaneReadRequest` contract, defined independently in the
/// test: snake_case `operation` tag with flattened arguments and unknown fields
/// refused. A client that invents uppercase names or a nested `input` object
/// fails here.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpectedReadRequest {
    ReadOverview,
    ListComponents {
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
    },
    ReadComponent {
        plugin_id: String,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
    },
    ReadArtifactChunk {
        plugin_id: String,
        digest: String,
        #[serde(default)]
        offset: Option<ExactU64>,
        #[serde(default)]
        max_bytes: Option<u64>,
    },
}

/// A fake hub capability router: records every call and returns the real
/// browser-wire projection of the WEB.1 fixture for the requested read
/// operation.
///
/// The `effects` counter is the negative control: the read transport must never
/// reach `hub.PublishReport` or `hub.ReconcileCapability`, so those handlers only
/// count and refuse.
pub struct FakeHub {
    pub seen: Mutex<Vec<SeenCall>>,
    pub effects: AtomicUsize,
    responses: Mutex<BTreeMap<String, Value>>,
    delay: Duration,
}

impl FakeHub {
    pub fn new() -> Self {
        let mut responses = BTreeMap::new();
        responses.insert(
            "read_overview".to_string(),
            wire_fixture::<ReadOverview>(OVERVIEW_JSON),
        );
        responses.insert(
            "list_components".to_string(),
            wire_fixture::<ListComponents>(COMPONENTS_PAGE_JSON),
        );
        responses.insert(
            "read_component".to_string(),
            wire_fixture::<ReadComponent>(COMPONENT_JSON),
        );
        responses.insert(
            "read_artifact_chunk".to_string(),
            wire_fixture::<ReadArtifactChunk>(ARTIFACT_JSON),
        );
        Self {
            seen: Mutex::new(Vec::new()),
            effects: AtomicUsize::new(0),
            responses: Mutex::new(responses),
            delay: Duration::ZERO,
        }
    }

    /// Delay every capability response (a stalled peer for deadline tests).
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// The current wire response for one snake_case operation.
    pub fn response(&self, operation: &str) -> Value {
        self.responses
            .lock()
            .unwrap()
            .get(operation)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Replace the wire response for one snake_case operation.
    pub fn set_response(&self, operation: &str, value: Value) {
        self.responses
            .lock()
            .unwrap()
            .insert(operation.to_string(), value);
    }

    pub fn calls(&self) -> Vec<SeenCall> {
        self.seen.lock().unwrap().clone()
    }

    pub fn effect_calls(&self) -> usize {
        self.effects.load(Ordering::SeqCst)
    }
}

/// Decode a canonical WEB.1 fixture and project it exactly as WEB.2 does.
fn wire_fixture<T: BrowserWire + DeserializeOwned>(json: &str) -> Value {
    let record: T = serde_json::from_str(json).expect("canonical fixture decodes");
    record
        .to_wire_value()
        .expect("fixture projects to browser wire")
}

impl HubHandler for FakeHub {
    fn invoke_capability(&self, request: Value) -> Result<Value, ContractError> {
        let call: SeenCall = serde_json::from_value(request).map_err(|e| {
            ContractError::KnownFailure(format!("fake hub: malformed capability call: {e}"))
        })?;
        // The transport must use the hub method and scoped capability exactly.
        if call.method != "hub.InvokeCapability" {
            return Err(ContractError::KnownFailure(format!(
                "fake hub: unexpected method {}",
                call.method
            )));
        }
        // Independently decode the flattened WEB.2 request contract.
        let _request: ExpectedReadRequest =
            serde_json::from_value(call.input.clone()).map_err(|e| {
                ContractError::KnownFailure(format!("fake hub: not a WEB.2 read request: {e}"))
            })?;
        let operation = call
            .input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ContractError::KnownFailure("fake hub: request has no operation".to_string())
            })?
            .to_string();
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let value = self
            .responses
            .lock()
            .unwrap()
            .get(&operation)
            .cloned()
            .ok_or_else(|| {
                ContractError::UnsupportedCapability(format!(
                    "fake hub: unknown read operation {operation}"
                ))
            })?;
        self.seen.lock().unwrap().push(call);
        Ok(value)
    }

    fn publish_report(&self, _request: Value) -> Result<Value, ContractError> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Err(ContractError::UnsupportedCapability(
            "fake hub: report publication is not a read".to_string(),
        ))
    }

    fn reconcile_capability(&self, _request: Value) -> Result<Value, ContractError> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Err(ContractError::UnsupportedCapability(
            "fake hub: reconciliation is not a read".to_string(),
        ))
    }
}

/// The browser adapter's typed handlers. Each maps a validated request to a
/// typed `ControlPlaneRead` call and emits the browser-wire JSON.
type LockedClient = Arc<tokio::sync::Mutex<ControlPlaneReadClient>>;

type HandlerFuture = Pin<Box<dyn Future<Output = WebResponse> + Send>>;

fn read_overview_response(
    client: LockedClient,
) -> impl Fn(WebRequest) -> HandlerFuture + Send + Sync + 'static {
    move |_request: WebRequest| {
        let client = client.clone();
        Box::pin(async move {
            let mut client = client.lock().await;
            match client.read_overview().await {
                Ok(record) => browser_json(&record),
                Err(error) => read_error(error),
            }
        })
    }
}

fn read_component_response(
    client: LockedClient,
) -> impl Fn(WebRequest) -> HandlerFuture + Send + Sync + 'static {
    move |request: WebRequest| {
        let client = client.clone();
        Box::pin(async move {
            let plugin_id = match request.param("plugin_id") {
                Some(id) => id.to_string(),
                None => {
                    return WebResponse::error(
                        http::StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "missing plugin_id",
                    )
                }
            };
            let page = match page_request(&request) {
                Ok(page) => page,
                Err(response) => return response,
            };
            let mut client = client.lock().await;
            match client.read_component(&plugin_id, &page).await {
                Ok(record) => browser_json(&record),
                Err(error) => read_error(error),
            }
        })
    }
}

fn list_components_response(
    client: LockedClient,
) -> impl Fn(WebRequest) -> HandlerFuture + Send + Sync + 'static {
    move |request: WebRequest| {
        let client = client.clone();
        Box::pin(async move {
            let page = match page_request(&request) {
                Ok(page) => page,
                Err(response) => return response,
            };
            let mut client = client.lock().await;
            match client.list_components(&page).await {
                Ok(record) => browser_json(&record),
                Err(error) => read_error(error),
            }
        })
    }
}

fn read_artifact_response(
    client: LockedClient,
) -> impl Fn(WebRequest) -> HandlerFuture + Send + Sync + 'static {
    move |request: WebRequest| {
        let client = client.clone();
        Box::pin(async move {
            let plugin_id = match request.param("plugin_id") {
                Some(id) => id.to_string(),
                None => {
                    return WebResponse::error(
                        http::StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "missing plugin_id",
                    )
                }
            };
            let digest = match request.param("digest") {
                Some(digest) => digest.to_string(),
                None => {
                    return WebResponse::error(
                        http::StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "missing digest",
                    )
                }
            };
            let offset = match request.query_param("offset") {
                None => None,
                Some(raw) => match ExactU64::parse(raw) {
                    Ok(offset) => Some(offset),
                    Err(error) => {
                        return WebResponse::error(
                            http::StatusCode::BAD_REQUEST,
                            "invalid_request",
                            error.to_string(),
                        )
                    }
                },
            };
            let max_bytes = match request.query_param("max_bytes") {
                None => None,
                Some(raw) => match raw.parse::<u64>() {
                    Ok(max_bytes) => Some(max_bytes),
                    Err(_) => {
                        return WebResponse::error(
                            http::StatusCode::BAD_REQUEST,
                            "invalid_request",
                            "max_bytes is not an integer",
                        )
                    }
                },
            };
            let chunk = ReadArtifactChunkRequest {
                plugin_id,
                digest,
                offset,
                max_bytes,
            };
            let mut client = client.lock().await;
            match client.read_artifact_chunk(&chunk).await {
                Ok(record) => {
                    let bytes = decode_hex(&record.data_hex).unwrap_or_default();
                    WebResponse::attachment("report-artifact.bin", bytes)
                }
                Err(error) => read_error(error),
            }
        })
    }
}

/// Build the browser-wire JSON response for a read record.
fn browser_json<T: BrowserWire>(record: &T) -> WebResponse {
    match record.to_wire_json() {
        Ok(text) => WebResponse::bytes(http::StatusCode::OK, "application/json", text.into_bytes()),
        Err(error) => WebResponse::error(
            http::StatusCode::BAD_GATEWAY,
            "upstream_error",
            error.to_string(),
        ),
    }
}

/// Map a client-side validation failure to `400` and a transport failure to
/// `502`.
fn read_error(error: agalma_ipc::IpcError) -> WebResponse {
    match error {
        agalma_ipc::IpcError::Contract(e) => WebResponse::error(
            http::StatusCode::BAD_REQUEST,
            "invalid_request",
            e.to_string(),
        ),
        other => WebResponse::error(
            http::StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            other.to_string(),
        ),
    }
}

/// Parse `limit`/`cursor` query parameters into a validated page request,
/// returning the explicit `400` response on a malformed value.
fn page_request(request: &WebRequest) -> Result<ReadPageRequest, WebResponse> {
    let limit = match request.query_param("limit") {
        None => None,
        Some(raw) => match raw.parse::<u32>() {
            Ok(limit) => Some(limit),
            Err(_) => {
                return Err(WebResponse::error(
                    http::StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "limit is not an integer",
                ))
            }
        },
    };
    let cursor = request.query_param("cursor").map(str::to_string);
    let request = ReadPageRequest { limit, cursor };
    if let Err(error) = request.effective_limit() {
        return Err(WebResponse::error(
            http::StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        ));
    }
    if let Some(cursor) = &request.cursor {
        if let Err(error) = ReadCursor::decode(cursor) {
            return Err(WebResponse::error(
                http::StatusCode::BAD_REQUEST,
                "invalid_request",
                error.to_string(),
            ));
        }
    }
    Ok(request)
}

/// Decode lowercase hex bytes.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let high = (bytes[index] as char).to_digit(16)? as u8;
        let low = (bytes[index + 1] as char).to_digit(16)? as u8;
        out.push((high << 4) | low);
        index += 2;
    }
    Some(out)
}

/// Assemble the dashboard app around a read client.
fn dashboard_app(config: ListenerConfig, client: LockedClient) -> WebApp {
    WebApp::builder(config)
        .asset(
            "/",
            "text/html; charset=utf-8",
            b"<html>dashboard</html>".to_vec(),
        )
        .asset(
            "/app.js",
            "text/javascript",
            b"console.log('dashboard')".to_vec(),
        )
        .get("/api/v1/overview", read_overview_response(client.clone()))
        .get(
            "/api/v1/components",
            list_components_response(client.clone()),
        )
        .get(
            "/api/v1/components/{plugin_id}",
            read_component_response(client.clone()),
        )
        .get(
            "/api/v1/artifacts/{plugin_id}/{digest}",
            read_artifact_response(client),
        )
        .build()
        .expect("dashboard app builds")
}

/// Spawn the Connect/UDS hub on `socket` serving `hub`.
fn spawn_hub(socket: PathBuf, hub: Arc<FakeHub>) {
    let info =
        agalma_ipc::plugin_server::plugin_server_info("hub", "test-hub", Vec::<String>::new());
    let mut server = Server::new(info);
    register_hub(&mut server, hub);
    std::thread::spawn(move || {
        if let Err(error) = serve_wire(server, &socket, Wire::Connect) {
            eprintln!("web_http test hub stopped: {error}");
        }
    });
}

/// Connect the typed read client once the hub socket accepts.
async fn connect_client(socket: &Path) -> ControlPlaneReadClient {
    for _ in 0..500 {
        if let Ok(client) =
            ControlPlaneReadClient::connect(socket, GRANT, PLUGIN_ID, GENERATION).await
        {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("hub never accepted a connection on {}", socket.display());
}

/// A short repository-local socket root whose physical bytes are retained under
/// the configured (persistent) run root.
///
/// macOS `SUN_LEN` (~104 bytes) overflows for the long worktree/bead paths, so a
/// short symlink under the shared `target/test-runs` points at the retained
/// root, and sockets bind through the short alias.
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
                let candidate = alias_base.join(format!("wh{pid}-{attempt}"));
                match symlink(physical.path(), &candidate) {
                    Ok(()) => Some(candidate),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => panic!("create socket alias {}: {error}", candidate.display()),
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "socket alias allocation exhausted under {}",
                    alias_base.display()
                )
            });
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

/// The running hub, listener and client of one dashboard test.
struct Dashboard {
    listener: LoopbackListener,
    hub: Arc<FakeHub>,
    _root: SocketRoot,
}

async fn dashboard(config: ListenerConfig) -> Dashboard {
    dashboard_with(config, FakeHub::new(), None).await
}

/// Start a dashboard over an explicit fake hub, optionally shortening the read
/// client's RPC deadline.
async fn dashboard_with(
    config: ListenerConfig,
    hub: FakeHub,
    deadline: Option<Duration>,
) -> Dashboard {
    let root = SocketRoot::new("web-http");
    let socket = root.alias.join("hub.sock");
    let hub = Arc::new(hub);
    spawn_hub(socket.clone(), hub.clone());
    let mut client = connect_client(&socket).await;
    if let Some(deadline) = deadline {
        client = client.with_deadline(deadline);
    }
    let client = Arc::new(tokio::sync::Mutex::new(client));
    let app = dashboard_app(config, client);
    let listener = LoopbackListener::start(app)
        .await
        .expect("loopback listener starts");
    Dashboard {
        listener,
        hub,
        _root: root,
    }
}

/// A parsed raw HTTP/1.1 response.
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

    fn error_message(&self) -> Option<String> {
        self.json()
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// Issue one raw HTTP/1.1 request over a fresh loopback TCP connection,
/// reading to EOF (`Connection: close`).
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

/// Split a raw HTTP/1.1 response into status, headers and body.
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

/// The `Host` header value for a real listener address.
fn host_of(addr: SocketAddr) -> String {
    format!("127.0.0.1:{}", addr.port())
}

/// Issue one keep-alive request and read exactly one response.
async fn keep_alive_request(
    stream: &mut TcpStream,
    path: &str,
    host: &str,
) -> Result<RawResponse, String> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| format!("write request: {error}"))?;
    read_one_http_response(stream).await
}

/// Read a single HTTP/1.1 response (headers plus its declared content length).
async fn read_one_http_response(stream: &mut TcpStream) -> Result<RawResponse, String> {
    let mut buffer = Vec::new();
    loop {
        if let Some(split) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..split]).to_string();
            let length = content_length_of(&head).unwrap_or(0);
            if buffer.len() >= split + 4 + length {
                return parse_response(buffer[..split + 4 + length].to_vec());
            }
        }
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .map_err(|_| "read timed out".to_string())?
            .map_err(|error| format!("read response: {error}"))?;
        if read == 0 {
            return Err("connection closed".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// The `Content-Length` of a raw response head, when present.
fn content_length_of(head: &str) -> Option<usize> {
    head.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn assert_error(response: &RawResponse, status: u16, code: &str) {
    assert_eq!(response.status, status, "body: {}", response.text);
    assert_eq!(response.error_code().as_deref(), Some(code));
    assert!(
        !response.error_message().unwrap_or_default().is_empty(),
        "error message is empty"
    );
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_no_credentials(response);
}

fn assert_no_credentials(response: &RawResponse) {
    assert!(
        !response.text.contains(GRANT),
        "response disclosed the hub grant"
    );
    for (_, value) in &response.headers {
        assert!(
            !value.contains(GRANT),
            "response header disclosed the grant"
        );
    }
    assert!(
        !response.text.to_ascii_lowercase().contains("authorization"),
        "response disclosed an authorization header"
    );
}

/// A deterministic linear congruential generator for property cases. The seed
/// is fixed so every failing case can be replayed exactly.
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

/// A gate shared with a slow route: `started` counts admitted handlers and
/// `release` holds every handler until the test opens it.
struct SlowGate {
    started: AtomicUsize,
    release: Semaphore,
}

impl SlowGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: AtomicUsize::new(0),
            release: Semaphore::new(0),
        })
    }

    async fn wait_started(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.started.load(Ordering::SeqCst) < count {
            assert!(
                Instant::now() < deadline,
                "only {} of {count} handlers started",
                self.started.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn slow_handler(gate: Arc<SlowGate>) -> impl Fn(WebRequest) -> HandlerFuture + Send + Sync {
    move |_request: WebRequest| {
        let gate = gate.clone();
        Box::pin(async move {
            gate.started.fetch_add(1, Ordering::SeqCst);
            let _permit = gate.release.acquire().await.expect("gate stays open");
            WebResponse::text(http::StatusCode::OK, "ok")
        })
    }
}

fn download_handler() -> impl Fn(WebRequest) -> HandlerFuture + Send + Sync {
    move |request: WebRequest| {
        let name = request.param("name").unwrap_or("missing").to_string();
        Box::pin(async move { WebResponse::attachment(&name, b"payload".to_vec()) })
    }
}

// ---------------------------------------------------------------------------
// Real listener and typed hub reads
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_serves_assets_and_typed_reads_without_credentials() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    assert!(
        addr.ip().is_loopback(),
        "listener bound a non-loopback address"
    );
    assert_ne!(addr.port(), 0, "ephemeral port was not resolved");
    let host = host_of(addr);

    let root = raw_request(addr, "GET", "/", &host, &[]).await;
    assert_eq!(root.status, 200, "body: {}", root.text);
    assert_eq!(
        root.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(root.header("cache-control"), Some("no-store"));
    assert_eq!(root.body, b"<html>dashboard</html>");
    assert_no_credentials(&root);

    let script = raw_request(addr, "GET", "/app.js", &host, &[]).await;
    assert_eq!(script.status, 200);
    assert_eq!(script.header("content-type"), Some("text/javascript"));
    assert_eq!(script.body, b"console.log('dashboard')");
    assert_no_credentials(&script);

    let overview = raw_request(addr, "GET", "/api/v1/overview", &host, &[]).await;
    assert_eq!(overview.status, 200, "body: {}", overview.text);
    assert_eq!(overview.header("content-type"), Some("application/json"));
    let overview_json = overview.json();
    assert_eq!(
        overview_json["metadata"]["format"], "ControlPlaneRead/v1",
        "typed read wire format"
    );
    assert_eq!(overview_json["factory"]["stopped"], true);
    assert_no_credentials(&overview);

    let components = raw_request(addr, "GET", "/api/v1/components?limit=100", &host, &[]).await;
    assert_eq!(components.status, 200, "body: {}", components.text);
    let components_json = components.json();
    assert_eq!(components_json["metadata"]["format"], "ControlPlaneRead/v1");
    assert!(components_json["entries"].is_array());
    assert_no_credentials(&components);

    let component = raw_request(
        addr,
        "GET",
        "/api/v1/components/example.unfamiliar-reporter",
        &host,
        &[],
    )
    .await;
    assert_eq!(component.status, 200, "body: {}", component.text);
    assert_eq!(
        component.json()["binding"]["binding_id"],
        "fixture.reporter"
    );
    assert_no_credentials(&component);

    let artifact_path = format!("/api/v1/artifacts/example.unfamiliar-reporter/{ARTIFACT_DIGEST}");
    let artifact = raw_request(addr, "GET", &artifact_path, &host, &[]).await;
    assert_eq!(artifact.status, 200, "body: {}", artifact.text);
    assert_eq!(
        artifact.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(artifact.body, ARTIFACT_BYTES);
    assert_no_credentials(&artifact);

    let calls = dashboard.hub.calls();
    assert_eq!(calls.len(), 4, "one read call per route: {calls:?}");
    // The exact canonical WEB.2 request JSON (flattened snake_case operation).
    assert_eq!(calls[0].input, json!({"operation": "read_overview"}));
    assert_eq!(
        calls[1].input,
        json!({"operation": "list_components", "limit": 100, "cursor": null})
    );
    assert_eq!(
        calls[2].input,
        json!({
            "operation": "read_component",
            "plugin_id": "example.unfamiliar-reporter",
            "limit": null,
            "cursor": null
        })
    );
    assert_eq!(
        calls[3].input,
        json!({
            "operation": "read_artifact_chunk",
            "plugin_id": "example.unfamiliar-reporter",
            "digest": ARTIFACT_DIGEST,
            "offset": null,
            "max_bytes": null
        })
    );
    for call in &calls {
        assert!(
            call.input.get("input").is_none(),
            "the invented nested input object must not exist: {}",
            call.input
        );
        let operation = call.input["operation"].as_str().expect("operation string");
        assert!(
            operation
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch == '_'),
            "operation must be snake_case: {operation}"
        );
    }
    let started = now_unix_ms();
    for call in &calls {
        assert_eq!(call.method, "hub.InvokeCapability");
        assert_eq!(call.capability, CONTROL_PLANE_READ_CAPABILITY);
        assert_eq!(call.grant, GRANT, "the server-side grant is used");
        assert_eq!(call.plugin_id, PLUGIN_ID);
        assert_eq!(call.generation, GENERATION);
        assert_eq!(call.depth, 0);
        assert!(
            call.deadline_unix_ms >= started && call.deadline_unix_ms <= started + 5_000,
            "read deadline must be five seconds from now: {} vs {started}",
            call.deadline_unix_ms
        );
    }
    let unique_ids: std::collections::BTreeSet<&str> = calls
        .iter()
        .map(|call| call.operation_id.as_str())
        .collect();
    assert_eq!(unique_ids.len(), calls.len(), "operation ids are unique");
    assert_eq!(
        dashboard.hub.effect_calls(),
        0,
        "no effectful hub method ran"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_match_the_web2_control_plane_read_contract() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    assert_eq!(
        raw_request(addr, "GET", "/api/v1/overview", &host, &[])
            .await
            .status,
        200
    );
    assert_eq!(
        raw_request(addr, "GET", "/api/v1/components?limit=7", &host, &[])
            .await
            .status,
        200
    );
    assert_eq!(
        raw_request(
            addr,
            "GET",
            "/api/v1/components/example.unfamiliar-reporter?limit=3",
            &host,
            &[]
        )
        .await
        .status,
        200
    );
    let artifact = format!(
        "/api/v1/artifacts/example.unfamiliar-reporter/{ARTIFACT_DIGEST}?offset=16&max_bytes=32"
    );
    assert_eq!(
        raw_request(addr, "GET", &artifact, &host, &[]).await.status,
        200
    );

    let calls = dashboard.hub.calls();
    assert_eq!(calls.len(), 4, "one call per read: {calls:?}");
    // Independently asserted canonical WEB.2 request JSON.
    assert_eq!(calls[0].input, json!({"operation": "read_overview"}));
    assert_eq!(
        calls[1].input,
        json!({"operation": "list_components", "limit": 7, "cursor": null})
    );
    assert_eq!(
        calls[2].input,
        json!({
            "operation": "read_component",
            "plugin_id": "example.unfamiliar-reporter",
            "limit": 3,
            "cursor": null
        })
    );
    assert_eq!(
        calls[3].input,
        json!({
            "operation": "read_artifact_chunk",
            "plugin_id": "example.unfamiliar-reporter",
            "digest": ARTIFACT_DIGEST,
            "offset": "16",
            "max_bytes": 32
        })
    );
    for call in &calls {
        assert!(call.input.get("input").is_none(), "{}", call.input);
        let operation = call.input["operation"].as_str().expect("operation");
        assert!(operation
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch == '_'));
    }

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_roundtrips_web1_wire_identity_strings() {
    let hub = FakeHub::new();
    hub.set_response(
        "read_component",
        serde_json::from_str(WIRE_IDENTITIES_GOLDEN).expect("wire identity golden parses"),
    );
    let dashboard = dashboard_with(ListenerConfig::loopback(0), hub, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let response = raw_request(
        addr,
        "GET",
        "/api/v1/components/example.wire-reporter",
        &host,
        &[],
    )
    .await;
    assert_eq!(response.status, 200, "body: {}", response.text);
    assert_no_credentials(&response);

    let expected: Value = serde_json::from_str(WIRE_IDENTITIES_GOLDEN).expect("golden JSON");
    let actual: Value = serde_json::from_slice(&response.body).expect("wire body JSON");
    assert_eq!(
        actual, expected,
        "client must round-trip the WEB.1 wire form"
    );
    // Identities beyond 2^53 stay exact decimal strings.
    assert_eq!(actual["binding"]["generation"], json!("9007199254740993"));
    assert_eq!(
        actual["entries"][0]["binding"]["generation"],
        json!("18446744073709551615")
    );
    assert_eq!(actual["entries"][0]["sequence"], json!("9007199254740993"));
    assert_eq!(
        actual["entries"][0]["reported_at_unix_ms"],
        json!("9223372036854775807")
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_rejects_unsupported_versions_and_oversized_overview() {
    let hub = FakeHub::new();
    let mut unsupported = hub.response("read_overview");
    unsupported["metadata"]["format"] = json!("ControlPlaneRead/v2");
    hub.set_response("read_overview", unsupported);

    let mut bad_envelope: Value =
        serde_json::from_str(WIRE_IDENTITIES_GOLDEN).expect("golden JSON");
    bad_envelope["entries"][0]["format"] = json!("ReportEnvelope/v2");
    hub.set_response("read_component", bad_envelope);

    let dashboard = dashboard_with(ListenerConfig::loopback(0), hub, None).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let versioned = raw_request(addr, "GET", "/api/v1/overview", &host, &[]).await;
    assert_error(&versioned, 400, "invalid_request");
    assert!(
        versioned
            .error_message()
            .unwrap_or_default()
            .contains("UnsupportedVersion"),
        "{}",
        versioned.text
    );

    let oversized_response = {
        let hub = &dashboard.hub;
        let mut oversized = hub.response("read_overview");
        oversized["metadata"]["format"] = json!("ControlPlaneRead/v1");
        oversized["timeline"] = json!([{
            "at_unix_ms": 1,
            "kind": "fixture",
            "title": "oversized",
            "detail": "d".repeat(300 * 1024)
        }]);
        hub.set_response("read_overview", oversized);
        raw_request(addr, "GET", "/api/v1/overview", &host, &[]).await
    };
    assert_error(&oversized_response, 400, "invalid_request");
    assert!(
        oversized_response
            .error_message()
            .unwrap_or_default()
            .contains("exceeding limit"),
        "{}",
        oversized_response.text
    );

    let envelope = raw_request(
        addr,
        "GET",
        "/api/v1/components/example.wire-reporter",
        &host,
        &[],
    )
    .await;
    assert_error(&envelope, 400, "invalid_request");
    assert!(
        envelope
            .error_message()
            .unwrap_or_default()
            .contains("ReportEnvelope"),
        "{}",
        envelope.text
    );

    assert_eq!(dashboard.hub.effect_calls(), 0);
    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_hub_rpc_reports_deadline_exceeded() {
    let hub = FakeHub::new().with_delay(Duration::from_millis(500));
    let dashboard = dashboard_with(
        ListenerConfig::loopback(0),
        hub,
        Some(Duration::from_millis(100)),
    )
    .await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let started = Instant::now();
    let response = raw_request(addr, "GET", "/api/v1/overview", &host, &[]).await;
    let elapsed = started.elapsed();
    assert_error(&response, 400, "invalid_request");
    assert!(
        response
            .error_message()
            .unwrap_or_default()
            .contains("DeadlineExceeded"),
        "{}",
        response.text
    );
    assert!(
        elapsed < Duration::from_millis(450),
        "client did not bound the stalled RPC: {elapsed:?}"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_deadline_setter_caps_at_five_seconds() {
    // A longer requested deadline is clamped: the upstream call still carries
    // at most five seconds from now.
    let dashboard = dashboard_with(
        ListenerConfig::loopback(0),
        FakeHub::new(),
        Some(Duration::from_secs(60)),
    )
    .await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let before = now_unix_ms();
    let response = raw_request(addr, "GET", "/api/v1/overview", &host, &[]).await;
    assert_eq!(response.status, 200, "body: {}", response.text);
    let after = now_unix_ms();

    let calls = dashboard.hub.calls();
    assert_eq!(calls.len(), 1);
    let deadline = calls[0].deadline_unix_ms;
    assert!(
        deadline >= before,
        "deadline {deadline} predates the call at {before}"
    );
    assert!(
        deadline <= after + 5_000,
        "deadline {deadline} exceeds the five-second cap (call ended {after})"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_and_rpc_deadline_helper_bounds_a_stalled_future() {
    // The connection handshake and every RPC use this exact bound. A future
    // that never completes must produce an explicit deadline failure promptly.
    let started = Instant::now();
    let error = agalma_ipc::control_plane_client::with_deadline(
        "control-plane.connect",
        Duration::from_millis(100),
        std::future::pending::<()>(),
    )
    .await
    .expect_err("a stalled future must not complete");
    let elapsed = started.elapsed();
    assert!(
        error.to_string().contains("DeadlineExceeded"),
        "expected an explicit deadline error, got {error}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the bound was not enforced: {elapsed:?}"
    );

    // The bounded connect still reaches a live hub well within the budget.
    let hub = FakeHub::new();
    let root = SocketRoot::new("web-http-connect");
    let socket = root.alias.join("hub.sock");
    let hub = Arc::new(hub);
    spawn_hub(socket.clone(), hub);
    let mut connected = false;
    for _ in 0..200 {
        match ControlPlaneReadClient::connect_with_timeout(
            &socket,
            GRANT,
            PLUGIN_ID,
            GENERATION,
            Duration::from_secs(3),
        )
        .await
        {
            Ok(_) => {
                connected = true;
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    assert!(connected, "a live hub must connect within the bound");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_origin_and_method_gates_reject_before_upstream() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let port = addr.port();

    let localhost = raw_request(
        addr,
        "GET",
        "/api/v1/overview",
        &format!("localhost:{port}"),
        &[],
    )
    .await;
    assert_eq!(localhost.status, 200, "body: {}", localhost.text);

    let baseline = dashboard.hub.calls().len();

    let foreign_host = raw_request(addr, "GET", "/api/v1/overview", "evil.example:4321", &[]).await;
    assert_error(&foreign_host, 403, "forbidden");
    assert_eq!(
        foreign_host.error_message().as_deref(),
        Some("request Host is not the configured local authority")
    );
    assert!(
        !foreign_host.text.contains("evil.example"),
        "the rejected Host must not be reflected"
    );

    let uppercase_host = raw_request(
        addr,
        "GET",
        "/api/v1/overview",
        &format!("LOCALHOST:{port}"),
        &[],
    )
    .await;
    assert_error(&uppercase_host, 403, "forbidden");

    let foreign_origin = raw_request(
        addr,
        "GET",
        "/api/v1/overview",
        &host,
        &[("Origin", "http://evil.example")],
    )
    .await;
    assert_error(&foreign_origin, 403, "forbidden");
    assert_eq!(
        foreign_origin.error_message().as_deref(),
        Some("request Origin is not the configured local origin")
    );
    assert!(!foreign_origin.text.contains("evil.example"));

    let https_origin = raw_request(
        addr,
        "GET",
        "/api/v1/overview",
        &host,
        &[("Origin", "https://127.0.0.1:4321")],
    )
    .await;
    assert_error(&https_origin, 403, "forbidden");

    let wrong_port_origin = raw_request(
        addr,
        "GET",
        "/api/v1/overview",
        &host,
        &[(
            "Origin",
            &format!("http://127.0.0.1:{}", port.wrapping_add(1)),
        )],
    )
    .await;
    assert_error(&wrong_port_origin, 403, "forbidden");

    let allowed_origin = format!("http://{host}");
    let allowed = raw_request(
        addr,
        "GET",
        "/api/v1/overview",
        &host,
        &[("Origin", &allowed_origin)],
    )
    .await;
    assert_eq!(allowed.status, 200, "body: {}", allowed.text);

    let localhost_origin = format!("http://localhost:{port}");
    let allowed_localhost = raw_request(
        addr,
        "GET",
        "/api/v1/overview",
        &host,
        &[("Origin", &localhost_origin)],
    )
    .await;
    assert_eq!(allowed_localhost.status, 200);

    assert_eq!(
        dashboard.hub.calls().len(),
        baseline + 2,
        "only the two admitted origins reached the hub"
    );

    let before_methods = dashboard.hub.calls().len();
    for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
        let response = raw_request(addr, method, "/api/v1/overview", &host, &[]).await;
        assert_error(&response, 405, "method_not_allowed");
        assert_eq!(response.header("allow"), Some("GET"));
        assert_eq!(
            response.error_message().as_deref(),
            Some("only GET is supported")
        );
    }
    let head = raw_request(addr, "HEAD", "/api/v1/overview", &host, &[]).await;
    assert_eq!(head.status, 405);
    assert_eq!(head.header("allow"), Some("GET"));
    assert_eq!(
        dashboard.hub.calls().len(),
        before_methods,
        "rejected methods never reached the hub"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_paths_ids_limits_and_cursors_are_exact() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let baseline = dashboard.hub.calls().len();

    let invalid_requests = [
        ("/api/v1/%", "invalid_request"),
        ("/api/v1/%zz", "invalid_request"),
        ("/api/v1/components/%2Fetc", "invalid_request"),
        ("/api/v1/components/%2fetc", "invalid_request"),
        ("/api/v1/components/%00", "invalid_request"),
        ("/api/v1/components/%0A", "invalid_request"),
        ("/api/v1/components/%5C", "invalid_request"),
        ("/api/v1/artifacts/example/sha256:xyz", "invalid_request"),
        ("/api/v1/components?limit=0", "invalid_request"),
        ("/api/v1/components?limit=101", "invalid_request"),
        ("/api/v1/components?limit=-1", "invalid_request"),
        ("/api/v1/components?limit=abc", "invalid_request"),
        ("/api/v1/components?cursor=not-a-cursor", "invalid_request"),
        ("/api/v1/components?cursor=cp1:", "invalid_request"),
        ("/api/v1/components?cursor=cp1:%7B%7D", "invalid_request"),
    ];
    for (path, code) in invalid_requests {
        let response = raw_request(addr, "GET", path, &host, &[]).await;
        assert_error(&response, 400, code);
        assert_eq!(
            dashboard.hub.calls().len(),
            baseline,
            "malformed request {path} reached the hub"
        );
    }

    for path in ["/api/v1/unknown", "/../secret", "/%2e%2e/secret"] {
        let response = raw_request(addr, "GET", path, &host, &[]).await;
        assert_error(&response, 404, "not_found");
        assert_eq!(dashboard.hub.calls().len(), baseline);
    }

    dashboard.listener.shutdown().await;
}

// ---------------------------------------------------------------------------
// Deterministic property cases for the changed invariants
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_foreign_authorities_and_methods_are_refused() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let port = addr.port();
    let mut rng = Lcg::new(0x5eed_1234_abcd_0001);
    let baseline = dashboard.hub.calls().len();

    for case in 0..32u32 {
        let (request_host, origin) = match case % 4 {
            0 => (format!("evil{case}.example:{case}"), None),
            1 => (
                format!("127.0.0.1:{}", port.wrapping_add(case as u16 + 1)),
                None,
            ),
            2 => (host.clone(), Some(format!("http://evil{case}.example"))),
            _ => (host.clone(), Some(format!("https://{host}"))),
        };
        let extra: Vec<(&str, &str)> = origin
            .as_deref()
            .map(|origin| vec![("Origin", origin)])
            .unwrap_or_default();
        let response = raw_request(addr, "GET", "/api/v1/overview", &request_host, &extra).await;
        assert_error(&response, 403, "forbidden");
        assert!(!response.text.contains(&request_host));
        if let Some(origin) = origin {
            assert!(!response.text.contains(&origin));
        }
        assert_eq!(
            dashboard.hub.calls().len(),
            baseline,
            "foreign authority reached the hub"
        );
        // Keep the generator live so later cases are not folded by the optimizer
        // and the recorded seed replays every branch.
        let _ = rng.next();
    }

    let methods = [
        "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT", "TRACE",
    ];
    for case in 0..42usize {
        let method = methods[(rng.index(methods.len()) + case) % methods.len()];
        let response = raw_request(addr, method, "/api/v1/overview", &host, &[]).await;
        assert_error(&response, 405, "method_not_allowed");
        assert_eq!(response.header("allow"), Some("GET"));
    }
    assert_eq!(dashboard.hub.calls().len(), baseline);

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_page_limits_follow_the_declared_band() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let mut rng = Lcg::new(0x5eed_1234_abcd_0002);

    let mut limits = vec![0u32, 1, 2, 50, 99, 100, 101, 200, u32::MAX];
    for _ in 0..24 {
        limits.push((rng.next() % 400) as u32);
    }

    let mut admitted = 0usize;
    let baseline = dashboard.hub.calls().len();
    for limit in limits {
        let path = format!("/api/v1/components?limit={limit}");
        let response = raw_request(addr, "GET", &path, &host, &[]).await;
        if (1..=100).contains(&limit) {
            assert_eq!(response.status, 200, "limit {limit}: {}", response.text);
            admitted += 1;
        } else {
            assert_error(&response, 400, "invalid_request");
        }
    }
    assert_eq!(
        dashboard.hub.calls().len() - baseline,
        admitted,
        "only in-band limits reached the hub"
    );
    assert_eq!(
        dashboard.hub.effect_calls(),
        0,
        "the read surface never invoked an effect"
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_path_escapes_never_reach_a_route() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);
    let mut rng = Lcg::new(0x5eed_1234_abcd_0003);
    let escapes = ["%2F", "%2f", "%00", "%0A", "%5C"];
    let baseline = dashboard.hub.calls().len();

    for case in 0..30usize {
        let escape = escapes[(rng.index(escapes.len()) + case) % escapes.len()];
        let path = format!("/api/v1/components/id{case}{escape}");
        let response = raw_request(addr, "GET", &path, &host, &[]).await;
        assert_error(&response, 400, "invalid_request");
    }
    assert_eq!(
        dashboard.hub.calls().len(),
        baseline,
        "an escaped path reached the hub"
    );

    for path in ["/..", "/.", "/a/../b", "/a/./b", "/%2e%2e/secret"] {
        let response = raw_request(addr, "GET", path, &host, &[]).await;
        assert_error(&response, 404, "not_found");
    }
    assert_eq!(dashboard.hub.calls().len(), baseline);

    dashboard.listener.shutdown().await;
}

// ---------------------------------------------------------------------------
// Deadline, backpressure, cancellation and drain
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_and_outstanding_read_bounds_hold() {
    assert_eq!(WEB_READ_DEADLINE, Duration::from_secs(5));
    assert_eq!(MAX_OUTSTANDING_READS, 16);
    let defaults = ListenerConfig::loopback(0);
    assert_eq!(defaults.read_deadline_value(), Duration::from_secs(5));
    assert_eq!(defaults.max_outstanding(), 16);

    // Hard caps: the boundary is accepted, one past it is refused.
    assert!(ListenerConfig::loopback(0)
        .max_outstanding_reads(MAX_OUTSTANDING_READS)
        .read_deadline(WEB_READ_DEADLINE)
        .validate()
        .is_ok());
    assert!(ListenerConfig::loopback(0)
        .max_outstanding_reads(MAX_OUTSTANDING_READS + 1)
        .validate()
        .is_err());
    assert!(ListenerConfig::loopback(0)
        .read_deadline(WEB_READ_DEADLINE + Duration::from_millis(1))
        .validate()
        .is_err());
    assert!(WebApp::builder(
        ListenerConfig::loopback(0).max_outstanding_reads(MAX_OUTSTANDING_READS + 1)
    )
    .build()
    .is_err());
    assert!(WebApp::builder(
        ListenerConfig::loopback(0).read_deadline(WEB_READ_DEADLINE + Duration::from_millis(1))
    )
    .build()
    .is_err());

    // Property: for every bound N, N admitted reads hold their slots and the
    // N+1th read receives bounded backpressure instead of queueing.
    for bound in [1usize, 2, 3, 4, MAX_OUTSTANDING_READS] {
        let gate = SlowGate::new();
        let app = WebApp::builder(
            ListenerConfig::loopback(0)
                .read_deadline(Duration::from_secs(3))
                .max_outstanding_reads(bound),
        )
        .get("/slow", slow_handler(gate.clone()))
        .build()
        .expect("slow app builds");
        let listener = LoopbackListener::start(app)
            .await
            .expect("slow listener starts");
        let addr = listener.local_addr();
        let host = host_of(addr);

        let mut tasks = Vec::new();
        for _ in 0..bound {
            let request_host = host.clone();
            tasks.push(tokio::spawn(async move {
                raw_request(addr, "GET", "/slow", &request_host, &[]).await
            }));
        }
        gate.wait_started(bound).await;

        let refused = raw_request(addr, "GET", "/slow", &host, &[]).await;
        assert_error(&refused, 503, "overloaded");
        assert_eq!(
            refused.error_message().as_deref(),
            Some("too many outstanding reads")
        );

        gate.release.add_permits(bound);
        for task in tasks {
            let response = task.await.expect("slow request task");
            assert_eq!(response.status, 200, "body: {}", response.text);
        }
        listener.shutdown().await;
    }

    // A handler that never finishes is bounded by the configured deadline.
    let gate = SlowGate::new();
    let deadline = Duration::from_millis(300);
    let app = WebApp::builder(
        ListenerConfig::loopback(0)
            .read_deadline(deadline)
            .max_outstanding_reads(2),
    )
    .get("/hang", slow_handler(gate.clone()))
    .build()
    .expect("hang app builds");
    let listener = LoopbackListener::start(app)
        .await
        .expect("hang listener starts");
    let addr = listener.local_addr();
    let host = host_of(addr);

    let started = Instant::now();
    let response = raw_request(addr, "GET", "/hang", &host, &[]).await;
    let elapsed = started.elapsed();
    assert_error(&response, 504, "deadline_exceeded");
    assert_eq!(
        response.error_message().as_deref(),
        Some("read deadline exceeded")
    );
    assert!(
        elapsed >= deadline - Duration::from_millis(50),
        "deadline returned early after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "deadline unbounded: {elapsed:?}"
    );
    gate.release.add_permits(1);
    listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_admitted_reads_and_closes() {
    let gate = SlowGate::new();
    let app = WebApp::builder(ListenerConfig::loopback(0).read_deadline(Duration::from_secs(3)))
        .get("/slow", slow_handler(gate.clone()))
        .build()
        .expect("slow app builds");
    let listener = LoopbackListener::start(app).await.expect("listener starts");
    let addr = listener.local_addr();
    let host = host_of(addr);

    let request_host = host.clone();
    let request =
        tokio::spawn(async move { raw_request(addr, "GET", "/slow", &request_host, &[]).await });
    gate.wait_started(1).await;

    let shutdown = tokio::spawn(listener.shutdown());
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Admission is stopped while the still-blocked read drains. The connect may
    // still land in the closed listener's backlog, so bound the probe tightly.
    let during_drain = tokio::time::timeout(
        Duration::from_millis(200),
        try_raw_request(addr, "GET", "/", &host, &[]),
    )
    .await;
    let admitted_during_drain =
        matches!(during_drain, Ok(Ok(ref response)) if response.status == 200);
    assert!(
        !admitted_during_drain,
        "a new read was admitted during drain"
    );

    gate.release.add_permits(1);
    let response = request.await.expect("drained request task");
    assert_eq!(response.status, 200, "admitted read did not drain");
    shutdown.await.expect("shutdown task");

    let after = try_raw_request(addr, "GET", "/", &host, &[]).await;
    assert!(after.is_err(), "listener still accepted after shutdown");
    assert_eq!(gate.started.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_established_keep_alive_connections() {
    let app = WebApp::builder(ListenerConfig::loopback(0))
        .get("/", |_request: WebRequest| async move {
            WebResponse::text(http::StatusCode::OK, "ok")
        })
        .build()
        .expect("keep-alive app builds");
    let listener = LoopbackListener::start(app).await.expect("listener starts");
    let addr = listener.local_addr();
    let host = host_of(addr);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let first = keep_alive_request(&mut stream, "/", &host)
        .await
        .expect("first keep-alive response");
    assert_eq!(first.status, 200);
    assert_eq!(first.body, b"ok");

    let shutdown = tokio::spawn(listener.shutdown());
    tokio::time::sleep(Duration::from_millis(30)).await;

    // The established idle keep-alive must not serve another read once drain
    // has begun; it is closed instead.
    let reuse = tokio::time::timeout(
        Duration::from_secs(2),
        keep_alive_request(&mut stream, "/", &host),
    )
    .await;
    match reuse {
        Ok(Ok(response)) => assert_ne!(
            response.status, 200,
            "idle keep-alive served a read after shutdown began: {}",
            response.text
        ),
        Ok(Err(_)) | Err(_) => {}
    }
    shutdown.await.expect("shutdown task");

    let after = try_raw_request(addr, "GET", "/", &host, &[]).await;
    assert!(after.is_err(), "listener still accepted after shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_client_read_releases_capacity() {
    let gate = SlowGate::new();
    let app = WebApp::builder(
        ListenerConfig::loopback(0)
            .read_deadline(Duration::from_secs(3))
            .max_outstanding_reads(1),
    )
    .get("/slow", slow_handler(gate.clone()))
    .get("/ok", |_request: WebRequest| async move {
        WebResponse::text(http::StatusCode::OK, "ok")
    })
    .build()
    .expect("cancellation app builds");
    let listener = LoopbackListener::start(app).await.expect("listener starts");
    let addr = listener.local_addr();
    let host = host_of(addr);

    let request_host = host.clone();
    let request =
        tokio::spawn(
            async move { try_raw_request(addr, "GET", "/slow", &request_host, &[]).await },
        );
    gate.wait_started(1).await;
    request.abort();
    let _ = request.await;

    // The handler stays blocked forever and is never released. Dropping the
    // client must cancel its future and restore the read slot well before the
    // listener's own three-second read deadline: this fails if capacity only
    // returns when the normal read timeout fires.
    assert_eq!(
        gate.release.available_permits(),
        0,
        "the handler must still be blocked when the client is dropped"
    );
    let started = Instant::now();
    let cancellation_budget = Duration::from_millis(400);
    let deadline = started + cancellation_budget;
    loop {
        let response = raw_request(addr, "GET", "/ok", &host, &[]).await;
        if response.status == 200 {
            break;
        }
        assert_eq!(response.status, 503, "body: {}", response.text);
        assert!(
            Instant::now() < deadline,
            "cancelled read never restored capacity within {cancellation_budget:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        started.elapsed() < cancellation_budget,
        "capacity returned only after the read timeout: {:?}",
        started.elapsed()
    );
    assert_eq!(
        gate.release.available_permits(),
        0,
        "capacity was restored without ever releasing the blocked handler"
    );
    assert_eq!(
        gate.started.load(Ordering::SeqCst),
        1,
        "the blocked handler must not have been re-admitted"
    );

    listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_read_shutdown_is_bounded_by_deadline() {
    let gate = SlowGate::new();
    let deadline = Duration::from_millis(300);
    let app = WebApp::builder(
        ListenerConfig::loopback(0)
            .read_deadline(deadline)
            .max_outstanding_reads(2),
    )
    .get("/hang", slow_handler(gate.clone()))
    .build()
    .expect("hang app builds");
    let listener = LoopbackListener::start(app).await.expect("listener starts");
    let addr = listener.local_addr();
    let host = host_of(addr);

    let request_host = host.clone();
    let request =
        tokio::spawn(
            async move { try_raw_request(addr, "GET", "/hang", &request_host, &[]).await },
        );
    gate.wait_started(1).await;

    let started = Instant::now();
    listener.shutdown().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= deadline - Duration::from_millis(50),
        "shutdown did not wait for the drain window: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown was not bounded by the deadline: {elapsed:?}"
    );

    gate.release.add_permits(1);
    let _ = request.await.expect("stuck request task");
    let after = try_raw_request(addr, "GET", "/", &host, &[]).await;
    assert!(after.is_err(), "listener still accepted after shutdown");
}

// ---------------------------------------------------------------------------
// Artifacts and attachment safety
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_attachments_are_bounded_and_nosniff() {
    let dashboard = dashboard(ListenerConfig::loopback(0)).await;
    let addr = dashboard.listener.local_addr();
    let host = host_of(addr);

    let path = format!("/api/v1/artifacts/example.unfamiliar-reporter/{ARTIFACT_DIGEST}");
    let response = raw_request(addr, "GET", &path, &host, &[]).await;
    assert_eq!(response.status, 200, "body: {}", response.text);
    assert_eq!(
        response.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(
        response.header("content-disposition"),
        Some("attachment; filename=\"report-artifact.bin\"")
    );
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(response.body, ARTIFACT_BYTES);
    assert!(
        response.body.len() as u64 <= 64 * 1024,
        "artifact chunk exceeded the 64 KiB bound"
    );
    assert!(
        !response.body.windows(7).any(|window| window == b"<script"),
        "artifact bytes must not be served as executable content"
    );
    assert_no_credentials(&response);

    let baseline = dashboard.hub.calls().len();
    for path in [
        "/api/v1/artifacts/example/sha256:xyz",
        "/api/v1/artifacts/example/sha256:000000000000000000000000000000000000000000000000000000000000000",
        "/api/v1/artifacts/example",
    ] {
        let response = raw_request(addr, "GET", path, &host, &[]).await;
        let expected = if path == "/api/v1/artifacts/example" {
            404
        } else {
            400
        };
        assert_error(&response, expected, if expected == 404 { "not_found" } else { "invalid_request" });
    }
    assert_eq!(
        dashboard.hub.calls().len(),
        baseline,
        "invalid artifact request reached the hub"
    );

    for query in [
        "?max_bytes=0",
        "?max_bytes=65537",
        "?max_bytes=abc",
        "?offset=x",
        "?offset=-1",
    ] {
        let request_path =
            format!("/api/v1/artifacts/example.unfamiliar-reporter/{ARTIFACT_DIGEST}{query}");
        let response = raw_request(addr, "GET", &request_path, &host, &[]).await;
        assert_error(&response, 400, "invalid_request");
        assert_eq!(dashboard.hub.calls().len(), baseline);
    }

    // The exact chunk bound is admitted; one byte more is already refused.
    let accepted_path = format!(
        "/api/v1/artifacts/example.unfamiliar-reporter/{ARTIFACT_DIGEST}?max_bytes=65536&offset=0"
    );
    let accepted = raw_request(addr, "GET", &accepted_path, &host, &[]).await;
    assert_eq!(accepted.status, 200, "body: {}", accepted.text);
    assert_eq!(accepted.body, ARTIFACT_BYTES);
    assert_eq!(dashboard.hub.calls().len(), baseline + 1);
    assert_eq!(
        dashboard.hub.calls()[baseline].input,
        json!({
            "operation": "read_artifact_chunk",
            "plugin_id": "example.unfamiliar-reporter",
            "digest": ARTIFACT_DIGEST,
            "offset": "0",
            "max_bytes": 65536
        })
    );

    dashboard.listener.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_attachment_filenames_are_sanitized() {
    let app = WebApp::builder(ListenerConfig::loopback(0))
        .get("/download/{name}", download_handler())
        .build()
        .expect("download app builds");
    let listener = LoopbackListener::start(app).await.expect("listener starts");
    let addr = listener.local_addr();
    let host = host_of(addr);
    let mut rng = Lcg::new(0x5eed_1234_abcd_0004);

    let cases = [
        "/download/evil.html",
        "/download/..%5Cevil.html",
        "/download/%22evil%22.html",
        "/download/%3Cscript%3E.html",
        "/download/file%20name.txt",
        "/download/%C3%A9.html",
        "/download/..",
        "/download/a.b-c_d",
    ];
    for case in 0..cases.len().max(16) {
        let path = if case < cases.len() {
            cases[case]
        } else {
            cases[rng.index(cases.len())]
        };
        let response = raw_request(addr, "GET", path, &host, &[]).await;
        assert_eq!(response.status, 200, "body: {}", response.text);
        assert_eq!(
            response.header("content-type"),
            Some("application/octet-stream")
        );
        assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
        let disposition = response
            .header("content-disposition")
            .expect("attachment disposition");
        let filename = disposition
            .strip_prefix("attachment; filename=\"")
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or_else(|| panic!("malformed disposition {disposition:?}"));
        assert!(
            filename
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
            "unsafe filename {filename:?} in {disposition:?}"
        );
        assert!(
            !filename.contains('/') && !filename.contains('\\'),
            "filename crossed a path boundary: {filename:?}"
        );
    }

    listener.shutdown().await;
}
