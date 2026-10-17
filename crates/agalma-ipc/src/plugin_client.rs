//! Typed Connect clients for the common plugin RPC (HP.4, `agalma-jmr.4`).
//!
//! [`PluginClient`] is the hub's client to a spoke's UDS (spoke methods);
//! [`HubCallbackClient`] is a spoke's client back into the hub (hub methods).
//! Both wrap the existing Connect envelope transport ([`crate::connect`]) and
//! reuse its receipt-replay semantics, so a transport re-send reuses the same
//! envelope request id and never re-runs an effect.
//!
//! Durable operation identity never comes from the envelope's numeric request
//! id: [`PluginClient::invoke`] and [`HubCallbackClient::invoke_capability`]
//! carry a caller-supplied `operation_id` inside the request payload. Two
//! independent clients that happen to allocate the same numeric transport id
//! therefore cannot collide as durable operation identity.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connect::ConnectClient;
use crate::error::IpcError;
use crate::plugin_server::{
    METHOD_CANCEL, METHOD_DESCRIBE, METHOD_HEALTH, METHOD_INVOKE, METHOD_PUBLISH_REPORT,
    METHOD_RECONCILE, METHOD_RECONCILE_CAPABILITY, METHOD_INVOKE_CAPABILITY, METHOD_SNAPSHOT,
    PLUGIN_API, PLUGIN_PROTOCOL_MAJOR,
};

/// The `plugin.Describe` identity and protocol agreement record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginIdentity {
    /// Announced `api` (must be [`PLUGIN_API`]).
    pub api: String,
    /// Announced protocol major (must be [`PLUGIN_PROTOCOL_MAJOR`]).
    pub protocol_major: u32,
    /// The bound plugin id.
    pub plugin_id: String,
    pub implementation_version: String,
    pub capabilities: Vec<String>,
}

/// The hub's typed client to one spoke's common plugin RPC over Connect.
pub struct PluginClient {
    client: ConnectClient,
}

impl PluginClient {
    /// Connect to the plugin's UDS and speak the common plugin protocol.
    pub async fn connect(socket: &Path) -> Result<PluginClient, IpcError> {
        Ok(PluginClient {
            client: ConnectClient::connect(socket).await?,
        })
    }

    /// `plugin.Describe`.
    pub async fn describe(&mut self) -> Result<PluginIdentity, IpcError> {
        let value = self.client.call(METHOD_DESCRIBE, Value::Null).await?;
        serde_json::from_value(value)
            .map_err(|e| IpcError::protocol(format!("describe identity decode: {e}")))
    }

    /// Protocol agreement: Describe, then refuse an api/protocol-major/identity
    /// mismatch before any effect. This is the "Connect protocol agreement"
    /// gate the common host runs before it treats a peer as Ready.
    pub async fn handshake(&mut self, expected_plugin_id: &str) -> Result<PluginIdentity, IpcError> {
        let identity = self.describe().await?;
        if identity.api != PLUGIN_API {
            return Err(IpcError::protocol(format!(
                "plugin api `{}` is not `{PLUGIN_API}`",
                identity.api
            )));
        }
        if identity.protocol_major != PLUGIN_PROTOCOL_MAJOR {
            return Err(IpcError::Contract(
                agalma_contracts::ContractError::UnsupportedCapability(format!(
                    "plugin protocol major {} is unsupported (expected {PLUGIN_PROTOCOL_MAJOR})",
                    identity.protocol_major
                )),
            ));
        }
        if identity.plugin_id != expected_plugin_id {
            return Err(IpcError::Contract(
                agalma_contracts::ContractError::KnownFailure(format!(
                    "plugin identity `{}` does not match expected `{expected_plugin_id}`",
                    identity.plugin_id
                )),
            ));
        }
        Ok(identity)
    }

    /// `plugin.Health`.
    pub async fn health(&mut self) -> Result<Value, IpcError> {
        self.client.call(METHOD_HEALTH, Value::Null).await
    }

    /// `plugin.Snapshot`.
    pub async fn snapshot(&mut self) -> Result<Value, IpcError> {
        self.client.call(METHOD_SNAPSHOT, Value::Null).await
    }

    /// `plugin.Invoke`. `request` carries the caller-supplied durable
    /// `operation_id`; the transport id is assigned here and is never the
    /// durable identity.
    pub async fn invoke(&mut self, request: Value) -> Result<Value, IpcError> {
        self.client.call(METHOD_INVOKE, request).await
    }

    /// `plugin.Reconcile`.
    pub async fn reconcile(&mut self, request: Value) -> Result<Value, IpcError> {
        self.client.call(METHOD_RECONCILE, request).await
    }

    /// `plugin.Cancel`.
    pub async fn cancel(&mut self, request: Value) -> Result<Value, IpcError> {
        self.client.call(METHOD_CANCEL, request).await
    }

    /// The next auto-allocated transport id (test/diagnostics).
    pub fn next_transport_id(&self) -> u64 {
        self.client.next_id()
    }
}

/// A spoke's typed client back into the hub's common RPC over Connect.
///
/// A spoke uses this to publish a report or invoke an authorized hub capability
/// *before* its enclosing action completes; the hub must serve it without
/// holding the dispatcher lock across the outbound call (see
/// [`crate::connect::IpcDispatcher`]).
pub struct HubCallbackClient {
    client: ConnectClient,
}

impl HubCallbackClient {
    /// Connect to the hub's common RPC socket.
    pub async fn connect(socket: &Path) -> Result<HubCallbackClient, IpcError> {
        Ok(HubCallbackClient {
            client: ConnectClient::connect(socket).await?,
        })
    }

    /// `hub.PublishReport`.
    pub async fn publish_report(&mut self, request: Value) -> Result<Value, IpcError> {
        self.client.call(METHOD_PUBLISH_REPORT, request).await
    }

    /// `hub.InvokeCapability`.
    pub async fn invoke_capability(&mut self, request: Value) -> Result<Value, IpcError> {
        self.client.call(METHOD_INVOKE_CAPABILITY, request).await
    }

    /// `hub.ReconcileCapability`.
    pub async fn reconcile_capability(&mut self, request: Value) -> Result<Value, IpcError> {
        self.client.call(METHOD_RECONCILE_CAPABILITY, request).await
    }
}
