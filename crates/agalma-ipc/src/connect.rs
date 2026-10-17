//! ConnectRPC envelope transport over a Unix domain socket (design D1/D2).
//!
//! This module is the IPCX foundation: it moves the existing envelope contract
//! ([`crate::envelope`], [`crate::server`]) onto a ConnectRPC `Call` RPC over
//! HTTP/2 on a Unix socket, without changing the envelope semantics.
//!
//! - [`serve`] binds a [`tokio::net::UnixListener`] and hands each accepted
//!   connection to hyper's connection driver (the documented "listen on a Unix
//!   socket" path alongside `connectrpc::server`, which only accepts TCP).
//! - [`ConnectClient`] connects with
//!   [`Http2ConnectionBuilder::connect_unix`](connectrpc::client::Http2ConnectionBuilder::connect_unix)
//!   and speaks the Connect protocol.
//!
//! Envelope mapping is verbatim: `v`/`id`/`method`/`params_json` carry the same
//! fields as the framed transport; duplicate ids replay the server's recorded
//! receipt; in-band `IpcError.code` carries the frozen `ContractError` variant
//! names (including `UnknownOutcome`); transport faults surface as
//! [`IpcError::Unavailable`] and protocol faults as [`IpcError::Protocol`].

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use connectrpc::client::{ClientConfig, SharedHttp2Connection};
use connectrpc::{
    ConnectError, ConnectRpcService, ErrorCode, RequestContext, ServiceRequest, ServiceResult,
};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tower::Service as _;

use crate::client::seed_next_id;
use crate::envelope::{Response, PROTOCOL_VERSION};
use crate::error::{IpcError, CODE_PROTOCOL};
use crate::proto::agalma::ipc::v1::{
    Ipc, IpcClient, IpcError as ProtoError, IpcRequest, IpcResponse, IpcServer,
};
use crate::server::{Handler, ReceiptScope, Server};

/// Authority used as the HTTP/2 `:authority` pseudo-header for the local
/// socket. Ignored for routing (the socket path selects the peer); the Connect
/// path always begins with `agalma.ipc.v1.Ipc/Call`.
const AUTHORITY: &str = "http://agalma.local";

/// Request header carrying a client's stable logical identity.
///
/// Receipt replay is scoped by this value (a transport header, not an envelope
/// field — the frozen envelope is unchanged). Independent clients use distinct
/// identities and never collide on overlapping numeric request ids; a client
/// that reconnects and reuses its identity still replays its own receipts.
pub const CLIENT_ID_HEADER: &str = "x-agalma-client";

// ---------------------------------------------------------------------------
// Wire selector
// ---------------------------------------------------------------------------

/// Wire transport backing a process binding during the ConnectRPC migration
/// (IPCX design D7).
///
/// Both wires speak the *same* envelope ([`crate::envelope`]); only the byte
/// transport differs. `Framed` is the B1 length-prefixed JSON transport
/// ([`crate::server`]/[`crate::client`]); `Connect` is the IPCX HTTP/2
/// Connect transport ([`serve`]/[`ConnectClient`]). The per-boundary selector
/// lets a parity run compare both against the same scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// Length-prefixed JSON frames over a Unix socket (B1 D1).
    Framed,
    /// ConnectRPC `Call` over HTTP/2 on a Unix socket (IPCX D1).
    Connect,
}

/// Environment variable selecting a process binding's wire transport
/// (`framed` | `connect`). Absent or unrecognized uses the caller's default.
pub const WIRE_ENV: &str = "AGALMA_IPC_WIRE";

/// Environment variable set by the supervisor on a supervised component
/// server. When present, the server watches its stdin for EOF and exits when
/// the supervisor closes it — i.e. when the conductor dies (even on `SIGKILL`),
/// so a supervised server never outlives its supervisor. Absent (a raw/embedded
/// serve) keeps the historical blocking serve with no stdin watch.
pub const PARENT_DEATH_STDIN_ENV: &str = "AGALMA_SERVER_PARENT_DEATH_STDIN";

