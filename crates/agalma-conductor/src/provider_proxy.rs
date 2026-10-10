//! Parent-owned loopback forward proxy for confined-harness provider egress.
//!
//! The Seatbelt worker profile (S0b) denies arbitrary network and allows only
//! loopback. A confined harness therefore cannot reach the model provider
//! directly. The fix is this proxy: the parent (unconfined) binds an ephemeral
//! `127.0.0.1` port, the harness is pointed at it with `HTTPS_PROXY`/`HTTP_PROXY`
//! (see the OpenCode adapter), and every provider request tunnels through the
//! parent, which is the only process that may open arbitrary sockets.
//!
//! Scope:
//!
//! - `CONNECT host:port` tunnelling (the HTTPS path the free provider uses);
//! - optional plain-HTTP absolute-form forwarding (`GET http://host/path`);
//! - **paid HTTP-forward mode** (`agalma-csw.2`): the worker talks plain HTTP to
//!   `http://127.0.0.1:<port>/<path>`; the proxy maps the path onto the
//!   configured upstream base (e.g. `https://opencode.ai/zen/go/v1`), opens TLS
//!   (rustls + webpki-roots), injects `Authorization: Bearer <token>`, and
//!   streams the response (chunked / SSE included) back. The token is read by
//!   the parent process and never crosses into the sandbox, the ledger, or a
//!   digest ([`crate::provider_auth`]);
//! - a host allowlist (exact or suffix match, case-insensitive), configurable
//!   via [`ALLOW_ENV`] (`AGALMA_PROXY_ALLOW`, comma-separated) and defaulting to
//!   the OpenCode provider hosts the free model needs ([`DEFAULT_ALLOW`]);
//! - non-allowlisted `CONNECT`/absolute-form/forward requests are answered `403`;
//! - transparent tunnelling and absolute-form forwarding handle **no
//!   credential**; only the paid forward mode inspects/rewrites `Authorization`.
//!
//! Lifecycle: [`ProviderProxy::start`] binds (`127.0.0.1:0`), spawns the accept
//! loop, and exposes the bound port via [`ProviderProxy::port`]/
//! [`ProviderProxy::url`]. [`ProviderProxy::stop`] tears the listener down; a
//! `Drop` guard makes cleanup idempotent.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::provider_auth::{self, AuthState};

/// Environment variable holding the comma-separated host allowlist.
pub const ALLOW_ENV: &str = "AGALMA_PROXY_ALLOW";

/// Environment variable enabling per-request proxy logging to stderr.
pub const LOG_ENV: &str = "AGALMA_PROXY_LOG";

/// Default allowlist: the OpenCode provider hosts the free model needs.
///
/// Empirically (`opencode v2.0.15`, bundled models catalog) the free zen
/// provider `opencode` uses `https://opencode.ai/zen/v1` and the catalog source
/// is `https://models.opencode.ai`; both are covered by the `opencode.ai`
/// suffix. Extend with [`ALLOW_ENV`] without a rebuild.
pub const DEFAULT_ALLOW: &[&str] = &["opencode.ai"];

/// `NO_PROXY` value the confined child must receive: loopback control traffic
/// (the harness's own HTTP server, the nerve bridge) bypasses the proxy.
pub const NO_PROXY: &str = "127.0.0.1,localhost";

/// Upper bound on a request header block, to bound memory for a hostile client.
const MAX_HEAD: usize = 64 * 1024;

/// A host allowlist supporting exact and suffix matches.
///
/// Matching is case-insensitive. A suffix entry `example.com` matches the host
/// `example.com` and any subdomain (`api.example.com`), but never a look-alike
/// (`notexample.com`): the suffix rule requires a leading dot boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowList {
    entries: Vec<String>,
}

