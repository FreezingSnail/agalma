//! Typed factory runtime/control client and private launch-binding descriptor
//! (WEB.12, `agalma-rwn.12`).
//!
//! This module is the factory half of the browser adapter described in
//! `docs/web-dashboard-design.md` ("Private launch bindings and HTTP controls").
//! It adds:
//!
//! - [`FactoryRuntimeClient`], a typed client for the WEB.9
//!   `FactoryRuntime/v1` snapshot and the `FactoryControl/v1`
//!   lookup/pause/resume operations. It reuses the existing locked IPC stack
//!   ([`HubCallbackClient`], [`with_deadline`]) and adds no client dependency.
//! - the private `PluginCapabilityBindings/v1` launch descriptor
//!   ([`CapabilityBindings`]) parsed from
//!   [`AGALMA_PLUGIN_CAPABILITY_BINDINGS`]. Bindings stay server-side: the grant
//!   is never returned, printed in a `Debug` form, or placed in a browser
//!   response.
//! - typed HTTP route handlers ([`register_factory_routes`]) that map the WEB.9
//!   typed responses and failures onto the exact HTTP statuses from the design.
//!
//! Authority rules:
//!
//! - `factory.read/v1` (snapshot/lookup) and `factory.control/v1` (pause/resume)
//!   are independent bindings. A missing control binding disables pause/resume;
//!   it never falls back to a read/report/legacy grant.
//! - a legacy single-grant (read-only) client may still call snapshot/lookup,
//!   but it holds no control authority.
//! - the client preserves the caller's original action id and confirmation
//!   exactly (the digest identity lives in the WEB.9 contract) and never
//!   generates a replacement id or automatically retries an effect.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::control_plane::ExactU64;
use agalma_contracts::factory::{
    FactoryActionRequest, FactoryControlRequest, FactoryControlResponse,
    FactoryControlUnconfirmedResponse, FactoryControlResult, FactoryLookupRequest, FactoryRuntime,
    FactoryRuntimeRequest, FactoryUnconfirmedCode, FACTORY_CONTROL_CAPABILITY,
    FACTORY_CONTROL_FORMAT, FACTORY_READ_CAPABILITY, FACTORY_RUNTIME_FORMAT,
};
use agalma_contracts::{failure_code, ContractError, FailureCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::control_plane_client::{with_deadline, CONNECT_TIMEOUT, READ_DEADLINE};
use crate::error::IpcError;
use crate::plugin_client::HubCallbackClient;
use crate::plugin_server::METHOD_INVOKE_CAPABILITY;
use crate::web_http::{
    decode_query_value, ControlAuthority, WebAppBuilder, WebFuture, WebRequest, WebResponse,
    CSRF_HEADER,
};

/// Prefix for the private launch-binding descriptor environment variable.
pub const PLUGIN_CAPABILITY_BINDINGS_ENV: &str = "AGALMA_PLUGIN_CAPABILITY_BINDINGS";
/// The private capability-binding descriptor format.
pub const PLUGIN_CAPABILITY_BINDINGS_FORMAT: &str = "PluginCapabilityBindings/v1";
/// Basename of the descriptor format family.
pub const PLUGIN_CAPABILITY_BINDINGS_FAMILY: &str = "PluginCapabilityBindings";
/// Maximum descriptor size (64 KiB).
pub const PLUGIN_CAPABILITY_BINDINGS_MAX_BYTES: usize = 64 * 1024;
/// Maximum unique capability entries in one descriptor.
pub const PLUGIN_CAPABILITY_BINDINGS_MAX_ENTRIES: usize = 16;
/// Maximum capability name (128 bytes).
pub const CAPABILITY_NAME_MAX_BYTES: usize = 128;
/// Maximum bound plugin id (256 bytes).
pub const BINDING_PLUGIN_ID_MAX_BYTES: usize = 256;
/// Maximum bound hub socket path (4096 bytes).
pub const BINDING_SOCKET_MAX_BYTES: usize = 4096;
/// Canonical launch-grant token length: 64 hex characters encoding the existing
/// 32-byte per-launch grant (`GRANT_TOKEN_BYTES`).
pub const BINDING_GRANT_HEX_CHARS: usize = 64;
/// The operation-id namespace this client stamps on capability calls.
pub const FACTORY_CLIENT_API: &str = "FactoryRuntimeClient/v1";

/// One private capability binding: a scoped grant for one capability at one
/// installed plugin generation over the hub's common RPC socket.
///
/// `Debug` deliberately redacts the socket, grant and any token-shaped field:
/// the descriptor is a private channel and must never leak into a log.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityBinding {
    /// The bound capability (exactly `factory.read/v1` or `factory.control/v1`).
    pub capability: String,
    /// The private hub RPC socket path.
    pub hub_socket: String,
    /// The scoped grant token (private; never disclosed).
    pub grant: String,
    /// The installed plugin id the binding is pinned to.
    pub plugin_id: String,
    /// The exact binding generation.
    pub generation: ExactU64,
}

impl std::fmt::Debug for CapabilityBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityBinding")
            .field("capability", &self.capability)
            .field("hub_socket", &"<redacted>")
            .field("grant", &"<redacted>")
            .field("plugin_id", &self.plugin_id)
            .field("generation", &self.generation.as_str())
            .finish()
    }
}

/// The private `PluginCapabilityBindings/v1` descriptor: at most sixteen unique
/// capability entries.
#[derive(Clone, Debug, Default)]
pub struct CapabilityBindings {
    by_capability: BTreeMap<String, CapabilityBinding>,
}

impl CapabilityBindings {
    /// The binding for `capability`, if present.
    pub fn get(&self, capability: &str) -> Option<&CapabilityBinding> {
        self.by_capability.get(capability)
    }

    /// The required `factory.read/v1` binding.
    pub fn read(&self) -> Option<&CapabilityBinding> {
        self.get(FACTORY_READ_CAPABILITY)
    }

    /// The optional `factory.control/v1` binding.
    pub fn control(&self) -> Option<&CapabilityBinding> {
        self.get(FACTORY_CONTROL_CAPABILITY)
    }

    /// Number of bindings.
    pub fn len(&self) -> usize {
        self.by_capability.len()
    }

    /// Whether the descriptor holds no bindings.
    pub fn is_empty(&self) -> bool {
        self.by_capability.is_empty()
    }
}

