//! Common plugin RPC method registry and dispatch seams (HP.4, `agalma-jmr.4`).
//!
//! This module is the transport half of the frozen common plugin protocol
//! (`docs/self-mutation-contracts.md` §"HP: common RPC and callback behavior").
//! It defines the D1-frozen method names and the two directions of the protocol
//! over the existing Connect envelope ([`crate::connect`]):
//!
//! * **Spoke methods** (hub → plugin): `plugin.Describe`, `plugin.Health`,
//!   `plugin.Snapshot`, `plugin.Invoke`, `plugin.Reconcile`, `plugin.Cancel`.
//! * **Hub methods** (plugin → hub): `hub.PublishReport`,
//!   `hub.InvokeCapability`, `hub.ReconcileCapability`.
//!
//! The hub-side capability routing (grants, method/deadline/binding validation,
//! durable operation identity, stop admission) lives in the conductor
//! (`agalma-conductor`); this crate only carries the method names and the
//! typed request/response plumbing. `agalma-ipc` does not depend on the
//! conductor, so [`register_hub`] is generic over the [`HubHandler`] trait the
//! router implements, and [`register_plugin`] over the [`PluginHandler`] a
//! spoke implements.
//!
//! No new external crate is introduced; the generic host authenticates every
//! hub call with a per-launch scoped grant before any effect.

use std::sync::Arc;

use agalma_contracts::ContractError;
use serde_json::Value;

use crate::server::{Server, ServerInfo};

/// The `READY`/Describe `api` name every common plugin protocol peer announces.
pub const PLUGIN_API: &str = "plugin";
/// The common plugin protocol major this build understands.
pub const PLUGIN_PROTOCOL_MAJOR: u32 = 1;

/// Spoke method: identity + declared contract (hub → plugin).
pub const METHOD_DESCRIBE: &str = "plugin.Describe";
/// Spoke method: observed health (hub → plugin).
pub const METHOD_HEALTH: &str = "plugin.Health";
/// Spoke method: recover a durable report snapshot (hub → plugin).
pub const METHOD_SNAPSHOT: &str = "plugin.Snapshot";
/// Spoke method: journal + acknowledge an effectful action (hub → plugin).
pub const METHOD_INVOKE: &str = "plugin.Invoke";
/// Spoke method: reconcile a durable operation (hub → plugin).
pub const METHOD_RECONCILE: &str = "plugin.Reconcile";
/// Spoke method: request cancellation of a pinned operation (hub → plugin).
pub const METHOD_CANCEL: &str = "plugin.Cancel";

/// Hub method: publish a producer report (plugin → hub).
pub const METHOD_PUBLISH_REPORT: &str = "hub.PublishReport";
/// Hub method: invoke an authorized hub capability (plugin → hub).
pub const METHOD_INVOKE_CAPABILITY: &str = "hub.InvokeCapability";
/// Hub method: reconcile an authorized hub capability (plugin → hub).
pub const METHOD_RECONCILE_CAPABILITY: &str = "hub.ReconcileCapability";

/// Every spoke method, in the frozen order.
pub const SPOKE_METHODS: &[&str] = &[
    METHOD_DESCRIBE,
    METHOD_HEALTH,
    METHOD_SNAPSHOT,
    METHOD_INVOKE,
    METHOD_RECONCILE,
    METHOD_CANCEL,
];

/// Every hub method, in the frozen order.
pub const HUB_METHODS: &[&str] = &[
    METHOD_PUBLISH_REPORT,
    METHOD_INVOKE_CAPABILITY,
    METHOD_RECONCILE_CAPABILITY,
];

/// Whether `method` is a spoke method.
pub fn is_spoke_method(method: &str) -> bool {
    SPOKE_METHODS.contains(&method)
}

/// Whether `method` is a hub method.
pub fn is_hub_method(method: &str) -> bool {
    HUB_METHODS.contains(&method)
}

/// The `READY` identity a common plugin server announces.
///
/// `impl_name` is the exact `plugin_id` so the supervisor's existing identity
/// gate ([`crate::server::ServerInfo`]) checks the plugin identity, and
/// `api_version` is the common protocol major so a version-mismatched peer is
/// refused at the handshake.
pub fn plugin_server_info(
    plugin_id: &str,
    implementation_version: &str,
    capabilities: impl IntoIterator<Item = String>,
) -> ServerInfo {
    ServerInfo {
        api: PLUGIN_API.to_string(),
        api_version: PLUGIN_PROTOCOL_MAJOR,
        impl_name: plugin_id.to_string(),
        impl_version: implementation_version.to_string(),
        capabilities: capabilities.into_iter().collect(),
    }
}