impl Wire {
    /// Stable selector string (the [`WIRE_ENV`] value).
    pub fn as_str(self) -> &'static str {
        match self {
            Wire::Framed => "framed",
            Wire::Connect => "connect",
        }
    }

    /// Parse a selector string; an unknown value is `None`.
    pub fn parse(value: &str) -> Option<Wire> {
        match value {
            "framed" => Some(Wire::Framed),
            "connect" => Some(Wire::Connect),
            _ => None,
        }
    }

    /// Read [`WIRE_ENV`]; absent or unrecognized returns `default`.
    pub fn from_env(default: Wire) -> Wire {
        std::env::var(WIRE_ENV)
            .ok()
            .and_then(|value| Wire::parse(&value))
            .unwrap_or(default)
    }
}

/// Serve `server` over `wire` until stopped.
///
/// The framed branch runs the B1 serial accept loop; the Connect branch runs
/// the IPCX UDS accept loop. Both print the same single `READY {json}` line, so
/// the supervisor's handshake and bind gate are transport-agnostic. A
/// multithread Tokio runtime is installed for both branches because
/// server-hosted implementations (e.g. the OpenCode harness) may need a reactor.
pub fn serve_wire(mut server: Server, socket: &Path, wire: Wire) -> std::io::Result<()> {
    watch_stdin_for_parent_death();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| std::io::Error::other(format!("tokio runtime: {e}")))?;
    match wire {
        Wire::Framed => runtime.block_on(async move { server.serve(socket) }),
        Wire::Connect => runtime.block_on(serve(server, socket)),
    }
}