/// The on-the-wire descriptor object. Kept private so a malformed body can
/// never be constructed without going through [`parse_capability_bindings`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingDescriptor {
    format: String,
    bindings: Vec<CapabilityBinding>,
}

/// Gate the descriptor format: right family/wrong version is
/// [`FailureCode::UnsupportedVersion`]; an unrecognized family is
/// [`FailureCode::UnsupportedSchema`].
///
/// Only the exact canonical `PluginCapabilityBindings/v1` spelling is accepted.
/// A noncanonical alias such as `v01`, `V1`, or a trailing token is refused,
/// matching the contract `parse_format_version` rule used elsewhere.
fn gate_bindings_format(format: &str) -> Result<(), ContractError> {
    let (family, version) = format.split_once('/').ok_or_else(|| {
        FailureCode::UnsupportedSchema.failure(
            PLUGIN_CAPABILITY_BINDINGS_FORMAT,
            format!("{format:?} has no `/version`"),
        )
    })?;
    if family != PLUGIN_CAPABILITY_BINDINGS_FAMILY {
        return Err(FailureCode::UnsupportedSchema.failure(
            PLUGIN_CAPABILITY_BINDINGS_FORMAT,
            format!("unrecognized capability-binding family {family:?}"),
        ));
    }
    if version != "v1" {
        return Err(FailureCode::UnsupportedVersion.failure(
            PLUGIN_CAPABILITY_BINDINGS_FORMAT,
            format!(
                "unsupported capability-binding version {version:?} (expected canonical `v1`)"
            ),
        ));
    }
    Ok(())
}

/// Parse and validate a `PluginCapabilityBindings/v1` descriptor.
///
/// The descriptor is capped at 64 KiB and sixteen unique capabilities; a
/// duplicate, malformed, or overlong binding identity is refused before any
/// value reaches a caller.
pub fn parse_capability_bindings(raw: &str) -> Result<CapabilityBindings, ContractError> {
    if raw.len() > PLUGIN_CAPABILITY_BINDINGS_MAX_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            "capability_bindings",
            format!(
                "descriptor is {} bytes, exceeding the {PLUGIN_CAPABILITY_BINDINGS_MAX_BYTES}-byte bound",
                raw.len()
            ),
        ));
    }
    let descriptor: BindingDescriptor = serde_json::from_str(raw).map_err(|e| {
        FailureCode::InvalidRecord.failure(
            "capability_bindings",
            format!("malformed descriptor: {e}"),
        )
    })?;
    gate_bindings_format(&descriptor.format)?;
    if descriptor.bindings.len() > PLUGIN_CAPABILITY_BINDINGS_MAX_ENTRIES {
        return Err(FailureCode::InvalidRecord.failure(
            "capability_bindings",
            format!(
                "holds {} bindings, exceeding the {PLUGIN_CAPABILITY_BINDINGS_MAX_ENTRIES}-entry bound",
                descriptor.bindings.len()
            ),
        ));
    }
    let mut by_capability = BTreeMap::new();
    for binding in descriptor.bindings {
        validate_binding(&binding)?;
        if by_capability
            .insert(binding.capability.clone(), binding)
            .is_some()
        {
            return Err(FailureCode::InvalidRecord.failure(
                "capability_bindings",
                "duplicate capability entry",
            ));
        }
    }
    Ok(CapabilityBindings { by_capability })
}

/// Validate one binding's identity bounds. The capability is validated as a
/// bounded opaque name; the caller selects the capability it needs by exact
/// match. The socket and grant carry the descriptor's launch constraints: a
/// bounded, NUL-free absolute socket path and the canonical 64-hex (32-byte)
/// launch-grant token.
fn validate_binding(binding: &CapabilityBinding) -> Result<(), ContractError> {
    if binding.capability.is_empty() || binding.capability.len() > CAPABILITY_NAME_MAX_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            "capability",
            format!(
                "is {} bytes, expected 1..={CAPABILITY_NAME_MAX_BYTES}",
                binding.capability.len()
            ),
        ));
    }
    // The bound plugin id is an opaque configured id: nonempty, bounded, and
    // never a private filesystem path or traversal.
    if binding.plugin_id.is_empty() || binding.plugin_id.len() > BINDING_PLUGIN_ID_MAX_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            "plugin_id",
            format!(
                "is {} bytes, expected 1..={BINDING_PLUGIN_ID_MAX_BYTES}",
                binding.plugin_id.len()
            ),
        ));
    }
    if looks_like_private_path(&binding.plugin_id) {
        return Err(FailureCode::InvalidRecord.failure(
            "plugin_id",
            "must be an opaque identifier, not a private path",
        ));
    }
    validate_socket_path(&binding.hub_socket)?;
    validate_grant(&binding.grant)?;
    // `ExactU64` has already validated the generation decimal string.
    binding.generation.as_u64().map_err(|_| {
        FailureCode::InvalidRecord.failure("generation", "must be an exact unsigned integer")
    })?;
    Ok(())
}

/// Require a bounded, NUL-free, absolute hub socket path.
fn validate_socket_path(socket: &str) -> Result<(), ContractError> {
    if socket.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            "hub_socket",
            "must not be empty",
        ));
    }
    if socket.len() > BINDING_SOCKET_MAX_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            "hub_socket",
            format!(
                "is {} bytes, exceeding the {BINDING_SOCKET_MAX_BYTES}-byte bound",
                socket.len()
            ),
        ));
    }
    if !socket.starts_with('/') {
        return Err(FailureCode::InvalidRecord.failure(
            "hub_socket",
            "must be an absolute socket path",
        ));
    }
    if socket.contains('\0') {
        return Err(FailureCode::InvalidRecord.failure(
            "hub_socket",
            "must not contain a NUL byte",
        ));
    }
    Ok(())
}

/// Require the canonical 64-hex-character (32-byte) launch-grant token.
///
/// The grant is the existing per-launch capability grant representation
/// (`GRANT_TOKEN_BYTES` = 32 bytes rendered as lowercase/uppercase hex). It is
/// never an arbitrary nonempty string: the exact length and hex alphabet are
/// enforced so a malformed descriptor cannot smuggle a non-token value.
fn validate_grant(grant: &str) -> Result<(), ContractError> {
    if grant.len() != BINDING_GRANT_HEX_CHARS {
        return Err(FailureCode::InvalidRecord.failure(
            "grant",
            format!(
                "is {} characters, expected exactly {BINDING_GRANT_HEX_CHARS} for the 32-byte hex launch token",
                grant.len()
            ),
        ));
    }
    if !grant.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(FailureCode::InvalidRecord.failure(
            "grant",
            "must be a 64-character hex launch token",
        ));
    }
    Ok(())
}

