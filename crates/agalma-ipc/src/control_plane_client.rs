//! Typed hub read client for the `ControlPlaneReadApi/v1` read surface (WEB.3,
//! `agalma-rwn.3`).
//!
//! The browser dashboard adapter reads hub-owned projections through HP.4's
//! authenticated capability routing: it holds a scoped `control-plane.read/v1`
//! grant and calls `hub.InvokeCapability`. This module is the typed client half
//! of that surface. It:
//!
//! - reuses the existing locked IPC stack ([`HubCallbackClient`], See
//!   [`crate::connect`]) and never adds an HTTP client dependency;
//! - carries the scoped grant (lowercase hex) inside the server process only:
//!   the grant is never returned, printed, or placed in a browser response;
//! - sends the actual WEB.2 `ControlPlaneReadRequest` JSON: a snake_case
//!   `operation` tag with the operation's arguments flattened beside it (no
//!   invented nested `input` object, no uppercase operation names);
//! - enforces bounded connection and RPC deadlines (five seconds by default)
//!   and reports an explicit `DeadlineExceeded` failure;
//! - validates bounded requests client-side (page limit range, page cursor
//!   shape, artifact digest/offset/chunk bounds) and refuses a malformed value
//!   with the contract's exact `InvalidRecord` failure before any RPC;
//! - decodes responses through the canonical WEB.1 [`BrowserWire`] projection
//!   (nested identity `u64`s arrive as exact decimal strings), validates the
//!   response and report-envelope versions, and enforces the overview and page
//!   serialized byte bounds;
//! - invokes no effect: every call is an observation. Viewing never grants
//!   action authority.
//!
//! The wire shape of the capability call mirrors the hub's
//! `CapabilityCallRequest` (`grant`, `plugin_id`, `generation`, `method`,
//! `capability`, `operation_id`, `input`, `deadline_unix_ms`, `depth`). That
//! type lives in `agalma-conductor`, which `agalma-ipc` must not depend on, so
//! this crate carries an equal client-side record. `input` is the canonical
//! WEB.2 read request object.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agalma_contracts::control_plane::{
    ListComponents, ReadArtifactChunk, ReadArtifactChunkRequest, ReadComponent, ReadCursor,
    ReadMetadata, ReadOverview, ReadPageRequest, CONTROL_PLANE_READ_CAPABILITY,
    CONTROL_PLANE_READ_FORMAT, READ_PAGE_MAX_BYTES,
};
use agalma_contracts::{BrowserWire, ContractError, ExactU64, FailureCode};
use serde::Serialize;
use serde_json::Value;

use crate::error::IpcError;
use crate::plugin_client::HubCallbackClient;
use crate::plugin_server::METHOD_INVOKE_CAPABILITY;

/// The hub-owned read API family hosted behind the capability.
pub const CONTROL_PLANE_READ_API: &str = "ControlPlaneReadApi/v1";
/// The five-second read deadline every RPC carries (design).
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// The default bound on the connection handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The canonical WEB.2 `ControlPlaneReadRequest` JSON.
///
/// `#[serde(tag = "operation", rename_all = "snake_case")]` with flattened
/// variant fields is exactly the conductor's request contract: for example
/// `{"operation":"list_components","limit":50,"cursor":null}`. It is deliberately
/// a local record because `agalma-ipc` must not depend on `agalma-conductor`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ReadRequest {
    ReadOverview,
    ListComponents {
        limit: Option<u32>,
        cursor: Option<String>,
    },
    ReadComponent {
        plugin_id: String,
        limit: Option<u32>,
        cursor: Option<String>,
    },
    ReadArtifactChunk {
        plugin_id: String,
        digest: String,
        offset: Option<ExactU64>,
        max_bytes: Option<u64>,
    },
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

/// A typed client for the `control-plane.read/v1` hub capability.
///
/// The client owns the scoped grant; it is deliberately neither returned nor
/// `Debug`-printed, and no method copies it into an input or a response.
pub struct ControlPlaneReadClient {
    client: HubCallbackClient,
    grant: String,
    plugin_id: String,
    generation: u64,
    deadline: Duration,
    sequence: u64,
}

impl ControlPlaneReadClient {
    /// Wrap an already-connected [`HubCallbackClient`] with a scoped grant.
    pub fn new(
        client: HubCallbackClient,
        grant: impl Into<String>,
        plugin_id: impl Into<String>,
        generation: u64,
    ) -> Self {
        Self {
            client,
            grant: grant.into(),
            plugin_id: plugin_id.into(),
            generation,
            deadline: READ_DEADLINE,
            sequence: 0,
        }
    }

    /// Connect to the hub's common RPC socket with the default bounded
    /// connection timeout.
    pub async fn connect(
        socket: &Path,
        grant: impl Into<String>,
        plugin_id: impl Into<String>,
        generation: u64,
    ) -> Result<Self, IpcError> {
        Self::connect_with_timeout(socket, grant, plugin_id, generation, CONNECT_TIMEOUT).await
    }