impl AllowList {
    /// Build an allowlist from raw entries, trimming whitespace, dropping
    /// empties, and lower-casing.
    pub fn parse(raw: &str) -> Self {
        let entries = raw
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                entry
                    .trim_start_matches('.')
                    .trim_end_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|entry| !entry.is_empty())
            .collect();
        AllowList { entries }
    }

    /// The default provider allowlist ([`DEFAULT_ALLOW`]).
    pub fn default_provider() -> Self {
        AllowList {
            entries: DEFAULT_ALLOW
                .iter()
                .map(|entry| entry.to_ascii_lowercase())
                .collect(),
        }
    }

    /// Build an allowlist from an explicit entry set.
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        AllowList {
            entries: entries
                .into_iter()
                .map(|entry| {
                    entry
                        .trim()
                        .trim_start_matches('.')
                        .trim_end_matches('.')
                        .to_ascii_lowercase()
                })
                .filter(|entry| !entry.is_empty())
                .collect(),
        }
    }

    /// The allowlist configured by [`ALLOW_ENV`], else [`AllowList::default_provider`].
    pub fn from_env() -> Self {
        match std::env::var(ALLOW_ENV) {
            Ok(value) if !value.trim().is_empty() => AllowList::parse(&value),
            _ => AllowList::default_provider(),
        }
    }

    /// Whether `host` is allowed (exact or subdomain suffix, case-insensitive).
    pub fn is_allowed(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.entries
            .iter()
            .any(|entry| host == *entry || host.ends_with(&format!(".{entry}")))
    }

    /// The raw entries, in configuration order (diagnostics only).
    pub fn entries(&self) -> &[String] {
        &self.entries
    }
}

impl fmt::Display for AllowList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.entries.is_empty() {
            return f.write_str("<empty>");
        }
        f.write_str(&self.entries.join(","))
    }
}

/// One observed proxy request, retained for test evidence and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyRequest {
    /// `CONNECT` or the plain-HTTP method (e.g. `GET`).
    pub method: String,
    /// Destination host as named in the request (no DNS resolved here).
    pub host: String,
    /// Destination port.
    pub port: u16,
    /// Whether the allowlist admitted the request.
    pub allowed: bool,
}

/// Paid forward-mode injection: map a loopback HTTP request onto an absolute
/// upstream base URL, over TLS, with an injected bearer token.
///
/// `Debug` redacts the token: the secret must never appear in logs, panics, or
/// test output.
#[derive(Clone)]
pub struct InjectConfig {
    /// Upstream host (from `upstream_base`), used for the allowlist check, the
    /// TLS SNI/Host, and the audit event.
    pub host: String,
    /// Upstream port (443 for `https`, 80 for `http`).
    pub port: u16,
    /// Absolute upstream base URL (e.g. `https://opencode.ai/zen/go/v1`).
    pub base_url: String,
    /// Path component of the base, no trailing slash (e.g. `/zen/go/v1`).
    pub base_path: String,
    /// Whether to wrap the upstream connection in TLS (rustls).
    pub tls: bool,
    /// Bearer token injected as `Authorization: Bearer <token>`. Secret.
    pub token: String,
    /// Stable session id injected as `x-opencode-session` when the client did
    /// not send one (OpenCode Go requires it for routing/caching).
    pub session_id: String,
}

impl fmt::Debug for InjectConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InjectConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("base_url", &self.base_url)
            .field("base_path", &self.base_path)
            .field("tls", &self.tls)
            .field("session_id", &self.session_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl InjectConfig {
    /// Build from an absolute base URL and a token, deriving host/port/path/tls.
    pub fn from_base(base_url: &str, token: impl Into<String>) -> Result<Self, String> {
        let (scheme, rest) = base_url
            .split_once("://")
            .ok_or_else(|| format!("upstream base {base_url:?} is not an absolute URL"))?;
        let tls = match scheme {
            "https" => true,
            "http" => false,
            other => return Err(format!("unsupported upstream scheme {other:?}")),
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => (
                host.to_string(),
                port.parse::<u16>().map_err(|e| e.to_string())?,
            ),
            _ => (authority.to_string(), if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(format!("upstream base {base_url:?} has no host"));
        }
        let path = rest[authority.len()..]
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        Ok(InjectConfig {
            host,
            port,
            base_url: base_url.trim_end_matches('/').to_string(),
            base_path: path,
            tls,
            token: token.into(),
            session_id: default_session_id(),
        })
    }
}

/// A stable-per-process session id for the `x-opencode-session` header. The
/// value is opaque routing metadata, not a credential.
fn default_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ses_agalma_{nanos:032x}{:08x}", std::process::id())
}

/// Proxy configuration.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// Host allowlist.
    pub allow: AllowList,
    /// Log each decision to stderr (also available via [`ProviderProxy::requests`]).
    pub log: bool,
    /// Paid forward-injection config, when a credential is available.
    pub inject: Option<InjectConfig>,
    /// Credential audit state (present/absent + fingerprint + reason).
    pub auth: AuthState,
}

