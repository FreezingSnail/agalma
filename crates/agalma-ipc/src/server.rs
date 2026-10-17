//! Server runtime (decisions D1/D2).
//!
//! Binds a Unix listener, prints one `READY {json}` line to stdout after the
//! bind succeeds, then accepts connections serially. Each connection is served
//! one request at a time (serial MVP). Requests are dispatched by `method`;
//! duplicate request ids return the recorded receipt; unknown methods and
//! protocol-version mismatches are refused.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agalma_contracts::ContractError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::{Request, Response, PROTOCOL_VERSION};
use crate::error::CODE_PROTOCOL;
use crate::frame;

/// Error code returned for an unregistered method.
pub const ERR_UNKNOWN_METHOD: &str = "UnsupportedCapability";

/// A method implementation. Handlers are pure request→response functions over
/// JSON; effectful behaviour lives behind the handler (as the contract does).
pub trait Handler: Send + Sync {
    /// Handle one request's params, returning the result or a contract error.
    fn handle(&self, params: Value) -> Result<Value, ContractError>;
}

impl<F> Handler for F
where
    F: Fn(Value) -> Result<Value, ContractError> + Send + Sync,
{
    fn handle(&self, params: Value) -> Result<Value, ContractError> {
        self(params)
    }
}

/// Identity and capabilities announced in the `READY` handshake line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// API name (e.g. `harness`).
    pub api: String,
    /// API version.
    pub api_version: u32,
    /// Implementation name.
    #[serde(rename = "impl")]
    pub impl_name: String,
    /// Implementation version.
    pub impl_version: String,
    /// Advertised capabilities.
    pub capabilities: BTreeSet<String>,
}

impl ServerInfo {
    /// The single stdout handshake line: `READY {json}`.
    pub fn ready_line(&self) -> String {
        format!(
            "READY {}",
            serde_json::to_string(self).expect("ServerInfo always serializes")
        )
    }
}

/// Receipt-cache scope: a stable client identity. `0` is the framed/embedded
/// server's single implicit client; the Connect transport assigns one per
/// connection so two independent clients with overlapping numeric request ids
/// cannot replay each other's receipts (`agalma-jmr.4`).
pub type ReceiptScope = u64;

/// The framed/embedded single-client scope.
pub const FRAMED_SCOPE: ReceiptScope = 0;

/// A serial IPC server over a Unix socket.
pub struct Server {
    info: ServerInfo,
    handlers: BTreeMap<String, Arc<dyn Handler>>,
    receipts: BTreeMap<(ReceiptScope, u64), Response>,
}

impl Server {
    /// Create a server announcing `info`, with no handlers registered.
    pub fn new(info: ServerInfo) -> Self {
        Server {
            info,
            handlers: BTreeMap::new(),
            receipts: BTreeMap::new(),
        }
    }

    /// The announced server identity.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// Register a handler for `method`.
    pub fn on<F>(&mut self, method: impl Into<String>, handler: F)
    where
        F: Handler + 'static,
    {
        self.handlers.insert(method.into(), Arc::new(handler));
    }

    /// Whether a handler is registered for `method`.
    pub fn has_handler(&self, method: &str) -> bool {
        self.handlers.contains_key(method)
    }

    /// A reference-counted clone of the handler for `method`, if registered.
    ///
    /// HP.4 (`agalma-jmr.4`): the concurrent Connect dispatcher copies this
    /// reference **while holding the server lock**, then releases the lock before
    /// executing the handler. That lets a handler make an external RPC (e.g. call
    /// a spoke that calls back into this same server) without deadlocking on the
    /// shared server mutex.
    pub fn handler(&self, method: &str) -> Option<Arc<dyn Handler>> {
        self.handlers.get(method).cloned()
    }

    /// The recorded receipt for `id` in `scope` ([`FRAMED_SCOPE`] by default).
    pub fn recorded(&self, id: u64) -> Option<Response> {
        self.recorded_scoped(FRAMED_SCOPE, id)
    }

