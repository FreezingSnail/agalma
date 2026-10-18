//! Reusable loopback HTTP listener/response abstraction for the local browser
//! dashboard (WEB.3, `agalma-rwn.3`).
//!
//! This module is the transport half of the browser adapter described in
//! `docs/web-dashboard-design.md` ("Implementation contract for
//! decomposition"). It keeps every hyper/HTTP detail inside `agalma-ipc`; the
//! dashboard process supplies its asset inventory and typed route handlers and
//! this module owns:
//!
//! - a listener bound to `127.0.0.1` only (a non-loopback configuration is
//!   refused before any socket is created), defaulting to an ephemeral port;
//! - Host and Origin validation against the configured local authority;
//! - GET-only routing (mutation methods are refused with `405`);
//! - bounded request handling: a five-second read deadline and at most sixteen
//!   outstanding reads, with bounded backpressure;
//! - one absolute monotonic deadline per admitted request, propagated through
//!   body acquisition, adapter/client waits, and the hub RPC so a slow stage
//!   cannot grant a fresh budget;
//! - a live, revocable control authority ([`ControlAuthority`]) that is
//!   revalidated for every session bootstrap and every POST, so revocation
//!   denies effects even while an issued session cookie remains valid;
//! - exact, explicit transport errors for unknown routes, unsupported methods,
//!   foreign Host/Origin, malformed paths/IDs/cursors, overload and deadline;
//! - an explicit asset inventory (no filesystem traversal);
//! - safe attachment responses (`Content-Disposition: attachment` and
//!   `X-Content-Type-Options: nosniff`) so plugin HTML/scripts never execute
//!   under the dashboard origin;
//! - clean listener drain on shutdown.
//!
//! The listener never emits credentials: it forwards nothing from a request
//! header or query string into an error body, never reflects `Host`/`Origin`,
//! and its responses carry `Cache-Control: no-store`. A hub grant token lives
//! only in the server process (see [`crate::control_plane_client`]) and is
//! never placed in a browser-facing response.
//!
//! Reading a route is observation only: this module invokes no effect. The
//! typed read client that talks to the hub is
//! [`crate::control_plane_client::ControlPlaneReadClient`].
use std::collections::BTreeMap;
use std::future::{poll_fn, Future};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::header::{
    HeaderName, HeaderValue, ALLOW, CACHE_CONTROL, CONTENT_TYPE, COOKIE, ORIGIN, SET_COOKIE,
};
use http::{HeaderMap, Method, StatusCode};
use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};

/// Default loopback bind host.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Read deadline for one HTTP read (design: five seconds).
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum outstanding HTTP reads (design: sixteen).
pub const MAX_OUTSTANDING_READS: usize = 16;
/// Content type used when a response does not declare a more specific one.
pub const OCTET_STREAM: &str = "application/octet-stream";
/// Maximum accepted control POST body (design: 8192 bytes).
pub const POST_BODY_MAX_BYTES: usize = 8192;
/// Operator session/CSRF cookie name (design: `agalma_operator`).
pub const OPERATOR_COOKIE: &str = "agalma_operator";
/// The cookie path the operator session is scoped to.
pub const OPERATOR_COOKIE_PATH: &str = "/api/v1/factory";
/// Bytes of OS randomness in each operator token (design: 32 bytes).
pub const OPERATOR_TOKEN_BYTES: usize = 32;
/// The response header carrying the separate browser-visible CSRF secret.
pub const CSRF_HEADER: &str = "x-agalma-csrf";

/// A live operator-control authority seam.
///
/// The transport revalidates this for every session bootstrap and every POST,
/// so revoking control denies new effects even while a previously issued
/// session cookie and CSRF token remain valid. The typed factory handlers also
/// intersect the browser-visible `FactoryRuntime` permissions with it, so a
/// revoked/unavailable control surface is never reported as actionable.
pub trait ControlAuthority: Send + Sync + 'static {
    /// Whether operator control (session bootstrap and POST admission) is
    /// currently authorized.
    fn control_authorized(&self) -> bool;

    /// Whether a same-origin bootstrap GET may issue an operator session and
    /// CSRF secret.
    ///
    /// Defaults to [`Self::control_authorized`]. A factory adapter that also
    /// requires a live control binding overrides this so an authorized
    /// predicate over a read-only client never issues credentials.
    fn session_authorized(&self) -> bool {
        self.control_authorized()
    }
}

/// The read-only default: control is never authorized.
impl ControlAuthority for bool {
    fn control_authorized(&self) -> bool {
        *self
    }
}

/// A shared, revocable control authority.
///
/// It starts at `new(enabled)` and can be revoked (or re-granted) at runtime;
/// every request re-reads the current value. This is the seam that makes
/// revocation effective against an already-issued session cookie.
#[derive(Clone)]
pub struct RevocableControl {
    authorized: Arc<AtomicBool>,
}

impl RevocableControl {
    /// A new live authority, initially `authorized`.
    pub fn new(authorized: bool) -> Self {
        Self {
            authorized: Arc::new(AtomicBool::new(authorized)),
        }
    }

    /// Disable control for every subsequent request.
    pub fn revoke(&self) {
        self.authorized.store(false, Ordering::SeqCst);
    }

    /// Re-enable control for subsequent requests.
    pub fn authorize(&self) {
        self.authorized.store(true, Ordering::SeqCst);
    }

    /// Whether control is currently authorized.
    pub fn is_authorized(&self) -> bool {
        self.authorized.load(Ordering::SeqCst)
    }
}

impl ControlAuthority for RevocableControl {
    fn control_authorized(&self) -> bool {
        self.is_authorized()
    }
}


/// A rejected listener configuration, asset path, or route pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebHttpError {
    /// The configured bind address is not a loopback address.
    NonLoopbackBind(String),
    /// No host name was configured to accept as the local authority.
    EmptyAllowedHosts,
    /// An allowed host name is empty or contains a port/scheme.
    InvalidHost(String),
    /// An asset path is not a plain absolute path.
    InvalidAssetPath(String),
    /// The same asset path was registered twice.
    DuplicateAsset(String),
    /// A route pattern is malformed.
    InvalidRoute(String),
    /// The same route pattern was registered twice.
    DuplicateRoute(String),
    /// The outstanding-read bound must be at least one.
    ZeroOutstandingReads,
    /// The outstanding-read bound exceeds the design cap.
    OutstandingReadsTooLarge(usize),
    /// The read deadline must be non-zero.
    ZeroReadDeadline,
    /// The read deadline exceeds the design cap.
    ReadDeadlineTooLong(Duration),
    /// OS randomness was unavailable while generating operator tokens.
    RandomnessUnavailable(String),
    /// A `{name}` bootstrap pattern is not a valid route pattern.
    InvalidBootstrapRoute(String),
}