    /// Connect with an explicit connection-handshake timeout.
    ///
    /// A stalled peer is refused with an explicit `DeadlineExceeded` failure
    /// instead of hanging forever.
    pub async fn connect_with_timeout(
        socket: &Path,
        grant: impl Into<String>,
        plugin_id: impl Into<String>,
        generation: u64,
        timeout: Duration,
    ) -> Result<Self, IpcError> {
        let client = with_deadline(
            "control-plane.connect",
            timeout,
            HubCallbackClient::connect(socket),
        )
        .await??;
        Ok(Self::new(client, grant, plugin_id, generation))
    }

    /// Override the read deadline (tests use a short value). A value above the
    /// five-second design cap is clamped to it, so the client never permits a
    /// longer read than the contract allows.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline.min(READ_DEADLINE);
        self
    }

    /// The operation id used for the next call (monotonic, process-unique).
    fn next_operation_id(&mut self) -> String {
        self.sequence += 1;
        format!(
            "{}.{}.{}.{}",
            CONTROL_PLANE_READ_API, self.plugin_id, self.generation, self.sequence
        )
    }

    /// The absolute deadline (Unix milliseconds) for a call starting now.
    fn deadline_unix_ms(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        now.as_millis() as u64 + self.deadline.as_millis() as u64
    }

    /// Issue one capability call and return its raw JSON result.
    ///
    /// `request` is the canonical WEB.2 read request. This is the single place
    /// the grant is used; it never leaves this process.
    async fn call(&mut self, request: &ReadRequest) -> Result<Value, IpcError> {
        let operation_id = self.next_operation_id();
        let deadline_unix_ms = self.deadline_unix_ms();
        let input = serde_json::to_value(request)
            .map_err(|e| IpcError::protocol(format!("encode read request: {e}")))?;
        let call = CapabilityCall {
            grant: &self.grant,
            plugin_id: &self.plugin_id,
            generation: self.generation,
            method: METHOD_INVOKE_CAPABILITY,
            capability: CONTROL_PLANE_READ_CAPABILITY,
            operation_id: &operation_id,
            input,
            deadline_unix_ms,
            depth: 0,
        };
        let params = serde_json::to_value(&call)
            .map_err(|e| IpcError::protocol(format!("encode capability call: {e}")))?;
        let result = with_deadline(
            &operation_id,
            self.deadline,
            self.client.invoke_capability(params),
        )
        .await?;
        result
    }

    /// `ReadOverview`: factory stop observation, executions, economics and a
    /// curated timeline.
    pub async fn read_overview(&mut self) -> Result<ReadOverview, IpcError> {
        let value = self.call(&ReadRequest::ReadOverview).await?;
        let overview: ReadOverview = decode(value)?;
        check_read_format(&overview.metadata)?;
        check_overview_bound(&overview)?;
        Ok(overview)
    }

    /// `ListComponents`: a bounded page of registry-derived components.
    pub async fn list_components(
        &mut self,
        request: &ReadPageRequest,
    ) -> Result<ListComponents, IpcError> {
        check_page_request(request)?;
        let value = self
            .call(&ReadRequest::ListComponents {
                limit: request.limit,
                cursor: request.cursor.clone(),
            })
            .await?;
        let page: ListComponents = decode(value)?;
        check_read_format(&page.metadata)?;
        page.check_bounds()?;
        Ok(page)
    }

    /// `ReadComponent`: one component's descriptors and bounded namespace
    /// reports from the authoritative projection.
    pub async fn read_component(
        &mut self,
        plugin_id: &str,
        request: &ReadPageRequest,
    ) -> Result<ReadComponent, IpcError> {
        check_component_id(plugin_id)?;
        check_page_request(request)?;
        let value = self
            .call(&ReadRequest::ReadComponent {
                plugin_id: plugin_id.to_string(),
                limit: request.limit,
                cursor: request.cursor.clone(),
            })
            .await?;
        let component: ReadComponent = decode(value)?;
        check_read_format(&component.metadata)?;
        check_component_envelope_versions(&component)?;
        component.check_bounds()?;
        Ok(component)
    }

    /// `ReadArtifactChunk`: at most 64 KiB from an authorized immutable
    /// reference; the total artifact remains bounded by 16 MiB.
    pub async fn read_artifact_chunk(
        &mut self,
        request: &ReadArtifactChunkRequest,
    ) -> Result<ReadArtifactChunk, IpcError> {
        request.check()?;
        let value = self
            .call(&ReadRequest::ReadArtifactChunk {
                plugin_id: request.plugin_id.clone(),
                digest: request.digest.clone(),
                offset: request.offset.clone(),
                max_bytes: request.max_bytes,
            })
            .await?;
        let chunk: ReadArtifactChunk = decode(value)?;
        check_read_format(&chunk.metadata)?;
        chunk.check_bounds()?;
        Ok(chunk)
    }
}

