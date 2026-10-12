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
use serde_json::Value;
use tower::Service as _;

use crate::client::seed_next_id;
use crate::envelope::{Request, Response, PROTOCOL_VERSION};
use crate::error::{IpcError, CODE_PROTOCOL};
use crate::proto::agalma::ipc::v1::{
    Ipc, IpcClient, IpcError as ProtoError, IpcRequest, IpcResponse, IpcServer,
};
use crate::server::Server;

/// Authority used as the HTTP/2 `:authority` pseudo-header for the local
/// socket. Ignored for routing (the socket path selects the peer); the Connect
/// path always begins with `agalma.ipc.v1.Ipc/Call`.
const AUTHORITY: &str = "http://agalma.local";

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Dispatches ConnectRPC `Call` requests into the shared [`Server`].
///
/// The server's handler map and receipt table live behind one mutex, preserving
/// the envelope ordering guarantee (one mutating call at a time per boundary)
/// while HTTP/2 multiplexes the transport.
pub struct IpcDispatcher {
    server: Arc<Mutex<Server>>,
}

impl IpcDispatcher {
    /// Wrap `server` behind the ConnectRPC dispatcher.
    pub fn new(server: Server) -> Self {
        IpcDispatcher {
            server: Arc::new(Mutex::new(server)),
        }
    }

    /// Map one envelope request to a response, preserving all envelope
    /// semantics (version refusal, receipt replay, unknown-method refusal).
    fn handle(&self, request: IpcRequest) -> IpcResponse {
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

        let params: Value = match serde_json::from_slice(&request.params_json) {
            Ok(params) => params,
            Err(e) => {
                return error_proto(
                    request.id,
                    CODE_PROTOCOL,
                    format!("malformed params for id {}: {e}", request.id),
                );
            }
        };

        let envelope = Request {
            v: request.v,
            id: request.id,
            method: request.method,
            params,
        };

        let response = {
            let mut server = self.server.lock().unwrap_or_else(PoisonError::into_inner);
            server.dispatch(&envelope)
        };
        response_to_proto(&response)
    }
}

impl Ipc for IpcDispatcher {
    fn call<'a>(
        &'a self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, IpcRequest>,
    ) -> impl std::future::Future<
        Output = ServiceResult<impl connectrpc::Encodable<IpcResponse> + Send + use<'a>>,
    > + Send {
        let request = request.to_owned_message();
        async move { connectrpc::Response::ok(self.handle(request)) }
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
    let error = response
        .error
        .as_ref()
        .map(|e| ProtoError {
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
    pub async fn connect(socket: &Path) -> Result<ConnectClient, IpcError> {
        let authority: http::Uri = AUTHORITY
            .parse()
            .expect("static authority is a valid URI");
        let connection = connectrpc::client::Http2Connection::builder()
            .connect_unix(socket, authority.clone())
            .await
            .map_err(|e| {
                IpcError::unavailable(format!("connect to {}: {e}", socket.display()))
            })?;
        let config = ClientConfig::new(authority).with_protocol(connectrpc::Protocol::Connect);
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
        ErrorCode::Unavailable | ErrorCode::DeadlineExceeded => IpcError::unavailable(format!(
            "connect transport for request id {id}: {error}"
        )),
        _ => IpcError::protocol(format!("connect protocol fault for request id {id}: {error}")),
    }
}