impl std::fmt::Display for WebHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebHttpError::NonLoopbackBind(addr) => {
                write!(f, "web listener bind {addr} is not a loopback address")
            }
            WebHttpError::EmptyAllowedHosts => write!(f, "no allowed host name configured"),
            WebHttpError::InvalidHost(host) => write!(f, "invalid allowed host {host:?}"),
            WebHttpError::InvalidAssetPath(path) => write!(f, "invalid asset path {path:?}"),
            WebHttpError::DuplicateAsset(path) => {
                write!(f, "asset path {path:?} is already registered")
            }
            WebHttpError::InvalidRoute(route) => write!(f, "invalid route pattern {route:?}"),
            WebHttpError::DuplicateRoute(route) => {
                write!(f, "route {route:?} is already registered")
            }
            WebHttpError::ZeroOutstandingReads => {
                write!(f, "max outstanding reads must be at least one")
            }
            WebHttpError::OutstandingReadsTooLarge(max) => write!(
                f,
                "max outstanding reads {max} exceeds the cap {MAX_OUTSTANDING_READS}"
            ),
            WebHttpError::ZeroReadDeadline => write!(f, "read deadline must be non-zero"),
            WebHttpError::ReadDeadlineTooLong(deadline) => write!(
                f,
                "read deadline {deadline:?} exceeds the cap {READ_DEADLINE:?}"
            ),
            WebHttpError::RandomnessUnavailable(reason) => {
                write!(f, "OS randomness unavailable: {reason}")
            }
            WebHttpError::InvalidBootstrapRoute(route) => {
                write!(f, "invalid session bootstrap route {route:?}")
            }
        }
    }
}

impl std::error::Error for WebHttpError {}

/// Listener configuration: bind address, allowed local authorities, read
/// deadline and outstanding-read bound.
///
/// [`ListenerConfig::loopback`] builds the accepted default (loopback host,
/// ephemeral port, five-second deadline, sixteen outstanding reads). A
/// non-loopback [`ListenerConfig::bind`] is refused by
/// [`ListenerConfig::validate`] before any socket exists.
#[derive(Clone, Debug)]
pub struct ListenerConfig {
    bind: SocketAddr,
    allowed_hosts: Vec<String>,
    read_deadline: Duration,
    max_outstanding_reads: usize,
}

impl ListenerConfig {
    /// The accepted default: `127.0.0.1:<port>` (use `0` for an OS-assigned
    /// port), allowed hosts `127.0.0.1` and `localhost`.
    pub fn loopback(port: u16) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], port)),
            allowed_hosts: vec![DEFAULT_HOST.to_string(), "localhost".to_string()],
            read_deadline: READ_DEADLINE,
            max_outstanding_reads: MAX_OUTSTANDING_READS,
        }
    }

    /// Set an explicit bind address; validated by [`Self::validate`].
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.bind = addr;
        self
    }

    /// Replace the accepted host names (without port). The port always comes
    /// from the actual bound address.
    pub fn allowed_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Override the read deadline; [`Self::validate`] refuses a value above the
    /// five-second design cap (shorter test deadlines stay allowed).
    pub fn read_deadline(mut self, deadline: Duration) -> Self {
        self.read_deadline = deadline;
        self
    }

    /// Override the outstanding-read bound; [`Self::validate`] refuses a value
    /// above the sixteen-read design cap.
    pub fn max_outstanding_reads(mut self, max: usize) -> Self {
        self.max_outstanding_reads = max;
        self
    }

    /// The configured bind address.
    pub fn bind_addr(&self) -> SocketAddr {
        self.bind
    }

    /// The configured read deadline.
    pub fn read_deadline_value(&self) -> Duration {
        self.read_deadline
    }

    /// The configured outstanding-read bound.
    pub fn max_outstanding(&self) -> usize {
        self.max_outstanding_reads
    }

    /// Validate the configuration: loopback bind, non-empty well-formed host
    /// names, a positive outstanding bound and a non-zero deadline.
    pub fn validate(&self) -> Result<(), WebHttpError> {
        if !self.bind.ip().is_loopback() {
            return Err(WebHttpError::NonLoopbackBind(self.bind.to_string()));
        }
        if self.allowed_hosts.is_empty() {
            return Err(WebHttpError::EmptyAllowedHosts);
        }
        for host in &self.allowed_hosts {
            if !valid_host_name(host) {
                return Err(WebHttpError::InvalidHost(host.clone()));
            }
        }
        if self.max_outstanding_reads == 0 {
            return Err(WebHttpError::ZeroOutstandingReads);
        }
        if self.max_outstanding_reads > MAX_OUTSTANDING_READS {
            return Err(WebHttpError::OutstandingReadsTooLarge(
                self.max_outstanding_reads,
            ));
        }
        if self.read_deadline.is_zero() {
            return Err(WebHttpError::ZeroReadDeadline);
        }
        if self.read_deadline > READ_DEADLINE {
            return Err(WebHttpError::ReadDeadlineTooLong(self.read_deadline));
        }
        Ok(())
    }
}

/// A bare host name: non-empty, ASCII alphanumeric/`.`/`-`, no scheme or port.
fn valid_host_name(host: &str) -> bool {
    !host.is_empty()
        && !host.contains(':')
        && !host.contains('/')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// One packaged browser asset served from the explicit inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    path: String,
    content_type: String,
    bytes: Vec<u8>,
}

impl Asset {
    /// The exact request path this asset answers (for example `/`, `/app.js`).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The declared content type.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// The asset bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// An explicit asset inventory: a request path maps to known bytes. Paths are
/// plain absolute paths with no `.`/`..` segments, no query/fragment, and no
/// backslashes, so a request can never address a filesystem location.
#[derive(Clone, Debug, Default)]
pub struct AssetInventory {
    assets: BTreeMap<String, Asset>,
}

impl AssetInventory {
    /// An empty inventory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `bytes` at `path`. Refuses a malformed path or a duplicate.
    pub fn insert(
        &mut self,
        path: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(), WebHttpError> {
        if !valid_asset_path(path) {
            return Err(WebHttpError::InvalidAssetPath(path.to_string()));
        }
        if self.assets.contains_key(path) {
            return Err(WebHttpError::DuplicateAsset(path.to_string()));
        }
        self.assets.insert(
            path.to_string(),
            Asset {
                path: path.to_string(),
                content_type: content_type.to_string(),
                bytes,
            },
        );
        Ok(())
    }

    /// The asset registered at `path`, if any.
    pub fn get(&self, path: &str) -> Option<&Asset> {
        self.assets.get(path)
    }

    /// Number of registered assets.
    pub fn len(&self) -> usize {
        self.assets.len()
    }

    /// Whether the inventory is empty.
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    /// The registered asset paths, in sorted order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.assets.keys().map(String::as_str)
    }
}

/// Whether `path` is a plain absolute asset path (no traversal, query, or
/// backslash), so an inventory lookup can never escape into the filesystem.
fn valid_asset_path(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 1024 {
        return false;
    }
    if path.contains('?') || path.contains('#') || path.contains('\\') {
        return false;
    }
    if path.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return false;
    }
    for segment in path[1..].split('/') {
        if segment == "." || segment == ".." {
            return false;
        }
    }
    true
}