/// Whether `raw` looks like a private absolute filesystem path or traversal.
fn looks_like_private_path(raw: &str) -> bool {
    if raw.starts_with('/') || raw.starts_with('\\') {
        return true;
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        if bytes[2] == b'\\' || bytes[2] == b'/' {
            return true;
        }
    }
    raw.split(['/', '\\']).any(|segment| segment == "..")
}

/// Read the private launch-binding descriptor from the environment. A missing
/// variable is `None`; a present-but-malformed or non-Unicode descriptor is an
/// error (a non-Unicode value is refused rather than silently treated as
/// absent, which would drop the binding).
pub fn capability_bindings_from_env() -> Result<Option<CapabilityBindings>, ContractError> {
    match std::env::var(PLUGIN_CAPABILITY_BINDINGS_ENV) {
        Ok(raw) => parse_capability_bindings(&raw).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(FailureCode::InvalidRecord.failure(
            PLUGIN_CAPABILITY_BINDINGS_ENV,
            "environment value is not valid Unicode",
        )),
    }
}

/// One connected capability binding (connection + private credentials).
struct Binding {
    client: HubCallbackClient,
    grant: String,
    plugin_id: String,
    generation: u64,
}

/// The `hub.InvokeCapability` request this client sends.
#[derive(Serialize)]
struct CapabilityCall<'a> {
    grant: &'a str,
    plugin_id: &'a str,
    generation: u64,
    method: &'a str,
    capability: &'a str,
    operation_id: &'a str,
    input: Value,
    deadline_unix_ms: u64,
    depth: u32,
}

/// A typed client for the factory runtime (`factory.read/v1`) and control
/// (`factory.control/v1`) capabilities.
///
/// The client owns the scoped grants; it is deliberately not `Debug` and no
/// method copies a grant into an input or a response.
pub struct FactoryRuntimeClient {
    read: Binding,
    control: Option<Binding>,
    deadline: Duration,
    sequence: u64,
}

impl FactoryRuntimeClient {
    /// Wrap an already-connected read binding (read-only).
    pub fn new_read_only(
        client: HubCallbackClient,
        grant: impl Into<String>,
        plugin_id: impl Into<String>,
        generation: u64,
    ) -> Self {
        Self {
            read: Binding {
                client,
                grant: grant.into(),
                plugin_id: plugin_id.into(),
                generation,
            },
            control: None,
            deadline: READ_DEADLINE,
            sequence: 0,
        }
    }

