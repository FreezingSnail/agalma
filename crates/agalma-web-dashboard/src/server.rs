//! Dashboard process bootstrap and loopback read adapter.
//!
//! One process owns three cooperating parts:
//!
//! 1. the common-plugin RPC server on the launch `--socket` (READY handshake,
//!    Describe/Health/Snapshot);
//! 2. the WEB.3 loopback HTTP listener serving the compiled asset inventory and
//!    the two supported read routes;
//! 3. the scoped hub read binding used by those routes.
//!
//! The listener binds before the plugin RPC server announces READY, so an
//! invalid configuration or an occupied port fails startup explicitly instead
//! of advertising a listener that does not exist. The actual loopback URL is
//! recorded only after the listener binds and is reported through
//! `plugin.Health` only once the listener exists.
//!
//! A missing or failed hub read binding does not crash the process: the
//! listener stays useful and health is `Degraded`, while every read route
//! answers `503 read_unavailable`. A failed read is never turned into a healthy
//! empty page. Shutdown drains the HTTP listener under its five-second bound.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agalma_contracts::control_plane::ReadPageRequest;
use agalma_contracts::{
    failure_code, BrowserWire, FailureCode, ReadCursor, CONTROL_PLANE_READ_CAPABILITY,
};
use agalma_ipc::{
    plugin_server_info, register_plugin, ControlPlaneReadClient, IpcError, LoopbackListener,
    Server, WebApp, WebHandler, WebRequest, WebResponse,
};
use http::StatusCode;
use tokio::sync::Mutex;

use crate::config::{ConfigError, DashboardConfig};
use crate::inventory::HEALTH_PAGE;
use crate::plugin::{DashboardHandler, RuntimeStatus};
use crate::{PACKAGE_VERSION, PLUGIN_ID, READ_CAPABILITY};

/// Private launch env: the hub capability RPC socket (HP.3 launch channel).
pub const HUB_SOCKET_ENV: &str = "AGALMA_HUB_SOCKET";
/// Private launch env: the per-launch scoped grant token.
pub const PLUGIN_GRANT_ENV: &str = "AGALMA_PLUGIN_GRANT";
/// Private launch env: the capability the grant authorizes.
pub const PLUGIN_GRANT_CAPABILITY_ENV: &str = "AGALMA_PLUGIN_GRANT_CAPABILITY";
/// Private launch env: exact hub-installed configuration path.
pub const PLUGIN_CONFIG_PATH_ENV: &str = "AGALMA_PLUGIN_CONFIG_PATH";
/// Private launch env: host-pinned installed binding generation.
pub const PLUGIN_BINDING_GENERATION_ENV: &str = "AGALMA_PLUGIN_BINDING_GENERATION";
/// Private launch env: exact installed configuration artifact digest.
pub const PLUGIN_CONFIG_DIGEST_ENV: &str = "AGALMA_PLUGIN_CONFIG_DIGEST";

/// The read binding launch inputs. They are private: no value here is copied
/// into a browser response, a log line, or a health detail.
#[derive(Clone, Debug, Default)]
pub struct LaunchInputs {
    /// Hub capability RPC socket.
    pub hub_socket: Option<PathBuf>,
    /// Per-launch scoped grant token.
    pub grant: Option<String>,
    /// Capability the grant authorizes.
    pub capability: Option<String>,
    /// Exact path to the hub-installed configuration artifact.
    pub config_path: Option<PathBuf>,
    /// Pinned generation carried by the capability call.
    pub generation: u64,
    /// Exact installed configuration artifact digest carried by the host.
    pub config_digest: String,
    /// Whether the generation was present in canonical positive-decimal form.
    valid_generation: bool,
}