/// One parsed, validated read request handed to a route handler.
///
/// `path` is the percent-decoded path; `params` holds decoded path captures.
/// The query string is exposed raw through [`WebRequest::query`] and parsed on
/// demand by [`WebRequest::query_param`]. Nothing here is reflected into an
/// error response.
#[derive(Clone, Debug)]
pub struct WebRequest {
    method: Method,
    path: String,
    raw_path: String,
    query: Option<String>,
    host: Option<String>,
    origin: Option<String>,
    params: BTreeMap<String, String>,
    matched_pattern: String,
    body: Vec<u8>,
    /// The absolute monotonic deadline admitted for this request. Every
    /// downstream step (body acquisition, adapter/client waits and the hub RPC)
    /// shares this single budget instead of minting a fresh one.
    deadline: Instant,
}

impl WebRequest {
    /// The request method (always `GET` for a routed request).
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// The percent-decoded request path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The original, still-encoded request path.
    pub fn raw_path(&self) -> &str {
        &self.raw_path
    }

    /// The raw query string, when present.
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// The `Host` header value.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The `Origin` header value, when present.
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    /// The matched route pattern.
    pub fn matched_pattern(&self) -> &str {
        &self.matched_pattern
    }

    /// A decoded path capture by name.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    /// All decoded path captures.
    pub fn params(&self) -> &BTreeMap<String, String> {
        &self.params
    }

    /// A raw query-string value by key (no percent-decoding; the adapter owns
    /// its query grammar and rejection of malformed values).
    pub fn query_param(&self, name: &str) -> Option<&str> {
        let query = self.query.as_deref()?;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then_some(value)
        })
    }

    /// The admitted request body (empty for a `GET`).
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The absolute monotonic deadline admitted for this request.
    ///
    /// Every downstream stage (body acquisition, adapter/client waits and the
    /// hub RPC) must derive its remaining budget from this value rather than
    /// starting a fresh timer, so a request cannot extend its total budget past
    /// the admitted HTTP deadline.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// A response a route handler or the transport produces.
///
/// Every response carries a content type and is emitted with
/// `Cache-Control: no-store`. [`WebResponse::attachment`] additionally sets
/// `Content-Disposition: attachment` and `X-Content-Type-Options: nosniff` so
/// a downloaded artifact never executes under the dashboard origin.
#[derive(Clone, Debug)]
pub struct WebResponse {
    status: StatusCode,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl WebResponse {
    /// A response with an explicit status, content type and body.
    pub fn bytes(status: StatusCode, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.to_string(),
            body,
            headers: Vec::new(),
        }
    }

    /// A `text/plain` response.
    pub fn text(status: StatusCode, body: impl Into<String>) -> Self {
        Self::bytes(
            status,
            "text/plain; charset=utf-8",
            body.into().into_bytes(),
        )
    }

    /// A JSON response.
    pub fn json<T: Serialize>(status: StatusCode, value: &T) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self::bytes(status, "application/json", body)
    }

    /// An attachment download: the content type is forced to
    /// `application/octet-stream`, `Content-Disposition: attachment` names the
    /// file, and `X-Content-Type-Options: nosniff` disables content sniffing.
    pub fn attachment(filename: &str, body: Vec<u8>) -> Self {
        let mut response = Self::bytes(StatusCode::OK, OCTET_STREAM, body);
        let disposition = format!("attachment; filename=\"{}\"", sanitize_filename(filename));
        response.set_header("content-disposition", disposition);
        response.set_header("x-content-type-options", "nosniff");
        response
    }

    /// The explicit error body shared by every transport-level refusal:
    /// `{"error":{"code":..,"message":..}}`.
    pub fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        let body = ErrorBody {
            error: ErrorDetail {
                code: code.to_string(),
                message: message.into(),
            },
        };
        Self::json(status, &body)
    }

    /// Add or replace a response header.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(&value)) {
            self.headers.retain(|(existing, _)| existing != &name);
            self.headers.push((name, value));
        }
    }

    /// The response status.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The response body bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The response's `Content-Type`.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Build the hyper response, adding the mandatory headers.
    fn into_http(self) -> Response<BytesBody> {
        let mut response = Response::new(BytesBody::new(self.body));
        *response.status_mut() = self.status;
        let headers = response.headers_mut();
        let content_type = HeaderValue::from_str(&self.content_type)
            .unwrap_or_else(|_| HeaderValue::from_static(OCTET_STREAM));
        headers.insert(CONTENT_TYPE, content_type);
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        for (name, value) in self.headers {
            headers.insert(name, value);
        }
        response
    }
}

/// `Content-Disposition` filename sanitizer: keep a conservative basename.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect()
}

/// The exact error body shared by every transport-level refusal.
#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
}

/// A single-shot hyper body over owned bytes.
///
/// Declared here (rather than pulling in `http-body-util`, which is not a
/// declared dependency of `agalma-ipc`) so the locked dependency set is
/// unchanged.
struct BytesBody {
    data: Option<Bytes>,
    len: usize,
}

impl BytesBody {
    fn new(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            data: Some(Bytes::from(bytes)),
            len,
        }
    }
}

impl Body for BytesBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        Poll::Ready(this.data.take().map(|data| Ok(Frame::data(data))))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.len as u64)
    }
}

/// A boxed handler future.
pub type WebFuture = Pin<Box<dyn Future<Output = WebResponse> + Send>>;

/// The dashboard-supplied typed handler for one route. The transport knows no
/// page or business logic; it only routes a validated [`WebRequest`] here.
pub trait WebHandler: Send + Sync + 'static {
    /// Handle one admitted read. Produces a [`WebResponse`]; an internal
    /// failure returns `WebResponse::error(..)` with an explicit code.
    fn handle(&self, request: WebRequest) -> WebFuture;
}