    /// Wrap already-connected read and control bindings.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_control(
        read: HubCallbackClient,
        read_grant: impl Into<String>,
        read_plugin_id: impl Into<String>,
        read_generation: u64,
        control: HubCallbackClient,
        control_grant: impl Into<String>,
        control_plugin_id: impl Into<String>,
        control_generation: u64,
    ) -> Self {
        Self {
            read: Binding {
                client: read,
                grant: read_grant.into(),
                plugin_id: read_plugin_id.into(),
                generation: read_generation,
            },
            control: Some(Binding {
                client: control,
                grant: control_grant.into(),
                plugin_id: control_plugin_id.into(),
                generation: control_generation,
            }),
            deadline: READ_DEADLINE,
            sequence: 0,
        }
    }

    /// Connect a read-only client to a hub socket with the legacy single-grant
    /// input. This client may call snapshot/lookup but holds no control
    /// authority.
    pub async fn connect_read_only(
        socket: &Path,
        grant: impl Into<String>,
        plugin_id: impl Into<String>,
        generation: u64,
    ) -> Result<Self, IpcError> {
        let client = with_deadline(
            "factory.connect",
            CONNECT_TIMEOUT,
            HubCallbackClient::connect(socket),
        )
        .await??;
        Ok(Self::new_read_only(client, grant, plugin_id, generation))
    }

    /// Connect from a parsed private descriptor. `factory.read/v1` is required;
    /// `factory.control/v1` is independent and optional. A read/report/legacy
    /// grant is never substituted for a missing control binding.
    pub async fn connect_from_bindings(bindings: &CapabilityBindings) -> Result<Self, IpcError> {
        let read = bindings.read().ok_or_else(|| {
            IpcError::Contract(FailureCode::MissingCapability.failure(
                FACTORY_READ_CAPABILITY,
                "no factory read binding is configured",
            ))
        })?;
        let read_conn = connect_binding(read).await?;
        let control_conn = match bindings.control() {
            Some(control) => Some(connect_binding(control).await?),
            None => None,
        };
        Ok(Self {
            read: read_conn,
            control: control_conn,
            deadline: READ_DEADLINE,
            sequence: 0,
        })
    }

    /// Override the RPC deadline (tests use a short value). Values above the
    /// five-second design cap are clamped to it.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline.min(READ_DEADLINE);
        self
    }

    /// Whether an independent control binding is present and live.
    pub fn has_control(&self) -> bool {
        self.control.is_some()
    }

    /// Revoke the control binding. Subsequent pause/resume fail closed with a
    /// missing-capability error and [`Self::has_control`] reports false; the
    /// read binding and any already-issued browser session are untouched.
    pub fn revoke_control(&mut self) {
        self.control = None;
    }

    /// The absolute monotonic deadline for a standalone call starting now.
    fn call_deadline(&self) -> Instant {
        Instant::now() + self.deadline
    }

    /// The effective RPC deadline for an externally admitted deadline: the
    /// earlier of the admitted deadline and the client's own configured cap. It
    /// never extends past the admitted deadline, so waiting for the body or the
    /// client lock cannot grant a fresh budget.
    fn effective_deadline(&self, admitted: Instant) -> Instant {
        admitted.min(Instant::now() + self.deadline)
    }

    /// `factory.read/v1` snapshot within an externally admitted deadline.
    ///
    /// The deadline is the transport's original absolute budget; this method
    /// derives its remaining RPC time from it rather than starting a fresh one.
    pub async fn snapshot_within(
        &mut self,
        deadline: Instant,
        request: &FactoryRuntimeRequest,
    ) -> Result<FactoryRuntime, IpcError> {
        request.validate()?;
        let deadline = self.effective_deadline(deadline);
        let input = serde_json::to_value(request)
            .map_err(|e| IpcError::protocol(format!("encode snapshot request: {e}")))?;
        let value = invoke(
            &mut self.read,
            &mut self.sequence,
            deadline,
            FACTORY_READ_CAPABILITY,
            input,
        )
        .await
        .map_err(RpcFailure::into_error)?;
        let runtime: FactoryRuntime = serde_json::from_value(value)
            .map_err(|e| FailureCode::InvalidRecord.failure("FactoryRuntime", e.to_string()))?;
        runtime.validate()?;
        Ok(runtime)
    }

    /// `factory.read/v1` snapshot.
    pub async fn snapshot(
        &mut self,
        request: &FactoryRuntimeRequest,
    ) -> Result<FactoryRuntime, IpcError> {
        let deadline = self.call_deadline();
        self.snapshot_within(deadline, request).await
    }

    /// `factory.read/v1` receipt lookup within an externally admitted deadline.
    pub async fn lookup_within(
        &mut self,
        deadline: Instant,
        request: &FactoryLookupRequest,
    ) -> Result<FactoryControlResponse, IpcError> {
        request.validate()?;
        let deadline = self.effective_deadline(deadline);
        let input = serde_json::to_value(request)
            .map_err(|e| IpcError::protocol(format!("encode lookup request: {e}")))?;
        let value = invoke(
            &mut self.read,
            &mut self.sequence,
            deadline,
            FACTORY_READ_CAPABILITY,
            input,
        )
        .await
        .map_err(RpcFailure::into_error)?;
        let response = FactoryControlResponse::from_value(value)?;
        // The response must be for the exact action we looked up; a response
        // for another action id is refused rather than returned as authoritative.
        check_response_action_id(&response, &request.action_id)?;
        Ok(response)
    }

    /// `factory.read/v1` receipt lookup.
    pub async fn lookup(
        &mut self,
        request: &FactoryLookupRequest,
    ) -> Result<FactoryControlResponse, IpcError> {
        let deadline = self.call_deadline();
        self.lookup_within(deadline, request).await
    }

    /// `factory.control/v1` pause. Requires an explicit control binding.
    pub async fn pause(
        &mut self,
        request: &FactoryActionRequest,
    ) -> Result<FactoryControlResponse, IpcError> {
        let deadline = self.call_deadline();
        self.control_action_within(deadline, request, "pause").await
    }

    /// `factory.control/v1` resume. Requires an explicit control binding.
    pub async fn resume(
        &mut self,
        request: &FactoryActionRequest,
    ) -> Result<FactoryControlResponse, IpcError> {
        let deadline = self.call_deadline();
        self.control_action_within(deadline, request, "resume").await
    }

    /// `factory.control/v1` pause within an externally admitted deadline.
    pub async fn pause_within(
        &mut self,
        deadline: Instant,
        request: &FactoryActionRequest,
    ) -> Result<FactoryControlResponse, IpcError> {
        self.control_action_within(deadline, request, "pause").await
    }

    /// `factory.control/v1` resume within an externally admitted deadline.
    pub async fn resume_within(
        &mut self,
        deadline: Instant,
        request: &FactoryActionRequest,
    ) -> Result<FactoryControlResponse, IpcError> {
        self.control_action_within(deadline, request, "resume").await
    }

    async fn control_action_within(
        &mut self,
        deadline: Instant,
        request: &FactoryActionRequest,
        expected: &str,
    ) -> Result<FactoryControlResponse, IpcError> {
        if request.operation != expected {
            return Err(FailureCode::InvalidRecord
                .failure(
                    "operation",
                    format!(
                        "request operation {:?} does not match the {expected} control",
                        request.operation
                    ),
                )
                .into());
        }
        request.validate()?;
        let deadline = self.effective_deadline(deadline);
        let input = serde_json::to_value(request)
            .map_err(|e| IpcError::protocol(format!("encode control request: {e}")))?;
        let Some(control) = self.control.as_mut() else {
            return Err(IpcError::Contract(FailureCode::MissingCapability.failure(
                FACTORY_CONTROL_CAPABILITY,
                "no live explicit control binding; control is disabled",
            )));
        };
        let value = match invoke(
            control,
            &mut self.sequence,
            deadline,
            FACTORY_CONTROL_CAPABILITY,
            input,
        )
        .await
        {
            Ok(value) => value,
            // A deadline that elapsed before the request was handed to the
            // transport is a plain failure; nothing could have run.
            Err(RpcFailure::ExpiredBeforeSend) => {
                return Err(deadline_exceeded_error(FACTORY_CONTROL_CAPABILITY));
            }
            // A deadline after a possible effect retains the original action id
            // as `EffectUnconfirmed`; it never claims cancellation or retries.
            Err(RpcFailure::DeadlineAfterSend) => return unconfirmed_after_send(request),
            Err(RpcFailure::Failed(error)) => return Err(error),
        };
        let response = FactoryControlResponse::from_value(value)?;
        check_control_response(&response, request)?;
        Ok(response)
    }
}

/// How a typed RPC ended when it did not return a value.
enum RpcFailure {
    /// The admitted deadline had already elapsed, so nothing was sent.
    ExpiredBeforeSend,
    /// The request was handed to the transport but the bound elapsed; the hub
    /// may have applied the effect.
    DeadlineAfterSend,
    /// Any other transport or contract failure.
    Failed(IpcError),
}

impl RpcFailure {
    fn into_error(self) -> IpcError {
        match self {
            RpcFailure::ExpiredBeforeSend | RpcFailure::DeadlineAfterSend => {
                deadline_exceeded_error("factory.invoke")
            }
            RpcFailure::Failed(error) => error,
        }
    }
}

/// An explicit deadline failure (never a silent timeout).
fn deadline_exceeded_error(subject: &str) -> IpcError {
    FailureCode::DeadlineExceeded
        .failure(subject, "the admitted request deadline elapsed")
        .into()
}

/// Build the `EffectUnconfirmed` response for a control whose deadline elapsed
/// after the request may have reached the hub. The original action id and
/// request digest are preserved; no replacement id is generated.
fn unconfirmed_after_send(
    request: &FactoryActionRequest,
) -> Result<FactoryControlResponse, IpcError> {
    let request_digest = request.request_digest()?;
    Ok(FactoryControlResponse::Unconfirmed(
        FactoryControlUnconfirmedResponse {
            format: FACTORY_CONTROL_FORMAT.to_string(),
            result: FactoryControlResult::Unconfirmed,
            action_id: request.action_id.clone(),
            request: request.clone(),
            request_digest,
            code: FactoryUnconfirmedCode::EffectUnconfirmed,
            reason: "deadline elapsed after dispatch; outcome not confirmed".to_string(),
            observed_stop: None,
        },
    ))
}