impl LaunchInputs {
    /// Read the private launch inputs from the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Read the private launch inputs from an injected lookup (tests never
    /// mutate the process environment).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let generation_value = lookup(PLUGIN_BINDING_GENERATION_ENV);
        let (generation, valid_generation) = match generation_value {
            Some(value) => match value.parse::<u64>() {
                Ok(generation) if generation > 0 && generation.to_string() == value => {
                    (generation, true)
                }
                _ => (0, false),
            },
            None => (0, false),
        };
        Self {
            hub_socket: lookup(HUB_SOCKET_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            grant: lookup(PLUGIN_GRANT_ENV).filter(|value| !value.is_empty()),
            capability: lookup(PLUGIN_GRANT_CAPABILITY_ENV).filter(|value| !value.is_empty()),
            config_path: lookup(PLUGIN_CONFIG_PATH_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            generation,
            config_digest: lookup(PLUGIN_CONFIG_DIGEST_ENV).unwrap_or_default(),
            valid_generation,
        }
    }

    /// Whether all immutable installed configuration and binding inputs are
    /// present and well-formed. This intentionally does not require a read
    /// capability: an unbound package may still serve its local health page.
    pub fn validate_installed_config_pin(&self) -> Result<(), ()> {
        if self
            .config_path
            .as_ref()
            .is_some_and(|path| path.is_absolute())
            && self.valid_generation
            && agalma_contracts::is_sha256(&self.config_digest)
        {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Whether a hub read binding was supplied.
    pub fn is_configured(&self) -> bool {
        self.hub_socket.is_some()
            && self.grant.is_some()
            && self.capability.as_deref() == Some(CONTROL_PLANE_READ_CAPABILITY)
            && self.validate_installed_config_pin().is_ok()
    }

    /// Establish the typed read binding.
    async fn connect(&self) -> Result<ControlPlaneReadClient, ()> {
        let socket = self.hub_socket.as_ref().ok_or(())?;
        let grant = self.grant.as_ref().ok_or(())?;
        ControlPlaneReadClient::connect(socket, grant.clone(), PLUGIN_ID, self.generation)
            .await
            .map_err(|_| ())
    }
}

/// A refused dashboard start.
#[derive(Debug)]
pub enum DashboardError {
    /// The configuration is invalid.
    Config(ConfigError),
    /// The compiled inventory or route table was refused by the transport.
    Assets(agalma_ipc::WebHttpError),
    /// The package compiled without its required health page.
    MissingHealthPage,
    /// The loopback listener could not bind (for example an occupied port).
    Bind(std::io::Error),
    /// A canonical projection failed.
    Projection(String),
    /// The plugin RPC server failed to bind or stopped unexpectedly.
    RpcServe,
}

impl std::fmt::Display for DashboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DashboardError::Config(error) => write!(f, "{error}"),
            DashboardError::Assets(error) => write!(f, "web application: {error}"),
            DashboardError::MissingHealthPage => {
                write!(f, "compiled asset inventory is missing {HEALTH_PAGE}")
            }
            DashboardError::Bind(error) => write!(f, "loopback listener bind failed: {error}"),
            DashboardError::Projection(detail) => write!(f, "projection failed: {detail}"),
            DashboardError::RpcServe => write!(f, "plugin RPC server failed"),
        }
    }
}

impl std::error::Error for DashboardError {}

/// The scoped read binding slot.
struct ReadSlot {
    launch: LaunchInputs,
    client: Option<ControlPlaneReadClient>,
    detail: Option<String>,
}

impl ReadSlot {
    fn new(launch: LaunchInputs) -> Self {
        Self {
            launch,
            client: None,
            detail: None,
        }
    }

    /// The established client, connecting lazily on first use.
    async fn client(&mut self) -> Result<&mut ControlPlaneReadClient, ()> {
        if self.client.is_none() {
            if !self.launch.is_configured() {
                self.detail = Some("hub read binding unavailable".to_string());
                return Err(());
            }
            match self.launch.connect().await {
                Ok(client) => self.client = Some(client),
                Err(()) => {
                    self.detail = Some("hub read binding unavailable".to_string());
                    return Err(());
                }
            }
        }
        Ok(self.client.as_mut().expect("client established above"))
    }