impl<F, Fut> WebHandler for F
where
    F: Fn(WebRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = WebResponse> + Send + 'static,
{
    fn handle(&self, request: WebRequest) -> WebFuture {
        Box::pin(self(request))
    }
}

/// One path pattern: literal segments and `{name}` captures.
#[derive(Clone, Debug)]
struct Pattern {
    source: String,
    segments: Vec<Seg>,
}

#[derive(Clone, Debug)]
enum Seg {
    Literal(String),
    Param(String),
}

impl Pattern {
    fn parse(source: &str) -> Result<Self, WebHttpError> {
        if !source.starts_with('/') {
            return Err(WebHttpError::InvalidRoute(source.to_string()));
        }
        let body = &source[1..];
        let mut segments = Vec::new();
        let mut names = Vec::new();
        if !body.is_empty() {
            for raw in body.split('/') {
                if raw.is_empty() {
                    return Err(WebHttpError::InvalidRoute(source.to_string()));
                }
                if let Some(name) = raw.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                    if name.is_empty()
                        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    {
                        return Err(WebHttpError::InvalidRoute(source.to_string()));
                    }
                    if names.contains(&name.to_string()) {
                        return Err(WebHttpError::InvalidRoute(source.to_string()));
                    }
                    names.push(name.to_string());
                    segments.push(Seg::Param(name.to_string()));
                } else {
                    segments.push(Seg::Literal(raw.to_string()));
                }
            }
        }
        Ok(Pattern {
            source: source.to_string(),
            segments,
        })
    }

    /// Match decoded request `segments`, returning the captured params.
    fn resolve(&self, segments: &[String]) -> Option<BTreeMap<String, String>> {
        if segments.len() != self.segments.len() {
            return None;
        }
        let mut params = BTreeMap::new();
        for (pattern, actual) in self.segments.iter().zip(segments) {
            match pattern {
                Seg::Literal(literal) if literal == actual => {}
                Seg::Param(name) if !actual.is_empty() => {
                    params.insert(name.clone(), actual.clone());
                }
                _ => return None,
            }
        }
        Some(params)
    }
}

/// The method-aware route table. `GET` routes are the read/asset surface; `POST`
/// routes are the explicitly declared control surface.
#[derive(Clone, Default)]
pub struct Router {
    routes: Vec<Route>,
}

#[derive(Clone)]
struct Route {
    method: Method,
    pattern: Pattern,
    handler: Arc<dyn WebHandler>,
}

impl Router {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a `GET` route for `pattern`. Only `GET` is supported here: the
    /// read transport grants no mutation method.
    pub fn get<H: WebHandler>(&mut self, pattern: &str, handler: H) -> Result<(), WebHttpError> {
        self.register(Method::GET, pattern, handler)
    }

    /// Register a `POST` route for `pattern`. A POST route is admitted only
    /// through the operator-control gates (session, CSRF and Origin).
    pub fn post<H: WebHandler>(&mut self, pattern: &str, handler: H) -> Result<(), WebHttpError> {
        self.register(Method::POST, pattern, handler)
    }

    fn register<H: WebHandler>(
        &mut self,
        method: Method,
        pattern: &str,
        handler: H,
    ) -> Result<(), WebHttpError> {
        let pattern = Pattern::parse(pattern)?;
        if self
            .routes
            .iter()
            .any(|route| route.method == method && route.pattern.source == pattern.source)
        {
            return Err(WebHttpError::DuplicateRoute(pattern.source));
        }
        self.routes.push(Route {
            method,
            pattern,
            handler: Arc::new(handler),
        });
        Ok(())
    }

    /// Resolve decoded request `segments` under `method` to a handler and its
    /// captures.
    fn resolve(
        &self,
        method: &Method,
        segments: &[String],
    ) -> Option<(&str, &Arc<dyn WebHandler>, BTreeMap<String, String>)> {
        self.routes
            .iter()
            .find_map(|route| {
                (route.method == *method)
                    .then(|| {
                        route
                            .pattern
                            .resolve(segments)
                            .map(|params| (route.pattern.source.as_str(), &route.handler, params))
                    })
                    .flatten()
            })
    }

    /// Whether any route is registered for `method` at `segments`.
    fn matches(&self, method: &Method, segments: &[String]) -> bool {
        self.routes
            .iter()
            .any(|route| route.method == *method && route.pattern.resolve(segments).is_some())
    }

    /// Number of registered routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the router has no routes.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// A fully assembled loopback application: configuration, asset inventory and
/// route table. Built with [`WebApp::builder`].
pub struct WebApp {
    config: ListenerConfig,
    inventory: AssetInventory,
    router: Router,
    control_authority: Arc<dyn ControlAuthority>,
    bootstrap_patterns: Vec<String>,
}

impl WebApp {
    /// Start building an application for `config`.
    pub fn builder(config: ListenerConfig) -> WebAppBuilder {
        WebAppBuilder {
            config,
            inventory: AssetInventory::new(),
            router: Router::new(),
            control_authority: Arc::new(false),
            bootstrap_patterns: Vec::new(),
            error: None,
        }
    }

    /// The configuration this application was built with.
    pub fn config(&self) -> &ListenerConfig {
        &self.config
    }

    /// The asset inventory.
    pub fn inventory(&self) -> &AssetInventory {
        &self.inventory
    }

    /// The route table.
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// Whether the operator-control surface (POST routes and session bootstrap)
    /// is currently authorized. This is re-read from the live authority, not a
    /// value captured at build time.
    pub fn control_enabled(&self) -> bool {
        self.control_authority.control_authorized()
    }

    /// The live control authority this application was built with.
    pub fn control_authority(&self) -> &Arc<dyn ControlAuthority> {
        &self.control_authority
    }
}

/// Builder for a [`WebApp`]. Methods are infallible and accumulate the first
/// error, which [`WebAppBuilder::build`] returns.
pub struct WebAppBuilder {
    config: ListenerConfig,
    inventory: AssetInventory,
    router: Router,
    control_authority: Arc<dyn ControlAuthority>,
    bootstrap_patterns: Vec<String>,
    error: Option<WebHttpError>,
}

impl WebAppBuilder {
    /// Add one asset to the explicit inventory.
    pub fn asset(mut self, path: &str, content_type: &str, bytes: Vec<u8>) -> Self {
        if self.error.is_none() {
            if let Err(e) = self.inventory.insert(path, content_type, bytes) {
                self.error = Some(e);
            }
        }
        self
    }

    /// Add every asset of `inventory`.
    pub fn assets(mut self, inventory: AssetInventory) -> Self {
        if self.error.is_none() {
            for path in inventory.paths().map(str::to_string).collect::<Vec<_>>() {
                let asset = inventory.get(&path).expect("path from inventory");
                if let Err(e) =
                    self.inventory
                        .insert(&path, asset.content_type(), asset.bytes().to_vec())
                {
                    self.error = Some(e);
                    break;
                }
            }
        }
        self
    }

    /// Register a `GET` route.
    pub fn get<H: WebHandler>(mut self, pattern: &str, handler: H) -> Self {
        if self.error.is_none() {
            if let Err(e) = self.router.get(pattern, handler) {
                self.error = Some(e);
            }
        }
        self
    }

    /// Register a `POST` route (admitted only through the control gates).
    pub fn post<H: WebHandler>(mut self, pattern: &str, handler: H) -> Self {
        if self.error.is_none() {
            if let Err(e) = self.router.post(pattern, handler) {
                self.error = Some(e);
            }
        }
        self
    }