/// The spoke side of the common protocol: a plugin implements these.
///
/// Every method receives/returns JSON matching the canonical contract records
/// (`PluginHealth/v1`, `PluginSnapshot/v1`, `PluginOperationStatus/v1`). Methods
/// a plugin does not implement return a typed `UnsupportedCapability` rather
/// than an arbitrary command.
pub trait PluginHandler: Send + Sync {
    /// `plugin.Describe`: manifest + actual bound identity.
    fn describe(&self) -> Result<Value, ContractError>;
    /// `plugin.Health`: `PluginHealth/v1`.
    fn health(&self) -> Result<Value, ContractError>;
    /// `plugin.Snapshot`: `PluginSnapshot/v1`.
    fn snapshot(&self) -> Result<Value, ContractError>;
    /// `plugin.Invoke`: journal + acknowledge an action.
    fn invoke(&self, _request: Value) -> Result<Value, ContractError> {
        Err(unsupported(METHOD_INVOKE))
    }
    /// `plugin.Reconcile`: `PluginOperationStatus/v1`.
    fn reconcile(&self, _request: Value) -> Result<Value, ContractError> {
        Err(unsupported(METHOD_RECONCILE))
    }
    /// `plugin.Cancel`: `PluginCancelRequest/v1` → status.
    fn cancel(&self, _request: Value) -> Result<Value, ContractError> {
        Err(unsupported(METHOD_CANCEL))
    }
}

/// Register every spoke method on `server`, dispatching to `handler`.
pub fn register_plugin(server: &mut Server, handler: Arc<dyn PluginHandler>) {
    let describe = handler.clone();
    server.on(METHOD_DESCRIBE, move |_params: Value| describe.describe());
    let health = handler.clone();
    server.on(METHOD_HEALTH, move |_params: Value| health.health());
    let snapshot = handler.clone();
    server.on(METHOD_SNAPSHOT, move |_params: Value| snapshot.snapshot());
    let invoke = handler.clone();
    server.on(METHOD_INVOKE, move |params: Value| invoke.invoke(params));
    let reconcile = handler.clone();
    server.on(METHOD_RECONCILE, move |params: Value| reconcile.reconcile(params));
    server.on(METHOD_CANCEL, move |params: Value| handler.cancel(params));
}

/// The hub side of the common protocol: the conductor's capability router.
///
/// The router authenticates the per-launch grant, validates the method,
/// capability, generation, deadline and input, applies stop admission, and
/// routes to a hub-owned typed adapter. A transport failure is a typed
/// [`ContractError`]; every refusal happens before any effect.
pub trait HubHandler: Send + Sync {
    /// `hub.PublishReport`: ingest a producer publication.
    fn publish_report(&self, _request: Value) -> Result<Value, ContractError> {
        Err(unsupported(METHOD_PUBLISH_REPORT))
    }
    /// `hub.InvokeCapability`: route an authorized capability call.
    fn invoke_capability(&self, _request: Value) -> Result<Value, ContractError> {
        Err(unsupported(METHOD_INVOKE_CAPABILITY))
    }
    /// `hub.ReconcileCapability`: durable operation status.
    fn reconcile_capability(&self, _request: Value) -> Result<Value, ContractError> {
        Err(unsupported(METHOD_RECONCILE_CAPABILITY))
    }
}

/// Register every hub method on `server`, dispatching to `handler`.
pub fn register_hub(server: &mut Server, handler: Arc<dyn HubHandler>) {
    let publish = handler.clone();
    server.on(METHOD_PUBLISH_REPORT, move |params: Value| {
        publish.publish_report(params)
    });
    let invoke = handler.clone();
    server.on(METHOD_INVOKE_CAPABILITY, move |params: Value| {
        invoke.invoke_capability(params)
    });
    server.on(METHOD_RECONCILE_CAPABILITY, move |params: Value| {
        handler.reconcile_capability(params)
    });
}

/// The typed `UnsupportedCapability` a default method returns.
fn unsupported(method: &str) -> ContractError {
    ContractError::UnsupportedCapability(format!("plugin peer does not implement `{method}`"))
}