    /// The recorded receipt for `id` in an explicit client `scope`.
    ///
    /// Receipts are **not** shared across scopes: a different client's request id
    /// never replays here, so independent clients with overlapping numeric ids do
    /// not collide.
    pub fn recorded_scoped(&self, scope: ReceiptScope, id: u64) -> Option<Response> {
        self.receipts.get(&(scope, id)).cloned()
    }

    /// Record `response` for `id` in `scope` unless an earlier receipt exists.
    ///
    /// Returns the receipt that is authoritative after the call (the pre-existing
    /// one on a concurrent duplicate, else `response`).
    pub fn record(&mut self, id: u64, response: Response) -> Response {
        self.record_scoped(FRAMED_SCOPE, id, response)
    }

    /// Record `response` for `id` in an explicit client `scope`.
    pub fn record_scoped(
        &mut self,
        scope: ReceiptScope,
        id: u64,
        response: Response,
    ) -> Response {
        self.receipts
            .entry((scope, id))
            .or_insert(response)
            .clone()
    }

    /// Bind the listener, remove any stale socket, and print the `READY` line.
    pub fn bind(&self, socket: &Path) -> std::io::Result<UnixListener> {
        // Remove a stale socket left by a previous run; ignore absence.
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket)?;
        println!("{}", self.info.ready_line());
        std::io::stdout().flush()?;
        Ok(listener)
    }

    /// Bind and serve until the process is stopped.
    pub fn serve(&mut self, socket: &Path) -> std::io::Result<()> {
        let listener = self.bind(socket)?;
        self.accept_loop(listener)
    }

    /// Accept connections serially, serving each one to completion.
    pub fn accept_loop(&mut self, listener: UnixListener) -> std::io::Result<()> {
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    if let Err(e) = self.serve_connection(stream) {
                        eprintln!("agalma-ipc: connection error: {e}");
                    }
                }
                Err(e) => eprintln!("agalma-ipc: accept error: {e}"),
            }
        }
        Ok(())
    }

    /// Serve one connection, one request at a time, until the peer disconnects.
    pub fn serve_connection(&mut self, stream: UnixStream) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = BufWriter::new(stream);

        loop {
            let payload = match frame::read_payload(&mut reader) {
                Ok(Some(payload)) => payload,
                Ok(None) => return Ok(()), // clean disconnect
                Err(e) => {
                    // Framing fault: close the connection without a response.
                    eprintln!("agalma-ipc: frame error, closing connection: {e}");
                    return Ok(());
                }
            };

            let request: Request = match serde_json::from_slice(&payload) {
                Ok(request) => request,
                Err(e) => {
                    // Without a parseable id we cannot reply; close.
                    eprintln!("agalma-ipc: malformed request, closing connection: {e}");
                    return Ok(());
                }
            };

            if request.v != PROTOCOL_VERSION {
                // Version mismatch is refused: reply with a Protocol error, then
                // close. Not recorded as a receipt (the id may be reused).
                let refusal = Response::error(
                    request.id,
                    CODE_PROTOCOL,
                    format!(
                        "unsupported protocol version {} (expected {})",
                        request.v, PROTOCOL_VERSION
                    ),
                );
                let _ = frame::write_value(&mut writer, &refusal);
                let _ = writer.flush();
                return Ok(());
            }

            let response = self.dispatch(&request);
            if let Err(e) = frame::write_value(&mut writer, &response) {
                // Peer likely gone; close the connection.
                eprintln!("agalma-ipc: response write error, closing connection: {e}");
                return Ok(());
            }
            writer.flush()?;
        }
    }

    /// Dispatch one request, recording and replaying receipts by id.
    ///
    /// Public so the ConnectRPC transport (see [`crate::connect`]) can reuse
    /// the exact envelope semantics — receipt replay, unknown-method refusal,
    /// contract-error mapping — over a different wire.
    ///
    /// Receipts are global (keyed by request id only) and intentionally outlive
    /// connections: explicit-id replay must survive a reconnect. Auto ids are
    /// connection-unique on the client side (see [`crate::client`] module docs),
    /// so a fresh connection never collides with a prior one's recorded ids.
    pub fn dispatch(&mut self, request: &Request) -> Response {
        self.dispatch_scoped(FRAMED_SCOPE, request)
    }

    /// Dispatch one request within an explicit client `scope`, recording and
    /// replaying that scope's receipts. The framed server uses
    /// [`FRAMED_SCOPE`]; the Connect dispatcher uses one scope per connection.
    pub fn dispatch_scoped(&mut self, scope: ReceiptScope, request: &Request) -> Response {
        if let Some(recorded) = self.recorded_scoped(scope, request.id) {
            return recorded;
        }

        let outcome = self
            .handlers
            .get(&request.method)
            .map(|handler| handler.handle(request.params.clone()));

        let response = match outcome {
            Some(Ok(result)) => Response::ok(request.id, result),
            Some(Err(error)) => Response::from_contract_error(request.id, &error),
            None => Response::error(
                request.id,
                ERR_UNKNOWN_METHOD,
                format!("unknown method: {}", request.method),
            ),
        };

        self.record_scoped(scope, request.id, response)
    }
}