    /// Enable the operator-control surface with a static authority. When
    /// disabled (the read-only default) there is no operator session and every
    /// POST is refused.
    ///
    /// For a live, revocable surface use [`Self::control_authority`].
    pub fn control(mut self, enabled: bool) -> Self {
        self.control_authority = Arc::new(enabled);
        self
    }

    /// Set the live control authority. The transport revalidates it for every
    /// session bootstrap and every POST, so a revoked authority denies effects
    /// even while an issued session cookie remains valid.
    pub fn control_authority(mut self, authority: Arc<dyn ControlAuthority>) -> Self {
        self.control_authority = authority;
        self
    }

    /// Mark a `GET` pattern whose same-origin response bootstraps the operator
    /// session and CSRF tokens (for example `/api/v1/factory`).
    pub fn session_bootstrap(mut self, pattern: &str) -> Self {
        if self.error.is_none() {
            match Pattern::parse(pattern) {
                Ok(_) => self.bootstrap_patterns.push(pattern.to_string()),
                Err(e) => self.error = Some(e),
            }
        }
        self
    }

    /// Validate and assemble the application.
    pub fn build(self) -> Result<WebApp, WebHttpError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        self.config.validate()?;
        Ok(WebApp {
            config: self.config,
            inventory: self.inventory,
            router: self.router,
            control_authority: self.control_authority,
            bootstrap_patterns: self.bootstrap_patterns,
        })
    }
}

/// One per-launch operator session: independent 32-byte OS-random session and
/// CSRF tokens. Tokens are created when the listener starts and never appear in
/// a URL, an error, or a log; rotation happens by restarting the listener.
pub struct OperatorSession {
    cookie_token: String,
    csrf_token: String,
}

impl OperatorSession {
    /// Generate independent 32-byte session/CSRF tokens from the OS CSPRNG
    /// (`/dev/urandom`). Fails closed (returns an error) when randomness is
    /// unavailable; it never falls back to time or a constant.
    pub fn generate() -> Result<Self, WebHttpError> {
        let mut cookie = [0u8; OPERATOR_TOKEN_BYTES];
        let mut csrf = [0u8; OPERATOR_TOKEN_BYTES];
        fill_random(&mut cookie)?;
        fill_random(&mut csrf)?;
        Ok(Self {
            cookie_token: hex_lower(&cookie),
            csrf_token: hex_lower(&csrf),
        })
    }

    /// The `Set-Cookie` value for this session.
    fn set_cookie(&self) -> String {
        format!(
            "{OPERATOR_COOKIE}={}; HttpOnly; SameSite=Strict; Path={OPERATOR_COOKIE_PATH}",
            self.cookie_token
        )
    }

    /// The separate CSRF secret delivered to same-origin JavaScript.
    fn csrf_value(&self) -> &str {
        &self.csrf_token
    }
}

/// Fill `out` from `/dev/urandom`, failing closed on any error.
fn fill_random(out: &mut [u8]) -> Result<(), WebHttpError> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|e| WebHttpError::RandomnessUnavailable(e.to_string()))?;
    file.read_exact(out)
        .map_err(|e| WebHttpError::RandomnessUnavailable(e.to_string()))
}

/// Lowercase hex of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Runtime state shared by every accepted connection.
struct ListenerState {
    app: WebApp,
    read_deadline: Duration,
    outstanding: Arc<Semaphore>,
    allowed_authorities: Vec<String>,
    allowed_origins: Vec<String>,
    control_authority: Arc<dyn ControlAuthority>,
    bootstrap_patterns: Vec<String>,
    session: Option<OperatorSession>,
    /// Set once shutdown starts: no new read is admitted.
    draining: AtomicBool,
}

impl ListenerState {
    fn new(app: WebApp, local_addr: SocketAddr, session: Option<OperatorSession>) -> Self {
        let port = local_addr.port();
        let allowed_authorities: Vec<String> = app
            .config
            .allowed_hosts
            .iter()
            .map(|host| format!("{host}:{port}"))
            .collect();
        let allowed_origins = allowed_authorities
            .iter()
            .map(|authority| format!("http://{authority}"))
            .collect();
        let read_deadline = app.config.read_deadline;
        let max_outstanding = app.config.max_outstanding_reads;
        let control_authority = app.control_authority.clone();
        let bootstrap_patterns = app.bootstrap_patterns.clone();
        Self {
            app,
            read_deadline,
            outstanding: Arc::new(Semaphore::new(max_outstanding)),
            allowed_authorities,
            allowed_origins,
            control_authority,
            bootstrap_patterns,
            session,
            draining: AtomicBool::new(false),
        }
    }

    fn host_allowed(&self, host: Option<&str>) -> bool {
        match host {
            Some(host) => self.allowed_authorities.iter().any(|a| a == host),
            None => false,
        }
    }

    fn origin_allowed(&self, origin: Option<&str>) -> bool {
        match origin {
            None => true,
            Some(origin) => self.allowed_origins.iter().any(|o| o == origin),
        }
    }

