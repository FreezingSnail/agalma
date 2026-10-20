//! Common-plugin identity, health and snapshot handling for the dashboard.
//!
//! The dashboard speaks the canonical `plugin.Describe`/`plugin.Health`/
//! `plugin.Snapshot` protocol. Health distinguishes the two independent
//! readiness facts the process owns:
//!
//! - **listener**: the loopback HTTP listener is bound and serving;
//! - **binding**: the hub `control-plane.read/v1` read binding is established.
//!
//! `Ready` requires both. A bound listener with no usable read binding is
//! `Degraded`, never a healthy empty page; a failed listener is `Unavailable`.
//! The actual loopback URL appears only once the listener exists.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::{
    BindingPin, ContractError, FailureCode, HealthState, PluginHealth, PluginSnapshot,
    PLUGIN_HEALTH_FORMAT, PLUGIN_SNAPSHOT_FORMAT,
};
use agalma_ipc::{PluginHandler, PluginIdentity, PLUGIN_API, PLUGIN_PROTOCOL_MAJOR};
use serde_json::Value;

use crate::reports::ReportObservation;
use crate::{PACKAGE_VERSION, PLUGIN_ID};

/// A point-in-time view of the two readiness facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusSnapshot {
    /// The loopback HTTP listener is bound.
    pub listener: bool,
    /// The hub read binding is established.
    pub binding: bool,
    /// Actual loopback URL, once the listener exists.
    pub url: Option<String>,
    /// Latest binding/listener failure detail, if any.
    pub detail: Option<String>,
}

impl StatusSnapshot {
    /// The common-plugin health state for these facts.
    pub fn state(&self) -> HealthState {
        match (self.listener, self.binding) {
            (true, true) => HealthState::Ready,
            (true, false) => HealthState::Degraded,
            (false, _) => HealthState::Unavailable,
        }
    }

    /// The stable, parseable health detail line.
    ///
    /// Fields are `;`-separated so free-text detail never obscures the URL.
    pub fn detail_line(&self) -> String {
        let mut line = format!(
            "listener={};binding={}",
            readiness_word(self.listener),
            readiness_word(self.binding)
        );
        if self.listener && self.binding {
            if let Some(url) = &self.url {
                line.push_str(&format!(";url={url}"));
            }
        }
        if let Some(detail) = &self.detail {
            line.push_str(&format!(";detail={detail}"));
        }
        line
    }
}

fn readiness_word(ready: bool) -> &'static str {
    if ready {
        "ready"
    } else {
        "unavailable"
    }
}

#[derive(Debug)]
struct Inner {
    listener: bool,
    binding: bool,
    url: Option<String>,
    detail: Option<String>,
}

/// Shared runtime readiness owned by the process.
#[derive(Debug)]
pub struct RuntimeStatus {
    generation: u64,
    config_digest: String,
    inner: Mutex<Inner>,
}

impl RuntimeStatus {
    /// New status for a configuration with the given canonical digest.
    pub fn new(generation: u64, config_digest: impl Into<String>) -> Self {
        Self {
            generation,
            config_digest: config_digest.into(),
            inner: Mutex::new(Inner {
                listener: false,
                binding: false,
                url: None,
                detail: None,
            }),
        }
    }

    /// Record that the loopback listener is serving at `url`.
    pub fn listener_ready(&self, url: impl Into<String>) {
        let mut inner = self.lock();
        inner.listener = true;
        inner.url = Some(url.into());
    }

    /// Record that the listener is not usable.
    pub fn listener_unavailable(&self, detail: impl Into<String>) {
        let mut inner = self.lock();
        inner.listener = false;
        inner.url = None;
        inner.detail = Some(detail.into());
    }

    /// Record that the hub read binding is established.
    pub fn binding_ready(&self) {
        let mut inner = self.lock();
        inner.binding = true;
        inner.detail = None;
    }

    /// Record that the hub read binding is absent or failed.
    pub fn binding_unavailable(&self, detail: impl Into<String>) {
        let mut inner = self.lock();
        inner.binding = false;
        inner.detail = Some(detail.into());
    }

    /// The current readiness facts.
    pub fn snapshot(&self) -> StatusSnapshot {
        let inner = self.lock();
        StatusSnapshot {
            listener: inner.listener,
            binding: inner.binding,
            url: inner.url.clone(),
            detail: inner.detail.clone(),
        }
    }

    /// The canonical configuration digest reported in the component binding.
    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }

    /// The component binding this process reports.
    pub fn binding_pin(&self) -> BindingPin {
        BindingPin {
            binding_id: PLUGIN_ID.to_string(),
            api: PLUGIN_API.to_string(),
            api_version: PLUGIN_PROTOCOL_MAJOR as u64,
            implementation_version: PACKAGE_VERSION.to_string(),
            generation: self.generation,
            config_digest: self.config_digest.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The dashboard's common-plugin handler.
pub struct DashboardHandler {
    status: Arc<RuntimeStatus>,
    capabilities: Vec<String>,
    reporting: Arc<ReportObservation>,
}

impl DashboardHandler {
    /// Wrap the shared runtime status, the announced capability set and the
    /// acknowledged report boundary shared with the reporting task.
    ///
    /// The announced set is the launcher-issued capability descriptor (or the
    /// legacy single grant). It never contains a capability the hub did not
    /// grant this launch.
    pub fn new(
        status: Arc<RuntimeStatus>,
        capabilities: Vec<String>,
        reporting: Arc<ReportObservation>,
    ) -> Self {
        Self {
            status,
            capabilities,
            reporting,
        }
    }
}

impl PluginHandler for DashboardHandler {
    fn describe(&self) -> Result<Value, ContractError> {
        let identity = PluginIdentity {
            api: PLUGIN_API.to_string(),
            protocol_major: PLUGIN_PROTOCOL_MAJOR,
            plugin_id: PLUGIN_ID.to_string(),
            implementation_version: PACKAGE_VERSION.to_string(),
            capabilities: self.capabilities.clone(),
        };
        serde_json::to_value(identity).map_err(|error| {
            FailureCode::InvalidRecord.failure(PLUGIN_ID, format!("describe encode: {error}"))
        })
    }

    fn health(&self) -> Result<Value, ContractError> {
        let snapshot = self.status.snapshot();
        let health = PluginHealth {
            format: PLUGIN_HEALTH_FORMAT.to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding: self.status.binding_pin(),
            state: snapshot.state(),
            detail: snapshot.detail_line(),
            checked_at_unix_ms: now_unix_ms(),
        };
        serde_json::to_value(health).map_err(|error| {
            FailureCode::InvalidRecord.failure(PLUGIN_ID, format!("health encode: {error}"))
        })
    }

    fn snapshot(&self) -> Result<Value, ContractError> {
        // Only the acknowledged boundary is observable. A sealed but
        // unacknowledged sample never appears here, and a successful
        // publication never changes listener/binding facts.
        let observed = self.reporting.observed();
        let mut snapshot = PluginSnapshot {
            format: PLUGIN_SNAPSHOT_FORMAT.to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding: self.status.binding_pin(),
            watermark: observed.watermark,
            snapshots: observed.entry().into_iter().collect(),
            snapshot_digest: String::new(),
        };
        snapshot.snapshot_digest = snapshot.recomputed_snapshot_digest()?;
        serde_json::to_value(snapshot).map_err(|error| {
            FailureCode::InvalidRecord.failure(PLUGIN_ID, format!("snapshot encode: {error}"))
        })
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