impl ProxyConfig {
    /// Configuration from the environment: [`ALLOW_ENV`] allowlist, the
    /// [`LOG_ENV`] logging flag, and the paid credential read at proxy start.
    ///
    /// The token comes from the OpenCode auth store
    /// ([`provider_auth::default_auth_path`]) and the upstream base from
    /// [`provider_auth::PROVIDER_BASE_ENV`] (default
    /// [`provider_auth::DEFAULT_PROVIDER_BASE`]). A missing store/token leaves
    /// `inject` `None` and records the reason in `auth`.
    pub fn from_env() -> Self {
        let base = std::env::var(provider_auth::PROVIDER_BASE_ENV)
            .unwrap_or_else(|_| provider_auth::DEFAULT_PROVIDER_BASE.to_string());
        let host = provider_auth::host_of(&base);
        let auth = AuthState::load(None, &host);
        let inject = match read_inject(&base) {
            Some(token) => InjectConfig::from_base(&base, token).ok(),
            None => None,
        };
        // When the URL could not be turned into an inject config, reflect that
        // in the audit state rather than silently pretending the paid path is on.
        let auth = match (&inject, auth.present) {
            (None, true) => AuthState::disabled(host, "invalid AGALMA_PROVIDER_BASE"),
            _ => auth,
        };
        ProxyConfig {
            allow: AllowList::from_env(),
            log: std::env::var(LOG_ENV).is_ok_and(|v| !v.is_empty() && v != "0"),
            inject,
            auth,
        }
    }

    /// Configuration with an explicit (test) injection and audit state.
    pub fn with_injection(inject: InjectConfig) -> Self {
        let auth = AuthState::enabled(inject.host.clone(), &inject.token);
        ProxyConfig {
            allow: AllowList::default_provider(),
            log: false,
            inject: Some(inject),
            auth,
        }
    }

    /// Plain config (no injection) for tests and non-paid embedding.
    pub fn plain(allow: AllowList, log: bool) -> Self {
        ProxyConfig {
            allow,
            log,
            inject: None,
            auth: AuthState::disabled("", "no injection configured"),
        }
    }
}

/// Read the token for `base` from the auth store, or `None` when the paid
/// credential is unavailable. Never logs the value.
fn read_inject(base: &str) -> Option<String> {
    let _ = base;
    let path = provider_auth::default_auth_path()?;
    provider_auth::read_token(&path).ok()
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            allow: AllowList::default_provider(),
            log: false,
            inject: None,
            auth: AuthState::disabled("", "paid proxy disabled (default config)"),
        }
    }
}

/// A running parent-owned forward proxy.
pub struct ProviderProxy {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<ProxyRequest>>>,
    auth: AuthState,
}

impl ProviderProxy {
    /// Bind `127.0.0.1:0` and start accepting. Must be called inside a Tokio
    /// runtime (the accept loop is spawned).
    pub fn start(config: ProxyConfig) -> io::Result<Self> {
        // Bind synchronously so the ephemeral port is known before we return.
        let std_listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        std_listener.set_nonblocking(true)?;
        let addr = std_listener.local_addr()?;
        let listener = TcpListener::from_std(std_listener)?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let requests: Arc<Mutex<Vec<ProxyRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let allow = Arc::new(config.allow.clone());
        let log = config.log;
        let inject = config.inject.clone();
        let auth = config.auth.clone();

        let handle = tokio::spawn(async move {
            accept_loop(listener, allow, inject, log, task_requests, shutdown_rx).await;
        });

        Ok(ProviderProxy {
            addr,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
            requests,
            auth,
        })
    }

    /// The credential audit state captured at proxy start (token-free).
    pub fn auth_state(&self) -> AuthState {
        self.auth.clone()
    }

    /// The bound loopback port.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The bound loopback address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy URL to hand to the confined child (`http://127.0.0.1:<port>`).
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// A snapshot of the requests observed so far (test evidence).
    pub fn requests(&self) -> Vec<ProxyRequest> {
        self.requests
            .lock()
            .expect("proxy request log poisoned")
            .clone()
    }

    /// Stop the listener and wait for the accept loop to finish, releasing the
    /// port. Idempotent with `Drop`.
    pub async fn stop(mut self) {
        self.shutdown_now();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }

    fn shutdown_now(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl fmt::Debug for ProviderProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderProxy")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl Drop for ProviderProxy {
    fn drop(&mut self) {
        self.shutdown_now();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Accept loop: one task per connection; exits when the shutdown signal fires.
async fn accept_loop(
    listener: TcpListener,
    allow: Arc<AllowList>,
    inject: Option<InjectConfig>,
    log: bool,
    requests: Arc<Mutex<Vec<ProxyRequest>>>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let inject = Arc::new(inject);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let allow = Arc::clone(&allow);
                        let inject = Arc::clone(&inject);
                        let requests = Arc::clone(&requests);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, allow, inject, log, requests).await;
                        });
                    }
                    Err(err) => {
                        if log {
                            eprintln!("provider-proxy: accept error: {err}");
                        }
                    }
                }
            }
        }
    }
}