    /// Handle one admitted request, returning a hyper response.
    async fn handle(
        self: Arc<Self>,
        request: Request<Incoming>,
    ) -> Response<BytesBody> {
        let (parts, body) = request.into_parts();
        let method = parts.method.clone();
        let host = header_str(&parts.headers, "host");
        // Strict Origin: a malformed or duplicated Origin is refused here,
        // before any method logic, so it can never be silently dropped (which
        // would turn a malformed Origin into an absent one).
        let origins = match origin_headers(&parts.headers) {
            Ok(origins) => origins,
            Err(message) => {
                return error_response(StatusCode::FORBIDDEN, "forbidden", message)
            }
        };
        let origin = origins.first().cloned();
        // One absolute monotonic deadline for the whole admitted request: body
        // acquisition, adapter/client waits and the hub RPC all share it rather
        // than minting a fresh budget downstream.
        let deadline = Instant::now() + self.read_deadline;

        // Security gates precede any work: foreign Host is refused for every
        // method; drain refuses every method.
        if !self.host_allowed(host.as_deref()) {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "request Host is not the configured local authority",
            );
        }
        if self.draining.load(Ordering::SeqCst) {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "listener is shutting down",
            );
        }

        let raw_path = parts.uri.path().to_string();
        let segments = match decode_path(&raw_path) {
            Ok(segments) => segments,
            Err(message) => {
                return error_response(StatusCode::BAD_REQUEST, "invalid_request", message)
            }
        };
        let decoded_path = format!("/{}", segments.join("/"));

        match method {
            Method::GET => {
                // A GET Origin, when present, must match the configured local
                // origin; it is never sufficient for mutation.
                if !self.origin_allowed(origin.as_deref()) {
                    return error_response(
                        StatusCode::FORBIDDEN,
                        "forbidden",
                        "request Origin is not the configured local origin",
                    );
                }
                self.handle_get(
                    parts,
                    segments,
                    decoded_path,
                    raw_path,
                    host,
                    origin,
                    deadline,
                )
                .await
            }
            Method::POST => {
                self.handle_post(parts, body, segments, decoded_path, host, origins, deadline)
                    .await
            }
            other => method_not_allowed_for(&self, &other, &segments, &decoded_path),
        }
    }

    /// Serve one GET: packaged asset or a read route.
    async fn handle_get(
        self: &Arc<Self>,
        parts: http::request::Parts,
        segments: Vec<String>,
        decoded_path: String,
        raw_path: String,
        host: Option<String>,
        origin: Option<String>,
        deadline: Instant,
    ) -> Response<BytesBody> {
        if let Some(asset) = self.app.inventory.get(&decoded_path) {
            let response =
                WebResponse::bytes(StatusCode::OK, asset.content_type(), asset.bytes().to_vec());
            return response.into_http();
        }

        let Some((pattern, handler, params)) = self.app.router.resolve(&Method::GET, &segments)
        else {
            // A POST-only route reached with GET is method-denied, not unknown.
            if self.app.router.matches(&Method::POST, &segments) {
                return method_not_allowed("POST");
            }
            let message = format!("unknown route: GET {decoded_path}");
            return error_response(StatusCode::NOT_FOUND, "not_found", message);
        };

        // Bound outstanding requests: when every slot is held, refuse with
        // bounded backpressure instead of queueing without limit.
        let permit = match self.outstanding.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "overloaded",
                    "too many outstanding reads",
                )
            }
        };

        let web_request = WebRequest {
            method: Method::GET,
            path: decoded_path,
            raw_path,
            query: parts.uri.query().map(str::to_string),
            host,
            origin,
            params,
            matched_pattern: pattern.to_string(),
            body: Vec::new(),
            deadline,
        };
        let mut response = match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            handler.handle(web_request),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => WebResponse::error(
                StatusCode::GATEWAY_TIMEOUT,
                "deadline_exceeded",
                "read deadline exceeded",
            ),
        };
        drop(permit);

        // A same-origin factory GET bootstraps the session/CSRF tokens, but only
        // while the live control authority remains authorized and the adapter's
        // session predicate holds (for the factory routes: a live authority
        // predicate *and* an actual control binding).
        if self.control_authority.session_authorized()
            && self.bootstrap_patterns.iter().any(|p| p.as_str() == pattern)
        {
            if let Some(session) = &self.session {
                response.set_header(SET_COOKIE.as_str(), session.set_cookie());
                response.set_header(CSRF_HEADER, session.csrf_value());
            }
        }
        response.into_http()
    }

    /// Serve one POST request. A POST to a route that is not a declared control
    /// route is method-denied or unknown; a declared control route runs the
    /// full Origin/session/CSRF/authority gates before any work.
    async fn handle_post(
        self: &Arc<Self>,
        parts: http::request::Parts,
        body: Incoming,
        segments: Vec<String>,
        decoded_path: String,
        host: Option<String>,
        origins: Vec<String>,
        deadline: Instant,
    ) -> Response<BytesBody> {
        // Resolve the declared control route first: a POST to a non-control
        // path is method-denied/unknown, never an auth failure.
        let Some((pattern, handler, params)) = self.app.router.resolve(&Method::POST, &segments)
        else {
            if self.app.inventory.get(&decoded_path).is_some()
                || self.app.router.matches(&Method::GET, &segments)
            {
                return method_not_allowed("GET");
            }
            let message = format!("unknown route: POST {decoded_path}");
            return error_response(StatusCode::NOT_FOUND, "not_found", message);
        };

        // Exactly one Origin, equal to the actual scheme/host/port authority.
        if origins.len() != 1 {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "POST requires exactly one Origin matching the local authority",
            );
        }
        let expected_origin = match host.as_deref() {
            Some(host) => format!("http://{host}"),
            None => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "POST requires a configured local Host",
                )
            }
        };
        if origins[0] != expected_origin {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "request Origin does not match the request authority",
            );
        }

        // Revalidate the live control authority on every effect. A revoked
        // authority denies POST even while a previously issued cookie persists.
        if !self.control_authority.control_authorized() {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "operator control is not authorized",
            );
        }

        // Session cookie and separate CSRF secret are both required, and both
        // must be singular and well-formed: a duplicated or malformed
        // credential is refused rather than silently reduced to its first value.
        let Some(session) = self.session.as_ref() else {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "no operator session is available",
            );
        };
        let presented = match operator_cookie(&parts.headers) {
            Ok(value) => value,
            Err(message) => {
                return error_response(StatusCode::FORBIDDEN, "forbidden", message)
            }
        };
        if presented.as_deref() != Some(session.cookie_token.as_str()) {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "operator session cookie is missing or invalid",
            );
        }
        let csrf = match single_header(&parts.headers, CSRF_HEADER) {
            Ok(value) => value,
            Err(message) => {
                return error_response(StatusCode::FORBIDDEN, "forbidden", message)
            }
        };
        if csrf.as_deref() != Some(session.csrf_token.as_str()) {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "CSRF header is missing or invalid",
            );
        }

        let content_type = header_str(&parts.headers, CONTENT_TYPE.as_str());
        if !is_json_content_type(content_type.as_deref()) {
            return error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "request body must be application/json",
            );
        }

        // The common admission ceiling covers body acquisition and the handler.
        let permit = match self.outstanding.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "overloaded",
                    "too many outstanding requests",
                )
            }
        };

        let query = parts.uri.query().map(str::to_string);
        let matched = pattern.to_string();
        // Capture the live authority so the post-body recheck can run after the
        // body has been fully acquired (the pre-body check cannot cover that
        // window).
        let control = self.control_authority.clone();
        let work = async move {
            let bytes = match read_body_capped(body, POST_BODY_MAX_BYTES).await {
                Ok(bytes) => bytes,
                Err(BodyReadError::TooLarge) => {
                    return WebResponse::error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "payload_too_large",
                        "request body exceeds 8192 bytes",
                    )
                }
                Err(BodyReadError::Io(message)) => {
                    return WebResponse::error(StatusCode::BAD_REQUEST, "invalid_request", message)
                }
            };
            // Revalidate the live control authority now that the body is fully
            // acquired: a revocation while the body was arriving must deny the
            // effect before the handler (and any RPC) runs.
            if !control.control_authorized() {
                return WebResponse::error(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "operator control is not authorized",
                );
            }
            let web_request = WebRequest {
                method: Method::POST,
                path: decoded_path,
                raw_path: parts.uri.path().to_string(),
                query,
                host,
                origin: origins.into_iter().next(),
                params,
                matched_pattern: matched,
                body: bytes,
                deadline,
            };
            handler.handle(web_request).await
        };

        let response = match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), work)
            .await
        {
            Ok(response) => response,
            Err(_) => WebResponse::error(
                StatusCode::GATEWAY_TIMEOUT,
                "deadline_exceeded",
                "request deadline exceeded",
            ),
        };
        drop(permit);
        response.into_http()
    }
}