/// The action id carried by any `FactoryControl/v1` response variant.
fn response_action_id(response: &FactoryControlResponse) -> &str {
    match response {
        FactoryControlResponse::Receipt(response) => &response.receipt.action_id,
        FactoryControlResponse::Unconfirmed(response) => &response.action_id,
        FactoryControlResponse::NotFound(response) => &response.action_id,
    }
}

/// Refuse a response whose action id is not the one we submitted.
fn check_response_action_id(
    response: &FactoryControlResponse,
    expected: &str,
) -> Result<(), IpcError> {
    if response_action_id(response) != expected {
        return Err(FailureCode::IntegrityMismatch
            .failure(
                "action_id",
                "response action id does not match the submitted action id",
            )
            .into());
    }
    Ok(())
}

/// Refuse an inappropriate control response.
///
/// A control call must return a receipt or an unconfirmed result (never
/// `not_found`), for the exact action we submitted, whose embedded request and
/// request digest are the submitted ones.
fn check_control_response(
    response: &FactoryControlResponse,
    request: &FactoryActionRequest,
) -> Result<(), IpcError> {
    let (action_id, request_digest, embedded) = match response {
        FactoryControlResponse::Receipt(response) => (
            response.receipt.action_id.as_str(),
            response.receipt.request_digest.as_str(),
            &response.receipt.request,
        ),
        FactoryControlResponse::Unconfirmed(response) => (
            response.action_id.as_str(),
            response.request_digest.as_str(),
            &response.request,
        ),
        FactoryControlResponse::NotFound(_) => {
            return Err(FailureCode::IntegrityMismatch
                .failure("result", "a control action cannot answer with `not_found`")
                .into())
        }
    };
    if action_id != request.action_id {
        return Err(FailureCode::IntegrityMismatch
            .failure(
                "action_id",
                "response action id does not match the submitted action",
            )
            .into());
    }
    if embedded != request {
        return Err(FailureCode::IntegrityMismatch
            .failure(
                "request",
                "response request does not match the submitted action",
            )
            .into());
    }
    if request_digest != request.request_digest()? {
        return Err(FailureCode::IntegrityMismatch
            .failure(
                "request_digest",
                "response request digest does not match the submitted action",
            )
            .into());
    }
    Ok(())
}

/// Connect one binding's private socket with the bounded connect timeout.
async fn connect_binding(binding: &CapabilityBinding) -> Result<Binding, IpcError> {
    let client = with_deadline(
        "factory.connect",
        CONNECT_TIMEOUT,
        HubCallbackClient::connect(Path::new(&binding.hub_socket)),
    )
    .await??;
    Ok(Binding {
        client,
        grant: binding.grant.clone(),
        plugin_id: binding.plugin_id.clone(),
        generation: binding.generation.as_u64()?,
    })
}

/// Issue one capability call on `binding`. The grant never leaves this process.
///
/// `deadline` is the absolute monotonic budget admitted by the caller. The wire
/// deadline is the remaining part of that budget (never a fresh interval), and
/// an already-elapsed deadline is refused before anything is sent.
async fn invoke(
    binding: &mut Binding,
    sequence: &mut u64,
    deadline: Instant,
    capability: &str,
    input: Value,
) -> Result<Value, RpcFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(RpcFailure::ExpiredBeforeSend);
    }
    *sequence += 1;
    let operation_id = format!(
        "{}.{}.{}.{}",
        FACTORY_CLIENT_API, binding.plugin_id, binding.generation, *sequence
    );
    let deadline_unix_ms = now_unix_ms() + remaining.as_millis() as u64;
    let call = CapabilityCall {
        grant: &binding.grant,
        plugin_id: &binding.plugin_id,
        generation: binding.generation,
        method: METHOD_INVOKE_CAPABILITY,
        capability,
        operation_id: &operation_id,
        input,
        deadline_unix_ms,
        depth: 0,
    };
    let params = serde_json::to_value(&call).map_err(|e| {
        RpcFailure::Failed(IpcError::protocol(format!("encode capability call: {e}")))
    })?;
    match with_deadline(
        &operation_id,
        remaining,
        binding.client.invoke_capability(params),
    )
    .await
    {
        Ok(result) => result.map_err(RpcFailure::Failed),
        Err(_) => Err(RpcFailure::DeadlineAfterSend),
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// --- HTTP route handlers ----------------------------------------------------

/// A shared, serialized factory client (one outstanding RPC at a time).
pub type SharedFactoryClient = Arc<tokio::sync::Mutex<FactoryRuntimeClient>>;

/// Whether the shared client currently holds an actual control binding.
///
/// Fails closed (returns `false`) when the client lock is held, so it can never
/// promote a read-only client to a control surface.
fn client_has_control(client: &SharedFactoryClient) -> bool {
    client
        .try_lock()
        .map(|guard| guard.has_control())
        .unwrap_or(false)
}

/// The factory operator-session authority: the caller's live predicate narrowed
/// by the actual factory client control binding.
///
/// `control_authorized` is the transport's POST-admission predicate and delegates
/// to the caller. `session_authorized` additionally requires a live
/// `factory.control/v1` binding, so an authorized predicate over a read-only
/// client issues no operator session or CSRF secret.
struct FactorySessionAuthority {
    inner: Arc<dyn ControlAuthority>,
    client: SharedFactoryClient,
}

impl ControlAuthority for FactorySessionAuthority {
    fn control_authorized(&self) -> bool {
        self.inner.control_authorized()
    }

    fn session_authorized(&self) -> bool {
        self.inner.control_authorized() && client_has_control(&self.client)
    }
}

/// Register the four declared factory routes on `builder`.
///
/// `GET /api/v1/factory` bootstraps the operator session; the two `/pause` and
/// `/resume` POST routes are control-only. The same live `authority` gates the
/// session bootstrap and POST admission (via the builder) and narrows the
/// browser-visible permissions, so the two can never disagree. Session
/// bootstrap and control dispatch both additionally require an actual control
/// binding on the client.
pub fn register_factory_routes(
    builder: WebAppBuilder,
    client: SharedFactoryClient,
    authority: Arc<dyn ControlAuthority>,
) -> WebAppBuilder {
    let authority: Arc<dyn ControlAuthority> = Arc::new(FactorySessionAuthority {
        inner: authority,
        client: client.clone(),
    });
    builder
        .control_authority(authority.clone())
        .session_bootstrap("/api/v1/factory")
        .get(
            "/api/v1/factory",
            factory_snapshot_handler(client.clone(), authority.clone()),
        )
        .get(
            "/api/v1/factory/actions/{action_id}",
            factory_lookup_handler(client.clone()),
        )
        .post(
            "/api/v1/factory/pause",
            factory_pause_handler(client.clone(), authority.clone()),
        )
        .post("/api/v1/factory/resume", factory_resume_handler(client, authority))
}

/// `GET /api/v1/factory` handler: bounded snapshot.
pub fn factory_snapshot_handler(
    client: SharedFactoryClient,
    authority: Arc<dyn ControlAuthority>,
) -> impl Fn(WebRequest) -> WebFuture + Send + Sync + 'static {
    move |request: WebRequest| {
        let client = client.clone();
        let authority = authority.clone();
        Box::pin(async move {
            let snapshot = match snapshot_request(&request) {
                Ok(snapshot) => snapshot,
                Err(response) => return response,
            };
            let deadline = request.deadline();
            let mut client = client.lock().await;
            match client.snapshot_within(deadline, &snapshot).await {
                Ok(mut runtime) => {
                    // Never promote: a browser-visible permission requires both a
                    // live explicit control binding and a live operator-control
                    // session. A hub-reported permission alone is not enough, and
                    // revocation clears it on the next read.
                    let live = authority.control_authorized() && client.has_control();
                    if !live {
                        runtime.permissions.pause = false;
                        runtime.permissions.resume = false;
                    }
                    json_response(http::StatusCode::OK, &runtime)
                }
                Err(error) => client_error_response(error),
            }
        })
    }
}