/// Handle one client connection: parse the head, enforce the allowlist, then
/// tunnel (CONNECT), paid-forward (injection configured), or forward
/// (absolute-form plain HTTP).
async fn handle_connection(
    mut client: TcpStream,
    allow: Arc<AllowList>,
    inject: Arc<Option<InjectConfig>>,
    log: bool,
    requests: Arc<Mutex<Vec<ProxyRequest>>>,
) -> io::Result<()> {
    let head = match read_head(&mut client).await {
        Ok(head) => head,
        Err(_) => return Ok(()),
    };
    if head.trim().is_empty() {
        return Ok(());
    }
    let request_line = head.split("\r\n").next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();

    if method.eq_ignore_ascii_case("CONNECT") {
        return handle_connect(client, &target, allow, log, requests).await;
    }
    if let Some(inject) = inject.as_ref() {
        return handle_forward(
            client, &method, &target, &head, inject, allow, log, requests,
        )
        .await;
    }
    handle_plain(client, &method, &target, &head, allow, log, requests).await
}

/// Paid HTTP-forward mode: the worker's loopback request is mapped onto the
/// configured upstream base, sent over TLS with an injected bearer token, and
/// the response (including chunked/SSE) is streamed back verbatim. The upstream
/// host must be allowlisted; otherwise `403`.
#[allow(clippy::too_many_arguments)]
async fn handle_forward(
    mut client: TcpStream,
    method: &str,
    target: &str,
    head: &str,
    inject: &InjectConfig,
    allow: Arc<AllowList>,
    log: bool,
    requests: Arc<Mutex<Vec<ProxyRequest>>>,
) -> io::Result<()> {
    let path = request_path(target);
    let allowed = allow.is_allowed(&inject.host);
    record(&requests, method, &inject.host, inject.port, allowed, log);
    if !allowed {
        return respond(&mut client, 403, "Forbidden").await;
    }

    let upstream_path = join_base_path(&inject.base_path, &path);
    let rewritten = rewrite_forward_head(head, method, &upstream_path, inject);
    let addr = (inject.host.as_str(), inject.port);
    let tcp = match TcpStream::connect(addr).await {
        Ok(tcp) => tcp,
        Err(err) => {
            if log {
                eprintln!(
                    "provider-proxy: connect {}:{} failed: {err}",
                    inject.host, inject.port
                );
            }
            return respond(&mut client, 502, "Bad Gateway").await;
        }
    };

    if inject.tls {
        match tls_connect(&inject.host, tcp).await {
            Ok(mut upstream) => {
                upstream.write_all(rewritten.as_bytes()).await?;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
            Err(err) => {
                if log {
                    eprintln!("provider-proxy: TLS to {} failed: {err}", inject.host);
                }
                return respond(&mut client, 502, "Bad Gateway").await;
            }
        }
    } else {
        let mut upstream = tcp;
        upstream.write_all(rewritten.as_bytes()).await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }
    Ok(())
}

/// The origin-form path of a request target: absolute-form (`http://h/p`) →
/// `/p`; origin-form → itself; empty → `/`.
fn request_path(target: &str) -> String {
    if let Some(rest) = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
    {
        match rest.find(['/', '?']) {
            Some(idx) => rest[idx..].to_string(),
            None => "/".to_string(),
        }
    } else if target.is_empty() {
        "/".to_string()
    } else {
        target.to_string()
    }
}

/// Join the upstream base path with the request path
/// (`/zen/go/v1` + `/chat/completions` → `/zen/go/v1/chat/completions`).
fn join_base_path(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    if base.is_empty() {
        path.to_string()
    } else {
        format!("{base}{path}")
    }
}

/// Rebuild the request head for the upstream: origin-form request line, upstream
/// `Host`, hop-by-hop and inbound credential headers stripped, and
/// `Authorization: Bearer <token>` (plus `Connection: close`) injected.
fn rewrite_forward_head(
    head: &str,
    method: &str,
    upstream_path: &str,
    inject: &InjectConfig,
) -> String {
    let mut out = format!("{method} {upstream_path} HTTP/1.1\r\n");
    let mut lines = head.split("\r\n");
    let _request_line = lines.next();
    let mut has_session = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, _value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            // Hop-by-hop headers must not be forwarded.
            "connection"
            | "proxy-connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization" => continue,
            // Inbound credentials are stripped; the proxy injects its own.
            "authorization" | "x-api-key" | "x-goog-api-key" => continue,
            "host" => continue,
            "x-opencode-session" => has_session = true,
            _ => {}
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    let host_header = if inject.port == 443 || inject.port == 80 {
        inject.host.clone()
    } else {
        format!("{}:{}", inject.host, inject.port)
    };
    out.push_str(&format!("Host: {host_header}\r\n"));
    out.push_str(&format!("Authorization: Bearer {}\r\n", inject.token));
    // OpenCode Go requires a session id for routing/prompt caching; inject a
    // stable one when the client did not already provide its own.
    if !has_session {
        out.push_str(&format!("x-opencode-session: {}\r\n", inject.session_id));
    }
    // A single request/response per connection keeps the raw byte relay simple
    // and lets chunked/SSE responses terminate on upstream close.
    out.push_str("Connection: close\r\n\r\n");
    out
}

/// Build the rustls client config from the bundled webpki roots.
fn tls_client_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Open a TLS connection to `host` over an established TCP stream.
async fn tls_connect(
    host: &str,
    tcp: TcpStream,
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let connector = tokio_rustls::TlsConnector::from(tls_client_config());
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("server name: {e}")))?;
    connector.connect(server_name, tcp).await
}