/// A capped incremental body-read failure.
enum BodyReadError {
    /// More than the configured limit was received.
    TooLarge,
    /// The transport reported a body error.
    Io(String),
}

/// Read at most `limit` body bytes, enforcing the bound as frames arrive so a
/// streamed or chunked body cannot exceed it regardless of `Content-Length`.
async fn read_body_capped(mut body: Incoming, limit: usize) -> Result<Vec<u8>, BodyReadError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let frame = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        match frame {
            None => return Ok(out),
            Some(Err(error)) => return Err(BodyReadError::Io(error.to_string())),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    if out.len() + data.len() > limit {
                        return Err(BodyReadError::TooLarge);
                    }
                    out.extend_from_slice(&data);
                }
            }
        }
    }
}

/// Whether `value` is `application/json` with an optional `charset=utf-8`.
fn is_json_content_type(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let mut parts = value.split(';');
    let Some(media) = parts.next() else {
        return false;
    };
    if !media.trim().eq_ignore_ascii_case("application/json") {
        return false;
    }
    for param in parts {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        let Some((key, value)) = param.split_once('=') else {
            return false;
        };
        if !key.trim().eq_ignore_ascii_case("charset") {
            return false;
        }
        let value = value.trim().trim_matches('"');
        if !value.eq_ignore_ascii_case("utf-8") {
            return false;
        }
    }
    true
}

/// Parse every `Origin` request header strictly.
///
/// A malformed (non-UTF-8) Origin and a duplicated Origin both fail closed: the
/// caller turns the message into `403`. An absent Origin is `Ok(vec![])`, which
/// GET admits and POST rejects.
fn origin_headers(headers: &HeaderMap) -> Result<Vec<String>, &'static str> {
    let mut origins = Vec::new();
    for value in headers.get_all(ORIGIN).iter() {
        let text = value
            .to_str()
            .map_err(|_| "request Origin header is malformed")?;
        origins.push(text.to_string());
    }
    if origins.len() > 1 {
        return Err("request carries multiple Origin headers");
    }
    Ok(origins)
}

/// Read one header that must be singular and well-formed.
///
/// `Ok(None)` means absent; a duplicated or malformed header is an error the
/// caller turns into `403`.
fn single_header(headers: &HeaderMap, name: &str) -> Result<Option<String>, &'static str> {
    let mut iter = headers.get_all(name).iter();
    let Some(value) = iter.next() else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .map_err(|_| "request credential header is malformed")?
        .to_string();
    if iter.next().is_some() {
        return Err("request carries duplicate credential headers");
    }
    Ok(Some(text))
}

/// Extract the exactly-one operator session cookie value.
///
/// A malformed (non-UTF-8) or structurally broken `Cookie` header is refused as
/// malformed; two `agalma_operator` values are refused as duplicates. `Ok(None)`
/// means no operator cookie was presented.
fn operator_cookie(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let mut found: Option<String> = None;
    for value in headers.get_all(COOKIE).iter() {
        let text = value
            .to_str()
            .map_err(|_| "request Cookie header is malformed")?;
        for part in text.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((key, value)) = part.split_once('=') else {
                return Err("request Cookie header is malformed");
            };
            if key == OPERATOR_COOKIE {
                if found.is_some() {
                    return Err("request carries duplicate operator session cookies");
                }
                found = Some(value.to_string());
            }
        }
    }
    Ok(found)
}

/// Percent-decode an already-extracted query parameter value exactly once.
///
/// Query values arrive percent-encoded; a value that is compared or forwarded
/// (such as an opaque cursor) must be decoded exactly once — never zero, never
/// twice. `+` stays literal so an opaque token keeps its exact identity; a bad
/// escape or a non-UTF-8 result is refused.
pub fn decode_query_value(raw: &str) -> Result<String, String> {
    percent_decode(raw)
}

/// The `405` refusal for a method the matched route does not support.
fn method_not_allowed(allow: &str) -> Response<BytesBody> {
    let mut response = error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        format!("only {allow} is supported"),
    );
    if let Ok(value) = HeaderValue::from_str(allow) {
        response.headers_mut().insert(ALLOW, value);
    }
    response
}

/// The `405`/`404` selection for an unsupported method on `segments`.
fn method_not_allowed_for(
    state: &ListenerState,
    method: &Method,
    segments: &[String],
    decoded_path: &str,
) -> Response<BytesBody> {
    let mut allow: Vec<&str> = Vec::new();
    if state.app.inventory.get(decoded_path).is_some()
        || state.app.router.matches(&Method::GET, segments)
    {
        allow.push("GET");
    }
    if state.app.router.matches(&Method::POST, segments) {
        allow.push("POST");
    }
    if allow.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("unknown route: {method} {decoded_path}"),
        );
    }
    method_not_allowed(&allow.join(", "))
}

/// A running loopback listener. Dropping it does not drain; call
/// [`LoopbackListener::shutdown`] for a clean stop.
pub struct LoopbackListener {
    local_addr: SocketAddr,
    shutdown_tx: Option<watch::Sender<bool>>,
    accept_task: Option<tokio::task::JoinHandle<()>>,
}

impl LoopbackListener {
    /// Bind and start serving `app`. Refuses a non-loopback configuration
    /// before creating a socket.
    pub async fn start(app: WebApp) -> std::io::Result<LoopbackListener> {
        app.config
            .validate()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        // An authorized control surface needs a fresh per-launch operator
        // session; randomness failure refuses to start rather than weakening
        // control. A later revocation keeps the session but denies its use.
        let session = if app.control_authority.control_authorized() {
            Some(
                OperatorSession::generate()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?,
            )
        } else {
            None
        };
        let listener = TcpListener::bind(app.config.bind).await?;
        let local_addr = listener.local_addr()?;
        let state = Arc::new(ListenerState::new(app, local_addr, session));
        let drain_timeout = state.read_deadline;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let accept_state = state.clone();
        let accept_task = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            let mut connections = tokio::task::JoinSet::new();
            'accept: loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break 'accept;
                        }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _peer)) => {
                                let connection_state = accept_state.clone();
                                let mut connection_shutdown = shutdown_rx.clone();
                                connections.spawn(async move {
                                    let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                                        let state = connection_state.clone();
                                        async move {
                                            Ok::<_, std::convert::Infallible>(state.handle(req).await)
                                        }
                                    });
                                    let connection = hyper::server::conn::http1::Builder::new()
                                        .serve_connection(TokioIo::new(stream), service);
                                    tokio::pin!(connection);
                                    tokio::select! {
                                        _ = wait_for_drain(&mut connection_shutdown) => {
                                            // Stop admitting new requests on this
                                            // connection, then give the in-flight
                                            // response the drain window.
                                            connection.as_mut().graceful_shutdown();
                                            let _ = tokio::time::timeout(
                                                drain_timeout,
                                                connection.as_mut(),
                                            )
                                            .await;
                                        }
                                        result = connection.as_mut() => {
                                            let _ = result;
                                        }
                                    }
                                });
                            }
                            Err(_) => continue,
                        }
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            // Stop admission for good, drain admitted reads within the read
            // deadline, and abort whatever is left so shutdown stays bounded.
            accept_state.draining.store(true, Ordering::SeqCst);
            let drain = async { while connections.join_next().await.is_some() {} };
            if tokio::time::timeout(drain_timeout, drain).await.is_err() {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
            }
        });

        Ok(LoopbackListener {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            accept_task: Some(accept_task),
        })
    }

    /// The actual bound loopback address (the real port when `0` was
    /// configured).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The browser-facing base URL, for example `http://127.0.0.1:53211`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Stop accepting, close idle keep-alive connections, then drain every
    /// admitted read within the read deadline. Returns after the drain window
    /// even if a read is still stuck (remaining connections are then aborted).
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(task) = self.accept_task.take() {
            let _ = task.await;
        }
    }
}