/// Run `future` under `timeout`, mapping an elapsed bound onto an explicit
/// `DeadlineExceeded` failure (never a silent timeout).
///
/// The connection handshake and every RPC use this same bound, and the
/// dashboard adapter can reuse it for its own bounded work.
pub async fn with_deadline<T, F>(subject: &str, timeout: Duration, future: F) -> Result<T, IpcError>
where
    F: std::future::Future<Output = T>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(value) => Ok(value),
        Err(_) => Err(deadline_error(subject, timeout)),
    }
}

/// An explicit deadline failure (never a silent timeout).
fn deadline_error(subject: &str, timeout: Duration) -> IpcError {
    FailureCode::DeadlineExceeded
        .failure(
            subject,
            format!("deadline of {timeout:?} elapsed before the operation completed"),
        )
        .into()
}

/// Decode a response through the canonical WEB.1 browser-wire codec.
fn decode<T: BrowserWire>(value: Value) -> Result<T, IpcError> {
    T::from_wire_value(&value).map_err(IpcError::from)
}

/// Validate the response format and version.
fn check_read_format(metadata: &ReadMetadata) -> Result<(), IpcError> {
    if metadata.format == CONTROL_PLANE_READ_FORMAT {
        return Ok(());
    }
    let error = if metadata.format.starts_with("ControlPlaneRead/") {
        FailureCode::UnsupportedVersion.failure(
            CONTROL_PLANE_READ_FORMAT,
            format!(
                "unsupported ControlPlaneRead response version {:?} (expected {CONTROL_PLANE_READ_FORMAT})",
                metadata.format
            ),
        )
    } else {
        FailureCode::UnsupportedSchema.failure(
            CONTROL_PLANE_READ_FORMAT,
            format!("unrecognized read response format {:?}", metadata.format),
        )
    };
    Err(error.into())
}

/// Gate every hub-observed report envelope included in a component page.
fn check_component_envelope_versions(component: &ReadComponent) -> Result<(), IpcError> {
    for envelope in &component.entries {
        envelope.version_gate().map_err(IpcError::from)?;
    }
    Ok(())
}

/// Enforce the overview serialized byte bound on the browser-wire projection.
fn check_overview_bound(overview: &ReadOverview) -> Result<(), IpcError> {
    let bytes = overview.to_wire_json()?.len() as u64;
    if bytes > READ_PAGE_MAX_BYTES {
        return Err(FailureCode::InvalidRecord
            .failure(
                "overview",
                format!(
                    "overview serializes to {bytes} bytes, exceeding limit {READ_PAGE_MAX_BYTES}"
                ),
            )
            .into());
    }
    Ok(())
}

/// Validate a page request before any RPC: the limit range and, when present,
/// the cursor shape.
fn check_page_request(request: &ReadPageRequest) -> Result<(), IpcError> {
    request.effective_limit()?;
    if let Some(cursor) = &request.cursor {
        ReadCursor::decode(cursor)?;
    }
    Ok(())
}

/// Validate a caller-supplied component id before any RPC: non-empty and free
/// of control characters and path separators (including NUL).
///
/// This is the one component-id rule shared by the typed client and the
/// dashboard's `GET /api/v1/components/{plugin_id}` route, so a browser path
/// segment and an RPC caller are refused for exactly the same ids instead of
/// keeping two copies of the rule.
pub fn check_component_id(plugin_id: &str) -> Result<(), IpcError> {
    if plugin_id.is_empty()
        || plugin_id
            .bytes()
            .any(|b| b.is_ascii_control() || b == b'/' || b == b'\\')
    {
        return Err(
            ContractError::KnownFailure(format!("invalid component id {plugin_id:?}")).into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_component_id;
    use crate::error::IpcError;
    use agalma_contracts::ContractError;

    fn assert_refused(plugin_id: &str) {
        let error = check_component_id(plugin_id)
            .expect_err("invalid component id must be refused before any RPC");
        match error {
            IpcError::Contract(ContractError::KnownFailure(message)) => {
                assert!(
                    message.contains("invalid component id"),
                    "unexpected refusal message {message:?}"
                );
            }
            other => panic!("expected a KnownFailure contract error, got {other:?}"),
        }
    }

    #[test]
    fn control_plane_client_component_id_rule_is_shared_and_bounded() {
        // Accepted canonical ids (the dashboard route reuses this exact rule).
        for valid in [
            "agalma-web-dashboard",
            "example.unfamiliar-reporter",
            "a",
            "plugin_1",
            "Ünicode",
        ] {
            assert!(
                check_component_id(valid).is_ok(),
                "valid id {valid:?} was refused"
            );
        }

        // Empty, path separators, NUL and every ASCII control character are
        // refused before any RPC.
        for invalid in ["", "a/b", "/", "a\\b", "\\", "\0", "a\0b"] {
            assert_refused(invalid);
        }
        for byte in (0u8..=0x1f).chain(std::iter::once(0x7f)) {
            let plugin_id = format!("prefix{}suffix", char::from(byte));
            assert_refused(&plugin_id);
        }
    }
}