/// Exit the process when the supervisor closes the server's stdin.
///
/// Active only when [`PARENT_DEATH_STDIN_ENV`] is set (the supervisor spawns
/// the server with a piped stdin it holds open for the server's lifetime). On
/// supervisor death the OS closes that write end; this thread observes EOF and
/// exits, so an orphaned server cannot outlive a `SIGKILL`ed conductor and keep
/// a state lock (or leak) after it.
fn watch_stdin_for_parent_death() {
    if std::env::var_os(PARENT_DEATH_STDIN_ENV).is_none() {
        return;
    }
    std::thread::spawn(|| {
        use std::io::Read;
        let mut byte = [0u8; 1];
        loop {
            match std::io::stdin().read(&mut byte) {
                // EOF (or a read error) means the supervisor is gone.
                Ok(0) | Err(_) => std::process::exit(0),
                // Ignore any input; the pipe exists only as a liveness channel.
                Ok(_) => {}
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Dispatches ConnectRPC `Call` requests into the shared [`Server`].
///
/// The server's handler map and receipt table live behind one mutex, preserving
/// the envelope ordering guarantee (one mutating call at a time per boundary)
/// while HTTP/2 multiplexes the transport.
pub struct IpcDispatcher {
    state: Arc<Mutex<DispatchState>>,
    /// Client identity → receipt scope interner.
    ///
    /// A client identifies itself with the [`CLIENT_ID_HEADER`] transport
    /// header. Requests without it share the framed scope. Distinct client
    /// identities never share receipts, so two independent clients drawing
    /// overlapping numeric request ids do not collide; a client that reconnects
    /// and reuses its identity still replays its own recorded receipts.
    scopes: Arc<Mutex<BTreeMap<String, ReceiptScope>>>,
    next_scope: Arc<std::sync::atomic::AtomicU64>,
}

/// Shared dispatch state: the receipt/handler server plus the atomic in-flight
/// claim table, under one lock so the replay/claim check is atomic.
struct DispatchState {
    server: Server,
    /// `(client scope, request id)` → completion signal for the one in-flight
    /// execution of that request. A concurrent duplicate waits on this and
    /// replays the outcome instead of re-running the handler.
    inflight: BTreeMap<(ReceiptScope, u64), tokio::sync::watch::Sender<bool>>,
}

/// What the atomic claim phase decided for one request.
enum Claim {
    /// A recorded receipt already exists: replay it.
    Replay(Response),
    /// Another task owns the in-flight execution: await its completion signal.
    Wait(tokio::sync::watch::Sender<bool>),
    /// This task owns the claim; run the handler (if any).
    Run(Option<Arc<dyn Handler>>),
}

impl IpcDispatcher {
    /// Wrap `server` behind the ConnectRPC dispatcher.
    pub fn new(server: Server) -> Self {
        IpcDispatcher {
            state: Arc::new(Mutex::new(DispatchState {
                server,
                inflight: BTreeMap::new(),
            })),
            scopes: Arc::new(Mutex::new(BTreeMap::new())),
            next_scope: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// The receipt scope for `client_id` (interning a new scope on first sight).
    fn scope_for(&self, client_id: Option<&str>) -> ReceiptScope {
        let Some(client_id) = client_id else {
            return crate::server::FRAMED_SCOPE;
        };
        let mut scopes = self.scopes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(scope) = scopes.get(client_id) {
            return *scope;
        }
        let scope = self
            .next_scope
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        scopes.insert(client_id.to_string(), scope);
        scope
    }

    /// Atomically claim `(scope, id)` under one lock: replay a recorded receipt,
    /// wait on an existing in-flight claim, or take the claim for this task.
    fn claim(&self, scope: ReceiptScope, id: u64, method: &str) -> Claim {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(recorded) = state.server.recorded_scoped(scope, id) {
            return Claim::Replay(recorded);
        }
        if let Some(signal) = state.inflight.get(&(scope, id)) {
            return Claim::Wait(signal.clone());
        }
        let handler = state.server.handler(method);
        let (signal, _receiver) = tokio::sync::watch::channel(false);
        state.inflight.insert((scope, id), signal);
        Claim::Run(handler)
    }

    /// Record the terminal response and wake any waiters, under the lock.
    fn settle(&self, scope: ReceiptScope, id: u64, response: Response) -> Response {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let recorded = state.server.record_scoped(scope, id, response);
        if let Some(signal) = state.inflight.remove(&(scope, id)) {
            let _ = signal.send(true);
        }
        recorded
    }

    /// Map one envelope request to a response, preserving all envelope
    /// semantics (version refusal, receipt replay, unknown-method refusal).
    ///
    /// HP.4 (`agalma-jmr.4`): the shared lock is held only to atomically claim
    /// the request (replay check + in-flight claim + handler reference) and again
    /// to settle it. It is **released while the handler runs**, so a handler may
    /// make an external RPC (a spoke calling back into this hub) without
    /// deadlocking on the shared lock. A concurrent duplicate of the same client
    /// identity and request id waits on the claim and replays the outcome rather
    /// than re-running the effect.
    async fn handle(&self, scope: ReceiptScope, request: IpcRequest) -> IpcResponse {
        // Version mismatch is refused in-band and never recorded (the id may be
        // reused), mirroring `Server::serve_connection`.
        if request.v != PROTOCOL_VERSION {
            return error_proto(
                request.id,
                CODE_PROTOCOL,
                format!(
                    "unsupported protocol version {} (expected {})",
                    request.v, PROTOCOL_VERSION
                ),
            );
        }

        let mut params = match serde_json::from_slice::<Value>(&request.params_json) {
            Ok(params) => Some(params),
            Err(e) => {
                return error_proto(
                    request.id,
                    CODE_PROTOCOL,
                    format!("malformed params for id {}: {e}", request.id),
                );
            }
        };

        let id = request.id;
        let method = request.method;
        loop {
            match self.claim(scope, id, &method) {
                Claim::Replay(recorded) => return response_to_proto(&recorded),
                Claim::Wait(signal) => {
                    // Subscribe and, if the claim already completed, do not wait
                    // for a second edge (watch receivers see the current value).
                    let mut receiver = signal.subscribe();
                    if !*receiver.borrow() {
                        let _ = receiver.changed().await;
                    }
                    // Loop: the receipt is now recorded (settle records before
                    // waking), or another claim was taken.
                }
                Claim::Run(handler) => {
                    let response = match handler {
                        Some(handler) => {
                            let params = params.take().unwrap_or(Value::Null);
                            let joined =
                                tokio::task::spawn_blocking(move || handler.handle(params)).await;
                            match joined {
                                Ok(Ok(result)) => Response::ok(id, result),
                                Ok(Err(error)) => Response::from_contract_error(id, &error),
                                Err(join_error) => Response::error(
                                    id,
                                    CODE_PROTOCOL,
                                    format!("handler for id {id} panicked: {join_error}"),
                                ),
                            }
                        }
                        None => Response::error(
                            id,
                            crate::server::ERR_UNKNOWN_METHOD,
                            format!("unknown method: {method}"),
                        ),
                    };
                    // Settle records the receipt before waking waiters, so a
                    // waiter that observes completion always finds it recorded.
                    let recorded = self.settle(scope, id, response);
                    return response_to_proto(&recorded);
                }
            }
        }
    }
}

impl Ipc for IpcDispatcher {
    fn call<'a>(
        &'a self,
        ctx: RequestContext,
        request: ServiceRequest<'_, IpcRequest>,
    ) -> impl std::future::Future<
        Output = ServiceResult<impl connectrpc::Encodable<IpcResponse> + Send + use<'a>>,
    > + Send {
        let scope = self.scope_for(
            ctx.header(CLIENT_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
        );
        let request = request.to_owned_message();
        async move { connectrpc::Response::ok(self.handle(scope, request).await) }
    }
}

/// Bind `socket`, announce `READY`, and serve ConnectRPC connections until the
/// process is stopped.
///
/// Each accepted connection is driven on its own task, so HTTP/2 streams
/// multiplex across the socket; the shared [`Server`] guard serializes
/// dispatch.
pub async fn serve(server: Server, socket: &Path) -> std::io::Result<()> {
    let ready = server.info().ready_line();
    let dispatcher = IpcDispatcher::new(server);
    let service = ConnectRpcService::new(IpcServer::new(dispatcher));

    let _ = std::fs::remove_file(socket);
    let listener = tokio::net::UnixListener::bind(socket)?;

    println!("{ready}");
    std::io::stdout().flush()?;

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                eprintln!("agalma-ipc: connect accept error: {e}");
                continue;
            }
        };
        let service = service.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let mut service = service.clone();
                async move { service.call(req).await }
            });
            let mut builder = AutoBuilder::new(TokioExecutor::new());
            builder.http1().timer(TokioTimer::new());
            builder.http2().timer(TokioTimer::new());
            let io = TokioIo::new(stream);
            if let Err(e) = builder.serve_connection(io, svc).into_owned().await {
                // Peer reset/disconnect is normal; log at most.
                eprintln!("agalma-ipc: connect connection error: {e}");
            }
        });
    }
}