    /// Forget a failed connection so the next read retries the binding.
    fn failed(&mut self) {
        self.detail = Some("hub read binding unavailable".to_string());
        self.client = None;
    }
}

struct Shared {
    status: Arc<RuntimeStatus>,
    read: Mutex<ReadSlot>,
}

/// A running dashboard: the loopback listener plus its shared readiness.
pub struct DashboardServer {
    listener: Option<LoopbackListener>,
    status: Arc<RuntimeStatus>,
    shared: Arc<Shared>,
    url: String,
}

impl DashboardServer {
    /// Build the application and bind the loopback listener.
    ///
    /// Configuration is validated first, so a foreign address or an
    /// out-of-range interval is refused before a socket exists. An occupied
    /// explicit port surfaces as [`DashboardError::Bind`].
    pub async fn start(
        config: DashboardConfig,
        launch: LaunchInputs,
    ) -> Result<Self, DashboardError> {
        launch
            .validate_installed_config_pin()
            .map_err(|_| DashboardError::Config(ConfigError::InvalidInstalledPin))?;
        let pinned_config = crate::config::resolve_installed_config(
            launch
                .config_path
                .as_deref()
                .expect("validated config path"),
            &launch.config_digest,
        )
        .map_err(DashboardError::Config)?;
        if pinned_config != config {
            return Err(DashboardError::Config(
                ConfigError::InstalledConfigDigestMismatch,
            ));
        }
        config.validate().map_err(DashboardError::Config)?;
        let generation = launch.generation;
        let config_digest = launch.config_digest.clone();
        let status = Arc::new(RuntimeStatus::new(generation, config_digest));
        let shared = Arc::new(Shared {
            status: status.clone(),
            read: Mutex::new(ReadSlot::new(launch)),
        });
        let app = build_app(&config, shared.clone())?;
        let listener = LoopbackListener::start(app)
            .await
            .map_err(DashboardError::Bind)?;
        let url = listener.base_url();
        status.listener_ready(url.clone());
        Ok(Self {
            listener: Some(listener),
            status,
            shared,
            url,
        })
    }

    /// The actual bound loopback base URL (`http://127.0.0.1:<actual-port>`).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The shared readiness observation.
    pub fn status(&self) -> Arc<RuntimeStatus> {
        self.status.clone()
    }

    /// Establish (or report the failure of) the hub read binding.
    pub async fn warm_binding(&self) {
        let mut slot = self.shared.read.lock().await;
        let failure = match slot.client().await {
            Ok(client) => client.read_overview().await.err(),
            Err(()) => Some(IpcError::unavailable("read binding unavailable")),
        };
        drop(slot);
        match failure {
            None => self.status.binding_ready(),
            Some(_) => {
                self.shared.read.lock().await.failed();
                self.status
                    .binding_unavailable("hub read binding validation failed");
            }
        }
    }

    /// Stop admitting reads, drain the listener under its bound, and close it.
    pub async fn shutdown(mut self) {
        if let Some(listener) = self.listener.take() {
            listener.shutdown().await;
        }
    }
}

/// Assemble the WEB.3 application: compiled assets, root fallback and the two
/// supported read routes.
fn build_app(config: &DashboardConfig, shared: Arc<Shared>) -> Result<WebApp, DashboardError> {
    let inventory = crate::inventory::asset_inventory().map_err(DashboardError::Assets)?;
    let root_asset = if inventory.get(crate::inventory::INDEX_PAGE).is_some() {
        crate::inventory::INDEX_PAGE
    } else {
        HEALTH_PAGE
    };
    let (content_type, bytes) =
        crate::inventory::compiled_asset(root_asset).ok_or(DashboardError::MissingHealthPage)?;
    WebApp::builder(config.listener_config())
        .asset("/", content_type, bytes.to_vec())
        .assets(inventory)
        .get("/api/v1/overview", overview_handler(shared.clone()))
        .get("/api/v1/components", components_handler(shared))
        .build()
        .map_err(DashboardError::Assets)
}