/// `CONNECT host:port HTTP/1.1` — establish a raw tunnel after the allowlist check.
async fn handle_connect(
    mut client: TcpStream,
    target: &str,
    allow: Arc<AllowList>,
    log: bool,
    requests: Arc<Mutex<Vec<ProxyRequest>>>,
) -> io::Result<()> {
    let Some((host, port)) = split_host_port(target, 443) else {
        return respond(&mut client, 400, "Bad Request").await;
    };
    let allowed = allow.is_allowed(&host);
    record(
        &requests,
        &method_label("CONNECT"),
        &host,
        port,
        allowed,
        log,
    );

    if !allowed {
        return respond(&mut client, 403, "Forbidden").await;
    }
    let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(upstream) => upstream,
        Err(err) => {
            if log {
                eprintln!("provider-proxy: connect {host}:{port} failed: {err}");
            }
            return respond(&mut client, 502, "Bad Gateway").await;
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// Plain HTTP absolute-form (`GET http://host:port/path HTTP/1.1`): rewrite to
/// origin-form, forward, and relay the response. Admitted only for allowlisted
/// hosts; non-absolute (origin-form) requests are refused because the proxy has
/// no implicit destination.
async fn handle_plain(
    mut client: TcpStream,
    method: &str,
    target: &str,
    head: &str,
    allow: Arc<AllowList>,
    log: bool,
    requests: Arc<Mutex<Vec<ProxyRequest>>>,
) -> io::Result<()> {
    let Some((host, port, path)) = parse_absolute_uri(target) else {
        return respond(&mut client, 400, "Bad Request").await;
    };
    let allowed = allow.is_allowed(&host);
    record(&requests, method, &host, port, allowed, log);
    if !allowed {
        return respond(&mut client, 403, "Forbidden").await;
    }
    let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(upstream) => upstream,
        Err(err) => {
            if log {
                eprintln!("provider-proxy: connect {host}:{port} failed: {err}");
            }
            return respond(&mut client, 502, "Bad Gateway").await;
        }
    };
    let rewritten = rewrite_to_origin_form(head, method, &path);
    upstream.write_all(rewritten.as_bytes()).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// Read the request head (through `\r\n\r\n`) one byte at a time, so no body
/// bytes are consumed. Bounded by [`MAX_HEAD`].
async fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head exceeds limit",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Rebuild the request head's first line in origin-form, preserving headers.
fn rewrite_to_origin_form(head: &str, method: &str, path: &str) -> String {
    match head.split_once("\r\n") {
        Some((_line, rest)) => format!("{method} {path} HTTP/1.1\r\n{rest}"),
        None => format!("{method} {path} HTTP/1.1\r\n\r\n"),
    }
}

/// Parse `http://host[:port]/path` into `(host, port, path)`; port defaults to 80.
fn parse_absolute_uri(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri.strip_prefix("http://")?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(authority, 80)?;
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Some((host, port, path))
}

/// Split `host[:port]`, honouring `[ipv6]` literals; `default_port` when absent.
fn split_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = rest[..end].to_string();
        let port = rest[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            Some((host.to_string(), port.parse::<u16>().ok()?))
        }
        _ => Some((authority.to_string(), default_port)),
    }
}