/// `GET /api/v1/factory/actions/{action_id}` handler: authoritative lookup.
pub fn factory_lookup_handler(
    client: SharedFactoryClient,
) -> impl Fn(WebRequest) -> WebFuture + Send + Sync + 'static {
    move |request: WebRequest| {
        let client = client.clone();
        Box::pin(async move {
            let Some(action_id) = request.param("action_id").map(str::to_string) else {
                return WebResponse::error(
                    http::StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "missing action_id",
                );
            };
            let lookup = FactoryLookupRequest {
                format: FACTORY_CONTROL_FORMAT.to_string(),
                operation: "lookup".to_string(),
                action_id,
            };
            if let Err(error) = lookup.validate() {
                return WebResponse::error(
                    http::StatusCode::BAD_REQUEST,
                    "invalid_request",
                    format!("invalid action_id: {}", failure_label(&error)),
                );
            }
            let deadline = request.deadline();
            let mut client = client.lock().await;
            match client.lookup_within(deadline, &lookup).await {
                Ok(response) => control_response_http(response),
                Err(error) => client_error_response(error),
            }
        })
    }
}

/// `POST /api/v1/factory/pause` handler.
pub fn factory_pause_handler(
    client: SharedFactoryClient,
    authority: Arc<dyn ControlAuthority>,
) -> impl Fn(WebRequest) -> WebFuture + Send + Sync + 'static {
    action_handler(client, authority, "pause", ActionKind::Pause)
}

/// `POST /api/v1/factory/resume` handler.
pub fn factory_resume_handler(
    client: SharedFactoryClient,
    authority: Arc<dyn ControlAuthority>,
) -> impl Fn(WebRequest) -> WebFuture + Send + Sync + 'static {
    action_handler(client, authority, "resume", ActionKind::Resume)
}

#[derive(Clone, Copy)]
enum ActionKind {
    Pause,
    Resume,
}

fn action_handler(
    client: SharedFactoryClient,
    authority: Arc<dyn ControlAuthority>,
    expected: &'static str,
    kind: ActionKind,
) -> impl Fn(WebRequest) -> WebFuture + Send + Sync + 'static {
    move |request: WebRequest| {
        let client = client.clone();
        let authority = authority.clone();
        Box::pin(async move {
            let body = match std::str::from_utf8(request.body()) {
                Ok(text) => text,
                Err(_) => {
                    return WebResponse::error(
                        http::StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "request body is not UTF-8",
                    )
                }
            };
            let parsed = match FactoryControlRequest::parse(body) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return WebResponse::error(
                        http::StatusCode::BAD_REQUEST,
                        "invalid_request",
                        format!("invalid FactoryControl/v1 body: {}", failure_label(&error)),
                    )
                }
            };
            let action = match parsed {
                FactoryControlRequest::Action(action) if action.operation == expected => action,
                FactoryControlRequest::Action(action) => {
                    return WebResponse::error(
                        http::StatusCode::BAD_REQUEST,
                        "invalid_request",
                        format!(
                            "body operation {:?} does not match the {expected} route",
                            action.operation
                        ),
                    )
                }
                FactoryControlRequest::Lookup(_) => {
                    return WebResponse::error(
                        http::StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "a lookup body is not a control action",
                    )
                }
            };
            let mut client = client.lock().await;
            // Revalidate immediately before dispatch, after waiting for the
            // serialized client lock: a revocation while this request was queued
            // (or while its body was acquired) must deny the effect even though
            // the earlier transport checks passed. Both the live predicate and an
            // actual control binding are required.
            if !authority.control_authorized() || !client.has_control() {
                return WebResponse::error(
                    http::StatusCode::FORBIDDEN,
                    "forbidden",
                    "operator control is not authorized",
                );
            }
            let deadline = request.deadline();
            let result = match kind {
                ActionKind::Pause => client.pause_within(deadline, &action).await,
                ActionKind::Resume => client.resume_within(deadline, &action).await,
            };
            match result {
                Ok(response) => control_response_http(response),
                Err(error) => client_error_response(error),
            }
        })
    }
}

/// Build a validated snapshot request from the query string (before any RPC).
fn snapshot_request(request: &WebRequest) -> Result<FactoryRuntimeRequest, WebResponse> {
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
    let cursor = match request.query_param("cursor") {
        None => None,
        // The query value arrives percent-encoded; decode it exactly once so the
        // opaque cursor keeps its exact identity (never zero, never twice).
        Some(raw) => match decode_query_value(raw) {
            Ok(decoded) => Some(decoded),
            Err(message) => {
                return Err(WebResponse::error(
                    http::StatusCode::BAD_REQUEST,
                    "invalid_request",
                    format!("invalid cursor encoding: {message}"),
                ))
            }
        },
    };
    let snapshot = FactoryRuntimeRequest {
        format: FACTORY_RUNTIME_FORMAT.to_string(),
        operation: "snapshot".to_string(),
        limit,
        cursor,
    };
    if let Err(error) = snapshot.validate() {
        return Err(WebResponse::error(
            http::StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid snapshot request: {}", failure_label(&error)),
        ));
    }
    Ok(snapshot)
}