/// Parse `--socket <path>` / `--socket=<path>` from an argument iterator.
pub fn socket_from_args<I>(args: I) -> Result<PathBuf, String>
where
    I: IntoIterator<Item = String>,
{
    let mut it = args.into_iter();
    let mut socket: Option<PathBuf> = None;
    while let Some(arg) = it.next() {
        if arg == "--socket" {
            let value = it
                .next()
                .ok_or_else(|| "--socket requires a path argument".to_string())?;
            socket = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(value));
        }
    }
    socket.ok_or_else(|| "missing required --socket <path>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info() -> ServerInfo {
        ServerInfo {
            api: "test-api".into(),
            api_version: 1,
            impl_name: "test-impl".into(),
            impl_version: "0.0.1".into(),
            capabilities: BTreeSet::from(["echo".to_string()]),
        }
    }

    #[test]
    fn ready_line_is_single_json_line() {
        let line = info().ready_line();
        assert!(line.starts_with("READY "));
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line["READY ".len()..]).unwrap();
        assert_eq!(
            value,
            json!({
                "api": "test-api",
                "api_version": 1,
                "impl": "test-impl",
                "impl_version": "0.0.1",
                "capabilities": ["echo"],
            })
        );
    }

    #[test]
    fn dispatch_replays_duplicate_ids() {
        let mut server = Server::new(info());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = calls.clone();
        server.on("count", move |_params: Value| {
            let n = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            Ok(json!({ "n": n }))
        });

        let first = server.dispatch(&Request::new(1, "count", json!({})));
        assert_eq!(first.result_value(), Some(json!({ "n": 1 })));
        // Same id, different params: replay, no second effect.
        let replay = server.dispatch(&Request::new(1, "count", json!({ "ignored": true })));
        assert_eq!(replay.result_value(), Some(json!({ "n": 1 })));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let next = server.dispatch(&Request::new(2, "count", json!({})));
        assert_eq!(next.result_value(), Some(json!({ "n": 2 })));
    }

    #[test]
    fn dispatch_unknown_method_is_unsupported_capability() {
        let mut server = Server::new(info());
        let response = server.dispatch(&Request::new(1, "nope", json!({})));
        assert_eq!(response.error.unwrap().code, ERR_UNKNOWN_METHOD);
    }

    #[test]
    fn socket_arg_parsing() {
        assert_eq!(
            socket_from_args(["--socket".into(), "/run/x.sock".into()]).unwrap(),
            PathBuf::from("/run/x.sock")
        );
        assert_eq!(
            socket_from_args(["--socket=/run/y.sock".into()]).unwrap(),
            PathBuf::from("/run/y.sock")
        );
        assert!(socket_from_args(["--socket".into()]).is_err());
        assert!(socket_from_args(Vec::<String>::new()).is_err());
    }
}