/// Write a minimal error response and close.
async fn respond(stream: &mut TcpStream, code: u16, reason: &str) -> io::Result<()> {
    let body = format!("{code} {reason}\n");
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn method_label(method: &str) -> String {
    method.to_string()
}

fn record(
    requests: &Arc<Mutex<Vec<ProxyRequest>>>,
    method: &str,
    host: &str,
    port: u16,
    allowed: bool,
    log: bool,
) {
    if let Ok(mut guard) = requests.lock() {
        guard.push(ProxyRequest {
            method: method.to_string(),
            host: host.to_string(),
            port,
            allowed,
        });
    }
    if log {
        let decision = if allowed { "allow" } else { "deny" };
        eprintln!("provider-proxy: {decision} {method} {host}:{port}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_exact_and_suffix_matches() {
        let allow = AllowList::new(vec!["opencode.ai".to_string()]);
        assert!(allow.is_allowed("opencode.ai"));
        assert!(allow.is_allowed("api.opencode.ai"));
        assert!(allow.is_allowed("models.opencode.ai"));
        assert!(allow.is_allowed("OPENCODE.AI"));
        assert!(allow.is_allowed("opencode.ai."));
        // Look-alikes must not ride a suffix entry.
        assert!(!allow.is_allowed("notopencode.ai"));
        assert!(!allow.is_allowed("opencode.ai.evil.com"));
        assert!(!allow.is_allowed(""));
    }

    #[test]
    fn allowlist_parses_env_style_list() {
        let allow = AllowList::parse(" example.com , .internal.test ,, ");
        assert_eq!(allow.entries(), &["example.com", "internal.test"]);
        assert!(allow.is_allowed("internal.test"));
        assert!(allow.is_allowed("svc.internal.test"));
        assert!(!allow.is_allowed("other.test"));
    }

    #[test]
    fn split_host_port_handles_ports_and_ipv6() {
        assert_eq!(
            split_host_port("opencode.ai:443", 80),
            Some(("opencode.ai".to_string(), 443))
        );
        assert_eq!(
            split_host_port("opencode.ai", 80),
            Some(("opencode.ai".to_string(), 80))
        );
        assert_eq!(
            split_host_port("[::1]:8443", 443),
            Some(("::1".to_string(), 8443))
        );
        assert_eq!(split_host_port("", 80), None);
    }

    #[test]
    fn parse_absolute_uri_extracts_authority_and_path() {
        assert_eq!(
            parse_absolute_uri("http://opencode.ai/zen/v1"),
            Some(("opencode.ai".to_string(), 80, "/zen/v1".to_string()))
        );
        assert_eq!(
            parse_absolute_uri("http://127.0.0.1:8080"),
            Some(("127.0.0.1".to_string(), 8080, "/".to_string()))
        );
        assert_eq!(
            parse_absolute_uri("http://h/a?b=c"),
            Some(("h".to_string(), 80, "/a?b=c".to_string()))
        );
        assert_eq!(parse_absolute_uri("https://opencode.ai/"), None);
    }

    #[test]
    fn rewrite_to_origin_form_replaces_request_line() {
        let head = "GET http://opencode.ai/zen/v1 HTTP/1.1\r\nHost: opencode.ai\r\n\r\n";
        let out = rewrite_to_origin_form(head, "GET", "/zen/v1");
        assert_eq!(out, "GET /zen/v1 HTTP/1.1\r\nHost: opencode.ai\r\n\r\n");
    }

    use std::time::Duration;

    /// Start a loopback TCP echo server; returns `(addr, handle)`.
    async fn spawn_echo() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if socket.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        (addr, handle)
    }

    async fn read_head_text(stream: &mut TcpStream) -> String {
        read_head(stream).await.expect("read response head")
    }

    #[tokio::test]
    async fn connect_tunnels_bytes_to_allowlisted_upstream() {
        let (echo_addr, echo) = spawn_echo().await;
        let proxy = ProviderProxy::start(ProxyConfig::plain(
            AllowList::new(vec!["127.0.0.1".to_string()]),
            false,
        ))
        .expect("start proxy");

        let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .expect("connect proxy");
        let req = format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            echo_addr.port(),
            echo_addr.port()
        );
        client.write_all(req.as_bytes()).await.unwrap();
        let response = read_head_text(&mut client).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected tunnel established, got: {response:?}"
        );

        client.write_all(b"ping-through-proxy").await.unwrap();
        let mut echoed = vec![0u8; "ping-through-proxy".len()];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping-through-proxy");

        let log = proxy.requests();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].method, "CONNECT");
        assert_eq!(log[0].host, "127.0.0.1");
        assert_eq!(log[0].port, echo_addr.port());
        assert!(log[0].allowed);

        proxy.stop().await;
        echo.abort();
    }

    #[tokio::test]
    async fn connect_to_non_allowlisted_host_is_forbidden_and_closed() {
        let proxy = ProviderProxy::start(ProxyConfig::plain(
            AllowList::new(vec!["opencode.ai".to_string()]),
            false,
        ))
        .expect("start proxy");

        let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .expect("connect proxy");
        client
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
            .await
            .unwrap();
        let response = read_head_text(&mut client).await;
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "expected 403 for denied host, got: {response:?}"
        );

        // The response closes the connection: a follow-up read hits EOF.
        let mut tail = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut tail)).await;

        let log = proxy.requests();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].host, "example.com");
        assert!(!log[0].allowed);

        proxy.stop().await;
    }

    #[tokio::test]
    async fn plain_absolute_form_is_forwarded() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await.unwrap();
            assert!(
                head.starts_with("GET /ok HTTP/1.1"),
                "rewritten head: {head:?}"
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi")
                .await
                .unwrap();
        });

        let proxy = ProviderProxy::start(ProxyConfig::plain(
            AllowList::new(vec!["127.0.0.1".to_string()]),
            false,
        ))
        .expect("start proxy");
        let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .unwrap();
        let req = format!(
            "GET http://127.0.0.1:{}/ok HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            addr.port()
        );
        client.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        client.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "response: {text:?}");
        assert!(text.ends_with("hi"));

        let log = proxy.requests();
        assert_eq!(log[0].method, "GET");
        assert!(log[0].allowed);

        proxy.stop().await;
        let _ = server.await;
    }

    #[tokio::test]
    async fn stop_releases_the_bound_port() {
        let proxy = ProviderProxy::start(ProxyConfig::default()).expect("start proxy");
        let port = proxy.port();
        // Sanity: the port accepts while running.
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_ok());
        proxy.stop().await;

        // After stop the listener is gone: connecting must fail.
        let after = TcpStream::connect(("127.0.0.1", port)).await;
        assert!(
            after.is_err(),
            "port {port} still accepting after stop: {:?}",
            after.map(|_| ())
        );
    }

    #[tokio::test]
    async fn drop_releases_the_bound_port() {
        let port = {
            let proxy = ProviderProxy::start(ProxyConfig::default()).expect("start proxy");
            proxy.port()
        };
        // Give the aborted task a moment to drop the listener.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = TcpStream::connect(("127.0.0.1", port)).await;
        assert!(after.is_err(), "port {port} still accepting after drop");
    }

    // --- paid HTTP-forward mode -------------------------------------------

    #[test]
    fn inject_config_parses_base_url_and_redacts_secret_in_debug() {
        let inject =
            InjectConfig::from_base("https://opencode.ai/zen/go/v1", "super-secret-token").unwrap();
        assert_eq!(inject.host, "opencode.ai");
        assert_eq!(inject.port, 443);
        assert_eq!(inject.base_path, "/zen/go/v1");
        assert!(inject.tls);
        let debug = format!("{inject:?}");
        assert!(!debug.contains("super-secret-token"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn request_path_and_join_handle_origin_and_absolute_forms() {
        assert_eq!(request_path("/chat/completions"), "/chat/completions");
        assert_eq!(request_path("http://h/a?b=c"), "/a?b=c");
        assert_eq!(request_path(""), "/");
        assert_eq!(
            join_base_path("/zen/go/v1", "/chat/completions"),
            "/zen/go/v1/chat/completions"
        );
        assert_eq!(join_base_path("", "/x"), "/x");
        assert_eq!(join_base_path("/v1", "/"), "/v1/");
    }

    #[test]
    fn rewrite_forward_head_strips_inbound_credentials_and_injects_bearer() {
        let inject = InjectConfig::from_base("https://opencode.ai/zen/go/v1", "injected").unwrap();
        let head = "POST /chat/completions HTTP/1.1\r\n\
                    Host: 127.0.0.1:1234\r\n\
                    Authorization: Bearer inbound\r\n\
                    x-api-key: inbound-key\r\n\
                    Content-Type: application/json\r\n\
                    Connection: keep-alive\r\n\r\n";
        let out = rewrite_forward_head(head, "POST", "/zen/go/v1/chat/completions", &inject);
        assert!(out.starts_with("POST /zen/go/v1/chat/completions HTTP/1.1\r\n"));
        assert!(out.contains("Authorization: Bearer injected\r\n"));
        assert!(!out.contains("inbound"));
        assert!(!out.contains("x-api-key"));
        assert!(!out.contains("Host: 127.0.0.1:1234"));
        assert!(out.contains("Host: opencode.ai\r\n"));
        assert!(out.contains("Connection: close\r\n"));
        assert!(
            out.contains("x-opencode-session: "),
            "OpenCode Go session id injected: {out}"
        );

        // A client-supplied session id is preserved (not overwritten).
        let head2 = "POST /chat/completions HTTP/1.1\r\n\
                     x-opencode-session: ses_client_1\r\n\r\n";
        let out2 = rewrite_forward_head(head2, "POST", "/x", &inject);
        assert!(
            out2.contains("x-opencode-session: ses_client_1\r\n"),
            "{out2}"
        );
        assert!(
            !out2.contains(&inject.session_id),
            "must not inject a second session id: {out2}"
        );
    }

    /// Spawn a one-shot mock upstream that reads the request head, records it,
    /// writes `response`, and closes.
    async fn spawn_mock_upstream(
        response: &'static str,
    ) -> (SocketAddr, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
            head
        });
        (addr, handle)
    }

    fn forward_proxy(upstream: SocketAddr, token: &str) -> ProviderProxy {
        let mut config = ProxyConfig::with_injection(
            InjectConfig::from_base(
                &format!("http://127.0.0.1:{}/zen/go/v1", upstream.port()),
                token,
            )
            .expect("inject config"),
        );
        config.allow = AllowList::new(vec!["127.0.0.1".to_string()]);
        ProviderProxy::start(config).expect("start proxy")
    }

    #[tokio::test]
    async fn forward_injects_authorization_and_maps_path() {
        let (upstream, server) = spawn_mock_upstream(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
        )
        .await;
        let proxy = forward_proxy(upstream, "real-secret-token");

        let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .unwrap();
        // Origin-form loopback request, as OpenCode sends with a baseURL of the
        // loopback proxy.
        client
            .write_all(
                b"POST /chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                  Authorization: Bearer inbound\r\nContent-Length: 2\r\n\r\n{}",
            )
            .await
            .unwrap();
        let mut raw = Vec::new();
        client.read_to_end(&mut raw).await.unwrap();
        assert!(
            String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 200 OK"),
            "response: {}",
            String::from_utf8_lossy(&raw)
        );

        let head = server.await.unwrap();
        assert!(
            head.starts_with("POST /zen/go/v1/chat/completions HTTP/1.1"),
            "request path is mapped onto the upstream base: {head:?}"
        );
        assert!(
            head.contains("Authorization: Bearer real-secret-token\r\n"),
            "{head}"
        );
        assert!(
            !head.contains("inbound"),
            "inbound credential must be stripped: {head}"
        );

        // The audit fingerprint is recorded, but the token never leaves the proxy.
        let log = proxy.requests();
        assert_eq!(log.len(), 1);
        assert!(log[0].allowed);
        assert_eq!(log[0].host, "127.0.0.1");
        assert!(proxy.auth_state().present);
        assert!(!format!("{:?}", proxy.auth_state()).contains("real-secret-token"));

        proxy.stop().await;
    }

    #[tokio::test]
    async fn forward_denies_non_allowlisted_upstream() {
        let mut config = ProxyConfig::with_injection(
            InjectConfig::from_base("http://example.com:8080/v1", "tok").unwrap(),
        );
        config.allow = AllowList::new(vec!["opencode.ai".to_string()]);
        let proxy = ProviderProxy::start(config).expect("start proxy");

        let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .unwrap();
        client
            .write_all(b"POST /chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let response = read_head_text(&mut client).await;
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "expected 403, got {response:?}"
        );
        let log = proxy.requests();
        assert_eq!(log.len(), 1);
        assert!(!log[0].allowed);
        assert_eq!(log[0].host, "example.com");
        proxy.stop().await;
    }

    #[tokio::test]
    async fn forward_streams_chunked_response_verbatim() {
        let (upstream, _server) = spawn_mock_upstream(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
             5\r\nhello\r\n0\r\n\r\n",
        )
        .await;
        let proxy = forward_proxy(upstream, "tok");

        let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .unwrap();
        client
            .write_all(b"GET /models HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut raw = Vec::new();
        client.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("Transfer-Encoding: chunked"), "{text}");
        assert!(
            text.contains("5\r\nhello\r\n"),
            "chunked body streamed: {text}"
        );
        proxy.stop().await;
    }
}