/// Serialize a typed record as an `application/json` response.
fn json_response<T: Serialize>(status: http::StatusCode, value: &T) -> WebResponse {
    match serde_json::to_vec(value) {
        Ok(body) => WebResponse::bytes(status, "application/json", body),
        Err(error) => WebResponse::error(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "serialization_error",
            error.to_string(),
        ),
    }
}

/// Map a typed control response to its exact HTTP status and body.
fn control_response_http(response: FactoryControlResponse) -> WebResponse {
    let status = match &response {
        FactoryControlResponse::Receipt(_) => http::StatusCode::OK,
        FactoryControlResponse::Unconfirmed(_) => http::StatusCode::ACCEPTED,
        FactoryControlResponse::NotFound(_) => http::StatusCode::NOT_FOUND,
    };
    json_response(status, &response)
}

/// Map a typed client failure to a bounded, secret-free HTTP response.
fn client_error_response(error: IpcError) -> WebResponse {
    let (status, code, label) = classify_error(&error);
    WebResponse::error(status, code, format!("factory request failed: {label}"))
}

fn classify_error(error: &IpcError) -> (http::StatusCode, &'static str, String) {
    use http::StatusCode;
    match error {
        IpcError::Contract(contract) => {
            let code = failure_code(contract);
            let (status, http_code) = match code {
                Some(FailureCode::IdentityConflict) | Some(FailureCode::RevisionConflict) => {
                    (StatusCode::CONFLICT, "conflict")
                }
                Some(FailureCode::AuthorityDenied)
                | Some(FailureCode::MissingCapability)
                | Some(FailureCode::ScopeDenied)
                | Some(FailureCode::StaleGeneration) => (StatusCode::FORBIDDEN, "forbidden"),
                Some(FailureCode::DeadlineExceeded) => {
                    (StatusCode::GATEWAY_TIMEOUT, "deadline_exceeded")
                }
                Some(FailureCode::StateUnavailable) | Some(FailureCode::BudgetUnavailable) => {
                    (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
                }
                Some(FailureCode::EffectUnconfirmed)
                | Some(FailureCode::UsageUnconfirmed)
                | Some(FailureCode::TerminationUnconfirmed) => {
                    (StatusCode::ACCEPTED, "unconfirmed")
                }
                Some(
                    FailureCode::InvalidRecord
                    | FailureCode::IntegrityMismatch
                    | FailureCode::UnsupportedSchema
                    | FailureCode::UnsupportedVersion,
                ) => (StatusCode::BAD_REQUEST, "invalid_request"),
                Some(other) => (StatusCode::SERVICE_UNAVAILABLE, other_label(other)),
                None => match contract {
                    ContractError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
                    ContractError::UnsupportedCapability(_) => {
                        (StatusCode::FORBIDDEN, "forbidden")
                    }
                    ContractError::UnknownOutcome(_) => {
                        (StatusCode::ACCEPTED, "unconfirmed")
                    }
                    ContractError::KnownFailure(_) => {
                        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
                    }
                },
            };
            (status, http_code, failure_label(contract))
        }
        IpcError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "transport unavailable".to_string(),
        ),
        IpcError::Protocol(_) => (
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "protocol violation".to_string(),
        ),
    }
}

fn other_label(code: FailureCode) -> &'static str {
    match code {
        FailureCode::IdentityConflict | FailureCode::RevisionConflict => "conflict",
        FailureCode::AuthorityDenied
        | FailureCode::MissingCapability
        | FailureCode::ScopeDenied
        | FailureCode::StaleGeneration => "forbidden",
        FailureCode::DeadlineExceeded => "deadline_exceeded",
        FailureCode::InvalidRecord
        | FailureCode::IntegrityMismatch
        | FailureCode::UnsupportedSchema
        | FailureCode::UnsupportedVersion => "invalid_request",
        _ => "unavailable",
    }
}

/// The bounded failure label: the [`FailureCode`] string when present, otherwise
/// a generic label. It never echoes a request body, token, or grant.
fn failure_label(error: &ContractError) -> String {
    match failure_code(error) {
        Some(code) => code.as_str().to_string(),
        None => match error {
            ContractError::Conflict(_) => "Conflict".to_string(),
            ContractError::UnsupportedCapability(_) => "UnsupportedCapability".to_string(),
            ContractError::UnknownOutcome(_) => "UnknownOutcome".to_string(),
            ContractError::KnownFailure(_) => "KnownFailure".to_string(),
        },
    }
}

/// The transport may re-export this helper for the CSRF response header name.
pub const FACTORY_CSRF_HEADER: &str = CSRF_HEADER;

#[cfg(test)]
mod tests {
    use super::*;

    /// A canonical 32-byte (64 hex-character) launch-grant token fixture.
    const TEST_GRANT: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn binding_json(capability: &str) -> String {
        format!(
            r#"{{"capability":"{capability}","hub_socket":"/run/hub.sock","grant":"{TEST_GRANT}","plugin_id":"agalma-web-dashboard","generation":"1"}}"#
        )
    }