/// Encode a response envelope as the ConnectRPC `IpcResponse` message.
fn response_to_proto(response: &Response) -> IpcResponse {
    let result_json = response.result.as_ref().map(|inner| {
        serde_json::to_vec(inner.as_ref().unwrap_or(&Value::Null))
            .expect("response result always serializes")
    });
    let error = response.error.as_ref().map(|e| ProtoError {
        code: e.code.clone(),
        message: e.message.clone(),
        ..Default::default()
    });
    IpcResponse {
        v: response.v,
        id: response.id,
        result_json,
        error: error.into(),
        ..Default::default()
    }
}

/// Build an in-band error response (no receipt recorded).
fn error_proto(id: u64, code: &str, message: String) -> IpcResponse {
    IpcResponse {
        v: PROTOCOL_VERSION,
        id,
        result_json: None,
        error: Some(ProtoError {
            code: code.to_string(),
            message,
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A ConnectRPC envelope client over a Unix socket.
///
/// Mirrors [`crate::client::Client`]: auto ids are connection-unique
/// (band-seeded per connection), explicit ids replay the server's recorded
/// receipt, and connection losses classify as `Unavailable` on connect and as
/// `UnknownOutcome` in-band.
pub struct ConnectClient {
    client: IpcClient<SharedHttp2Connection>,
    next_id: u64,
}

impl ConnectClient {
    /// Connect to the server listening on `socket` and speak Connect.
    ///
    /// The client presents a fresh logical identity, so its receipts are isolated
    /// from every other client. Use [`ConnectClient::connect_as`] to resume the
    /// same identity (and its recorded receipts) after a reconnect.
    pub async fn connect(socket: &Path) -> Result<ConnectClient, IpcError> {
        ConnectClient::connect_as(socket, fresh_client_id()).await
    }

    /// Connect with a caller-chosen stable logical identity.
    ///
    /// Reusing the same `client_id` across reconnects preserves that logical
    /// client's receipt replay; a different id is a different client and never
    /// replays another's receipts. The id travels in the [`CLIENT_ID_HEADER`]
    /// transport header, so the frozen envelope is unchanged.
    pub async fn connect_as(
        socket: &Path,
        client_id: impl Into<String>,
    ) -> Result<ConnectClient, IpcError> {
        let authority: http::Uri = AUTHORITY.parse().expect("static authority is a valid URI");
        let connection = connectrpc::client::Http2Connection::builder()
            .connect_unix(socket, authority.clone())
            .await
            .map_err(|e| IpcError::unavailable(format!("connect to {}: {e}", socket.display())))?;
        let config = ClientConfig::new(authority)
            .with_protocol(connectrpc::Protocol::Connect)
            .with_default_header(CLIENT_ID_HEADER, client_id.into());
        Ok(ConnectClient {
            client: IpcClient::new(connection.shared(1), config),
            next_id: seed_next_id(),
        })
    }

    /// The next auto-allocated request id (connection-unique).
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Call `method` with `params`, assigning a fresh request id.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        self.call_with_id(id, method, params).await
    }

    /// Call `method` with an explicit request `id` (for replay after a
    /// connection loss). Reusing an id returns the server's recorded receipt
    /// rather than re-running the effect.
    ///
    /// Takes `&self` so concurrent calls can multiplex one connection.
    pub async fn call_with_id(
        &self,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, IpcError> {
        let params_json = serde_json::to_vec(&params)
            .map_err(|e| IpcError::protocol(format!("encode params: {e}")))?;
        let request = IpcRequest {
            v: PROTOCOL_VERSION,
            id,
            method: method.to_string(),
            params_json,
            ..Default::default()
        };

        let response = self
            .client
            .call(request)
            .await
            .map_err(|e| map_connect_error(id, e))?
            .into_owned();

        if response.id != id {
            return Err(IpcError::protocol(format!(
                "response id {} does not match request id {id}",
                response.id
            )));
        }
        if response.v != PROTOCOL_VERSION {
            return Err(IpcError::protocol(format!(
                "response protocol version {} unsupported (expected {})",
                response.v, PROTOCOL_VERSION
            )));
        }

        if let Some(error) = response.error.as_option() {
            return Err(IpcError::from_wire(&error.code, &error.message));
        }
        match &response.result_json {
            Some(bytes) => serde_json::from_slice(bytes)
                .map_err(|e| IpcError::protocol(format!("malformed result for id {id}: {e}"))),
            None => Err(IpcError::protocol(format!(
                "response id {id} carried neither result nor error"
            ))),
        }
    }

    /// Call `method` with `params` (auto id) and decode the JSON result into `R`.
    pub async fn call_typed<P, R>(&mut self, method: &str, params: &P) -> Result<R, IpcError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let value = serde_json::to_value(params)
            .map_err(|e| IpcError::protocol(format!("encode params: {e}")))?;
        let result = self.call(method, value).await?;
        serde_json::from_value(result)
            .map_err(|e| IpcError::protocol(format!("decode result: {e}")))
    }
}

/// A fresh, process-unique logical client id.
fn fresh_client_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("c{:016x}{:016x}", seed_next_id(), n)
}

/// Classify a ConnectRPC transport error as an [`IpcError`].
///
/// The in-band envelope path never produces a `ConnectError`: contract and
/// protocol failures travel inside `IpcResponse.error`. A `ConnectError`
/// therefore signals a transport/protocol fault only (design D5): missing
/// socket and other unavailability map to `Unavailable`; everything else is a
/// `Protocol` violation.
fn map_connect_error(id: u64, error: ConnectError) -> IpcError {
    match error.code {
        ErrorCode::Unavailable | ErrorCode::DeadlineExceeded => {
            IpcError::unavailable(format!("connect transport for request id {id}: {error}"))
        }
        _ => IpcError::protocol(format!(
            "connect protocol fault for request id {id}: {error}"
        )),
    }
}