fn overview_handler(shared: Arc<Shared>) -> impl WebHandler {
    move |_request: WebRequest| {
        let shared = shared.clone();
        async move { serve_overview(shared).await }
    }
}

fn components_handler(shared: Arc<Shared>) -> impl WebHandler {
    move |request: WebRequest| {
        let shared = shared.clone();
        async move { serve_components(shared, request).await }
    }
}

async fn serve_overview(shared: Arc<Shared>) -> WebResponse {
    let mut slot = shared.read.lock().await;
    let result = match slot.client().await {
        Ok(client) => client.read_overview().await,
        Err(()) => {
            shared
                .status
                .binding_unavailable("hub read binding unavailable");
            return unavailable();
        }
    };
    match result {
        Ok(overview) => match overview.to_wire_json() {
            Ok(text) => {
                shared.status.binding_ready();
                WebResponse::bytes(StatusCode::OK, "application/json", text.into_bytes())
            }
            Err(error) => {
                let _ = error;
                shared
                    .status
                    .binding_unavailable("hub read response unavailable");
                projection_error()
            }
        },
        Err(_) => {
            slot.failed();
            shared
                .status
                .binding_unavailable("hub read binding unavailable");
            unavailable()
        }
    }
}

async fn serve_components(shared: Arc<Shared>, request: WebRequest) -> WebResponse {
    let limit = match request.query_param("limit") {
        None => None,
        Some(raw) => match decode_query_component(raw)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
        {
            Some(limit) => Some(limit),
            None => {
                return WebResponse::error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "invalid components request",
                )
            }
        },
    };
    let cursor = match request.query_param("cursor") {
        None => None,
        Some(raw) => match decode_query_component(raw) {
            Ok(cursor) if ReadCursor::decode(&cursor).is_ok() => Some(cursor),
            _ => {
                return WebResponse::error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "invalid components request",
                )
            }
        },
    };
    let page_request = ReadPageRequest { limit, cursor };
    if page_request.effective_limit().is_err() {
        return WebResponse::error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid components request",
        );
    }

    let mut slot = shared.read.lock().await;
    let result = match slot.client().await {
        Ok(client) => client.list_components(&page_request).await,
        Err(()) => {
            shared
                .status
                .binding_unavailable("hub read binding unavailable");
            return unavailable();
        }
    };
    match result {
        Ok(page) => match page.to_wire_json() {
            Ok(text) => {
                shared.status.binding_ready();
                WebResponse::bytes(StatusCode::OK, "application/json", text.into_bytes())
            }
            Err(error) => {
                let _ = error;
                shared
                    .status
                    .binding_unavailable("hub read response unavailable");
                projection_error()
            }
        },
        Err(error) => match request_error_kind(&error) {
            RequestErrorKind::Invalid => WebResponse::error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid components request",
            ),
            RequestErrorKind::RevisionConflict => WebResponse::error(
                StatusCode::CONFLICT,
                "cursor_conflict",
                "cursor revision is no longer current",
            ),
            RequestErrorKind::DeadlineExceeded => WebResponse::error(
                StatusCode::GATEWAY_TIMEOUT,
                "read_timeout",
                "hub read request timed out",
            ),
            RequestErrorKind::Unavailable => {
                slot.failed();
                shared
                    .status
                    .binding_unavailable("hub read binding unavailable");
                unavailable()
            }
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestErrorKind {
    Invalid,
    RevisionConflict,
    DeadlineExceeded,
    Unavailable,
}

fn request_error_kind(error: &IpcError) -> RequestErrorKind {
    let IpcError::Contract(contract) = error else {
        return RequestErrorKind::Unavailable;
    };
    match failure_code(contract) {
        Some(FailureCode::InvalidRecord) => RequestErrorKind::Invalid,
        Some(FailureCode::RevisionConflict) => RequestErrorKind::RevisionConflict,
        Some(FailureCode::DeadlineExceeded) => RequestErrorKind::DeadlineExceeded,
        _ => RequestErrorKind::Unavailable,
    }
}

fn unavailable() -> WebResponse {
    WebResponse::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "read_unavailable",
        "hub read capability unavailable",
    )
}