/// Resolve once the listener has begun draining (or the sender is gone).
async fn wait_for_drain(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Build an explicit JSON error response.
fn error_response(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> Response<BytesBody> {
    WebResponse::error(status, code, message).into_http()
}

/// Read a header's value as a UTF-8 string.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Percent-decode a raw request path into segments.
///
/// Refuses a relative path, a bad escape, or a decoded `/`/NUL (which would
/// let an encoded separator smuggle an extra path segment).
fn decode_path(raw: &str) -> Result<Vec<String>, String> {
    if !raw.starts_with('/') {
        return Err("request path must be absolute".to_string());
    }
    let body = &raw[1..];
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for segment in body.split('/') {
        let decoded = percent_decode(segment)?;
        if decoded.contains('/') || decoded.contains('\0') {
            return Err("escaped path separator or NUL in a path segment".to_string());
        }
        segments.push(decoded);
    }
    Ok(segments)
}

/// Decode `%XX` escapes in one path segment; `+` is literal.
fn percent_decode(segment: &str) -> Result<String, String> {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .ok_or_else(|| "truncated percent escape".to_string())?;
                let high = hex_value(hex[0]).ok_or_else(|| "invalid percent escape".to_string())?;
                let low = hex_value(hex[1]).ok_or_else(|| "invalid percent escape".to_string())?;
                out.push((high << 4) | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "percent-decoded segment is not UTF-8".to_string())
}

/// A hex digit's value.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback_bind() {
        let config = ListenerConfig::loopback(0).bind("0.0.0.0:8080".parse().unwrap());
        assert_eq!(
            config.validate(),
            Err(WebHttpError::NonLoopbackBind("0.0.0.0:8080".to_string()))
        );
    }

    #[test]
    fn accepts_loopback_ephemeral() {
        assert!(ListenerConfig::loopback(0).validate().is_ok());
    }

    #[test]
    fn enforces_the_read_deadline_and_outstanding_read_caps() {
        let boundary = ListenerConfig::loopback(0)
            .max_outstanding_reads(MAX_OUTSTANDING_READS)
            .read_deadline(READ_DEADLINE);
        assert_eq!(boundary.validate(), Ok(()));

        assert_eq!(
            ListenerConfig::loopback(0)
                .max_outstanding_reads(MAX_OUTSTANDING_READS + 1)
                .validate(),
            Err(WebHttpError::OutstandingReadsTooLarge(
                MAX_OUTSTANDING_READS + 1
            ))
        );
        assert_eq!(
            ListenerConfig::loopback(0)
                .read_deadline(READ_DEADLINE + Duration::from_millis(1))
                .validate(),
            Err(WebHttpError::ReadDeadlineTooLong(
                READ_DEADLINE + Duration::from_millis(1)
            ))
        );
    }

    #[test]
    fn asset_paths_reject_traversal_and_query() {
        let mut inventory = AssetInventory::new();
        assert!(inventory.insert("/", "text/html", vec![1]).is_ok());
        assert!(inventory
            .insert("/app.js", "text/javascript", vec![2])
            .is_ok());
        assert_eq!(
            inventory.insert("/../secret", "text/plain", vec![]),
            Err(WebHttpError::InvalidAssetPath("/../secret".to_string()))
        );
        assert_eq!(
            inventory.insert("relative", "text/plain", vec![]),
            Err(WebHttpError::InvalidAssetPath("relative".to_string()))
        );
        assert_eq!(
            inventory.insert("/a?b", "text/plain", vec![]),
            Err(WebHttpError::InvalidAssetPath("/a?b".to_string()))
        );
        assert_eq!(
            inventory.insert("/", "text/html", vec![9]),
            Err(WebHttpError::DuplicateAsset("/".to_string()))
        );
    }

    #[test]
    fn pattern_captures_and_rejects_malformed() {
        let pattern = Pattern::parse("/api/v1/components/{plugin_id}").unwrap();
        let captured = pattern
            .resolve(&[
                "api".to_string(),
                "v1".to_string(),
                "components".to_string(),
                "x".to_string(),
            ])
            .expect("match");
        assert_eq!(captured.get("plugin_id").map(String::as_str), Some("x"));
        assert!(pattern
            .resolve(&[
                "api".to_string(),
                "v1".to_string(),
                "components".to_string()
            ])
            .is_none());
        assert!(Pattern::parse("api/v1").is_err());
        assert!(Pattern::parse("/a/{}").is_err());
        assert!(Pattern::parse("/a/{x}/{x}").is_err());
    }

    #[test]
    fn percent_decode_is_exact() {
        assert_eq!(percent_decode("a%20b").unwrap(), "a b");
        assert_eq!(percent_decode("a+b").unwrap(), "a+b");
        assert_eq!(percent_decode("%2F").unwrap(), "/");
        assert!(percent_decode("%").is_err());
        assert!(percent_decode("%zz").is_err());
    }

    #[test]
    fn decode_path_refuses_smuggled_separator() {
        assert_eq!(decode_path("/").unwrap(), Vec::<String>::new());
        assert_eq!(decode_path("/a/b").unwrap(), vec!["a", "b"]);
        assert!(decode_path("a/b").is_err());
        assert!(decode_path("/a%2Fb").is_err());
        assert!(decode_path("/a%00b").is_err());
    }

    #[test]
    fn attachment_disables_sniffing() {
        let response = WebResponse::attachment("../evil.html", b"<script>".to_vec());
        assert_eq!(response.content_type(), OCTET_STREAM);
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "content-disposition"
                && value.to_str().unwrap().contains("attachment")));
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "x-content-type-options" && value == "nosniff"));
    }
}