    fn descriptor(entries: &[String]) -> String {
        format!(
            r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{}]}}"#,
            entries.join(",")
        )
    }

    #[test]
    fn parses_independent_read_and_control_bindings() {
        let raw = descriptor(&[
            binding_json(FACTORY_READ_CAPABILITY),
            binding_json(FACTORY_CONTROL_CAPABILITY),
        ]);
        let bindings = parse_capability_bindings(&raw).expect("descriptor parses");
        assert_eq!(bindings.len(), 2);
        assert_eq!(
            bindings.read().unwrap().generation.as_str(),
            "1",
            "exact generation"
        );
        assert!(bindings.control().is_some());
    }

    #[test]
    fn rejects_duplicate_and_malformed_bindings() {
        let duplicate = descriptor(&[
            binding_json(FACTORY_READ_CAPABILITY),
            binding_json(FACTORY_READ_CAPABILITY),
        ]);
        assert!(parse_capability_bindings(&duplicate).is_err());

        // Unknown fields and wrong family/version fail closed.
        let unknown = r#"{"format":"PluginCapabilityBindings/v1","bindings":[],"extra":true}"#;
        assert!(parse_capability_bindings(unknown).is_err());
        let wrong_family = r#"{"format":"Other/v1","bindings":[]}"#;
        assert!(parse_capability_bindings(wrong_family).is_err());
        let wrong_version = r#"{"format":"PluginCapabilityBindings/v2","bindings":[]}"#;
        assert!(parse_capability_bindings(wrong_version).is_err());

        // A generation must be an exact unsigned integer.
        let bad_generation = format!(
            r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{{"capability":"factory.read/v1","hub_socket":"/s","grant":"{TEST_GRANT}","plugin_id":"p","generation":"x"}}]}}"#
        );
        assert!(parse_capability_bindings(&bad_generation).is_err());
    }

    #[test]
    fn caps_entry_count_and_descriptor_size() {
        let mut entries = Vec::new();
        for index in 0..=PLUGIN_CAPABILITY_BINDINGS_MAX_ENTRIES {
            entries.push(format!(
                r#"{{"capability":"cap/v{index}","hub_socket":"/s","grant":"{TEST_GRANT}","plugin_id":"p","generation":"1"}}"#
            ));
        }
        let too_many = descriptor(&entries);
        assert!(parse_capability_bindings(&too_many).is_err());

        let oversized = format!(
            r#"{{"format":"PluginCapabilityBindings/v1","bindings":[],"pad":"{}"}}"#,
            "x".repeat(PLUGIN_CAPABILITY_BINDINGS_MAX_BYTES)
        );
        assert!(parse_capability_bindings(&oversized).is_err());
    }

    #[test]
    fn debug_never_prints_the_grant() {
        let bindings = parse_capability_bindings(&descriptor(&[binding_json(
            FACTORY_READ_CAPABILITY,
        )]))
        .unwrap();
        let debug = format!("{:?}", bindings);
        assert!(debug.contains("<redacted>"));
        assert!(
            !debug.contains(TEST_GRANT),
            "grant leaked into Debug: {debug}"
        );
    }

    #[test]
    fn rejects_noncanonical_format_aliases() {
        for format in [
            "PluginCapabilityBindings/v01",
            "PluginCapabilityBindings/V1",
            "PluginCapabilityBindings/v1x",
            "PluginCapabilityBindings/v1 ",
            "PluginCapabilityBindings/v1/1",
        ] {
            let raw = format!(r#"{{"format":"{format}","bindings":[]}}"#);
            assert!(
                parse_capability_bindings(&raw).is_err(),
                "noncanonical format {format:?} was accepted"
            );
        }
    }

    #[test]
    fn rejects_noncanonical_grants() {
        // A canonical 32-byte hex token is accepted.
        assert!(parse_capability_bindings(&descriptor(&[binding_json(
            FACTORY_READ_CAPABILITY
        )]))
        .is_ok());

        // Anything that is not exactly 64 hex characters is refused, whether it
        // is the wrong length or the wrong alphabet.
        let mut malformed: Vec<String> = vec![
            String::new(),
            "t".to_string(),
            "a".repeat(BINDING_GRANT_HEX_CHARS - 1),
            "a".repeat(BINDING_GRANT_HEX_CHARS + 1),
            "a".repeat(4096),
            "z".repeat(BINDING_GRANT_HEX_CHARS),
            format!("{}g", "a".repeat(BINDING_GRANT_HEX_CHARS - 1)),
            format!("{}\t", "a".repeat(BINDING_GRANT_HEX_CHARS - 1)),
        ];
        for grant in malformed.drain(..) {
            let raw = format!(
                r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{{"capability":"factory.read/v1","hub_socket":"/run/hub.sock","grant":"{grant}","plugin_id":"p","generation":"1"}}]}}"#
            );
            assert!(
                parse_capability_bindings(&raw).is_err(),
                "noncanonical grant {grant:?} was accepted"
            );
        }
    }

    #[test]
    fn rejects_unsafe_socket_and_plugin_id() {
        // A relative socket path is not a valid launch socket.
        let relative = format!(
            r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{{"capability":"factory.read/v1","hub_socket":"relative.sock","grant":"{TEST_GRANT}","plugin_id":"p","generation":"1"}}]}}"#
        );
        assert!(parse_capability_bindings(&relative).is_err());

        // A NUL in the socket path is refused.
        let nul_socket = format!(
            r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{{"capability":"factory.read/v1","hub_socket":"/run/hub\u0000.sock","grant":"{TEST_GRANT}","plugin_id":"p","generation":"1"}}]}}"#
        );
        assert!(parse_capability_bindings(&nul_socket).is_err());

        // A plugin id that is a private path is refused.
        let private_plugin = format!(
            r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{{"capability":"factory.read/v1","hub_socket":"/run/hub.sock","grant":"{TEST_GRANT}","plugin_id":"/etc/passwd","generation":"1"}}]}}"#
        );
        assert!(parse_capability_bindings(&private_plugin).is_err());

        // An overlong socket path is refused.
        let long_socket = format!(
            r#"{{"format":"PluginCapabilityBindings/v1","bindings":[{{"capability":"factory.read/v1","hub_socket":"/{}","grant":"{TEST_GRANT}","plugin_id":"p","generation":"1"}}]}}"#,
            "a".repeat(BINDING_SOCKET_MAX_BYTES)
        );
        assert!(parse_capability_bindings(&long_socket).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn env_descriptor_handles_missing_malformed_and_non_unicode() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let name = PLUGIN_CAPABILITY_BINDINGS_ENV;
        std::env::remove_var(name);
        assert!(
            capability_bindings_from_env().unwrap().is_none(),
            "a missing descriptor is None"
        );

        std::env::set_var(name, descriptor(&[binding_json(FACTORY_READ_CAPABILITY)]));
        assert!(capability_bindings_from_env().unwrap().is_some());

        std::env::set_var(name, "not json");
        assert!(capability_bindings_from_env().is_err());

        // A non-Unicode environment value is an error, never a silent absence.
        std::env::set_var(name, OsString::from_vec(vec![b'{', 0x80, b'}']));
        assert!(
            capability_bindings_from_env().is_err(),
            "a non-Unicode env value must be refused, not treated as absent"
        );

        std::env::remove_var(name);
    }
}