fn projection_error() -> WebResponse {
    WebResponse::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "projection_failed",
        "hub read response could not be projected",
    )
}

/// Decode one RFC 3986 query component without treating `+` as a space. Opaque
/// cursor tokens are percent-decoded exactly once before contract validation.
fn decode_query_component(raw: &str) -> Result<String, ()> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes
                .get(index + 1)
                .and_then(|byte| hex_value(*byte))
                .ok_or(())?;
            let low = bytes
                .get(index + 2)
                .and_then(|byte| hex_value(*byte))
                .ok_or(())?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| ())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Run the dashboard until shutdown: bind, authenticate a typed read, then
/// expose plugin health/READY and drain on a signal or parent-death EOF.
pub async fn run(
    socket: PathBuf,
    config: DashboardConfig,
    launch: LaunchInputs,
) -> Result<(), DashboardError> {
    let server = DashboardServer::start(config, launch).await?;
    server.warm_binding().await;

    let handler = Arc::new(DashboardHandler::new(server.status()));
    let mut plugin = Server::new(plugin_server_info(
        PLUGIN_ID,
        PACKAGE_VERSION,
        [READ_CAPABILITY.to_string()],
    ));
    register_plugin(&mut plugin, handler);

    let serve_socket = socket.clone();
    let mut serve_task =
        tokio::spawn(async move { agalma_ipc::connect::serve(plugin, &serve_socket).await });

    // Warmup finishes before the plugin RPC server can announce READY, so the
    // host's first health probe observes the authenticated binding result.
    if !wait_for_socket(&socket, &serve_task).await {
        server
            .status()
            .listener_unavailable("plugin RPC server unavailable");
        server.shutdown().await;
        serve_task.abort();
        let _ = serve_task.await;
        let _ = std::fs::remove_file(&socket);
        return Err(DashboardError::RpcServe);
    }
    let status = server.status().snapshot();
    if status.listener && status.binding {
        if let Some(url) = status.url {
            // stdout is the supervisor's READY handshake channel and may be
            // closed as soon as READY is consumed.
            eprintln!("DASHBOARD_URL {url}");
        }
    }

    // Parent-death channel: the supervisor holds our stdin open and closes it
    // when the conductor dies. Keep the sender alive when the env is absent so
    // the receiver stays pending.
    let (death_tx, death_rx) = tokio::sync::oneshot::channel::<()>();
    let _death_guard = if std::env::var_os(agalma_ipc::PARENT_DEATH_STDIN_ENV).is_some() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut byte = [0u8; 1];
            loop {
                match std::io::stdin().read(&mut byte) {
                    Ok(0) | Err(_) => {
                        let _ = death_tx.send(());
                        return;
                    }
                    Ok(_) => {}
                }
            }
        });
        None
    } else {
        Some(death_tx)
    };

    let rpc_result = tokio::select! {
        _ = shutdown_signal() => None,
        _ = death_rx => None,
        result = &mut serve_task => Some(result),
    };

    match rpc_result {
        None => {
            server.shutdown().await;
            serve_task.abort();
            let _ = serve_task.await;
            let _ = std::fs::remove_file(&socket);
            Ok(())
        }
        Some(_) => {
            server
                .status()
                .listener_unavailable("plugin RPC server unavailable");
            server.shutdown().await;
            let _ = std::fs::remove_file(&socket);
            Err(DashboardError::RpcServe)
        }
    }
}

async fn wait_for_socket(
    path: &std::path::Path,
    serve_task: &tokio::task::JoinHandle<std::io::Result<()>>,
) -> bool {
    for _ in 0..200 {
        if serve_task.is_finished() {
            return false;
        }
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut terminate), Ok(mut interrupt)) => {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = interrupt.recv() => {}
                }
            }
            _ => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
