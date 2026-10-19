//! Shared bounded report publisher/outbox primitives (CW.2).
//!
//! This module implements the producer half of the frozen CW.0 contract in
//! `docs/web-dashboard-design.md` (§ CW.0 freeze): one immutable publication
//! outstanding per binding, a caller-owned durable store, exact replay after a
//! lost acknowledgement, and generation fencing. It defines seams only. It
//! owns no persistence backend, no owner sampling loop, no hub-side recovery
//! driver, and no external I/O inside domain request handlers.
//!
//! Ordering invariants enforced here:
//!
//! 1. `seal` allocates `checkpoint.sequence + 1` only when no pending record
//!    exists and the caller-supplied coalescing window has elapsed.
//! 2. `begin` is durable before any send. A crash before send replays the exact
//!    persisted bytes.
//! 3. A returned envelope must match the pending publication's complete
//!    identity and content (plugin, every [`BindingPin`] field, namespace,
//!    version, sequence, timestamps, execution reference, durability, payload
//!    and digest) before it is acknowledged.
//! 4. `acknowledge` commits the checkpoint and clears pending atomically; only
//!    then is the next sequence allowed. A lost response, deadline, transport
//!    failure, store failure or invalid acknowledgement leaves the immutable
//!    pending record in place.
//! 5. A retired or mismatched generation is fenced. Pending bytes stay readable
//!    for replay but no new sequence is allocated.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use agalma_contracts::digest::{canonical_digest, canonical_json, validate_digest};
use agalma_contracts::{
    BindingPin, ContractError, Durability, FailureCode, ReportEnvelope, ReportPublication,
    ReportSchemaRef, REPORT_PAYLOAD_LIMIT_BYTES, REPORT_PUBLICATION_FORMAT,
};
use serde_json::{json, Value};

use crate::error::IpcError;
use crate::plugin_client::HubCallbackClient;

/// Default payload bound (64 KiB canonical JSON).
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = REPORT_PAYLOAD_LIMIT_BYTES as usize;
/// Default bound for one send attempt.
pub const DEFAULT_PUBLISH_DEADLINE_MS: u64 = 5_000;
/// Default owner sample coalescing window.
pub const DEFAULT_COALESCE_AFTER_MS: u64 = 5_000;
/// Default retry backoff ceiling.
pub const DEFAULT_MAX_RETRY_BACKOFF_MS: u64 = 5_000;
/// First retry delay after a failed attempt.
pub const INITIAL_RETRY_BACKOFF_MS: u64 = 250;

/// The complete publication identity a publisher is pinned to.
///
/// `declared_schemas` is the immutable manifest subset the caller loaded from
/// the installed generation. It is the only namespace authority available to
/// the publisher; an undeclared namespace/version is refused locally rather
/// than looked up or inferred.
#[derive(Clone, Debug, PartialEq)]
pub struct PublisherBinding {
    pub plugin_id: String,
    pub binding: BindingPin,
    pub declared_schemas: Vec<ReportSchemaRef>,
}

/// Bounded publisher behavior. Defaults match the frozen CW.0 values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherLimits {
    /// Canonical payload byte bound (never above [`REPORT_PAYLOAD_LIMIT_BYTES`]).
    pub max_payload_bytes: usize,
    /// Wall-clock bound for one send attempt.
    pub publish_deadline_ms: u64,
    /// Minimum interval between a completed publication and the next seal.
    pub coalesce_after_ms: u64,
    /// Retry backoff ceiling; the delay starts at [`INITIAL_RETRY_BACKOFF_MS`].
    pub max_retry_backoff_ms: u64,
}

impl Default for PublisherLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            publish_deadline_ms: DEFAULT_PUBLISH_DEADLINE_MS,
            coalesce_after_ms: DEFAULT_COALESCE_AFTER_MS,
            max_retry_backoff_ms: DEFAULT_MAX_RETRY_BACKOFF_MS,
        }
    }
}

/// Last acknowledged publication boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AckCheckpoint {
    pub generation: u64,
    pub sequence: u64,
    pub payload_digest: String,
    pub acknowledged_at_unix_ms: u64,
}

/// Restart state returned by a caller-owned [`PublicationStore`].
#[derive(Clone, Debug, PartialEq)]
pub enum LoadedPublication {
    /// No pending record; the next sequence is `checkpoint.sequence + 1`.
    Empty { checkpoint: Option<AckCheckpoint> },
    /// Exact sealed record to replay before any new sequence is allocated.
    ///
    /// `checkpoint` is the last acknowledged boundary persisted alongside the
    /// pending record. It is `None` only for the first publication (sequence 1).
    Pending {
        publication: ReportPublication,
        checkpoint: Option<AckCheckpoint>,
    },
    /// Store belongs to another generation; never read or write its sequence.
    Fenced { generation: u64 },
}

/// Caller-owned durable persistence seam. CW.2 defines it; the owner
/// implements it with its own single-writer durable storage.
pub trait PublicationStore {
    /// Load the one pending record (if any) and the last checkpoint.
    fn load(&mut self) -> Result<LoadedPublication, ContractError>;

    /// Durably write the one pending record before any send. Must fail if a
    /// pending record already exists; must not reserve or skip a sequence.
    fn begin(&mut self, publication: &ReportPublication) -> Result<(), ContractError>;

    /// Atomically persist the new checkpoint and clear the pending record.
    /// Returns the durable checkpoint; no later seal is allowed until it does.
    fn acknowledge(&mut self, envelope: &ReportEnvelope) -> Result<AckCheckpoint, ContractError>;
}

/// Async JSON transport used to send `hub.PublishReport`.
///
/// [`HubCallbackClient`] implements this over the existing Connect transport.
/// A fake implementation is used by deterministic tests.
pub trait ReportTransport {
    /// Send one `hub.PublishReport` request and return the raw response value.
    fn publish_report<'a>(
        &'a mut self,
        request: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, IpcError>> + Send + 'a>>;
}

impl ReportTransport for HubCallbackClient {
    fn publish_report<'a>(
        &'a mut self,
        request: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, IpcError>> + Send + 'a>> {
        Box::pin(HubCallbackClient::publish_report(self, request))
    }
}

/// Outcome of one `publish_pending` attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum PublishOutcome {
    /// The hub returned an envelope matching the pending identity; it was
    /// acknowledged and the pending record cleared.
    Acknowledged(ReportEnvelope),
    /// The identical record is still pending (send failure, lost
    /// acknowledgement, deadline, store failure, or retry/coalescing wait).
    Pending,
    /// Grant or generation no longer matches this publisher. Pending bytes are
    /// retained; no new sequence is allocated.
    Fenced,
}

/// Bounded report publisher over a caller-owned durable store.
pub struct ReportPublisher<S: PublicationStore> {
    store: S,
    binding: PublisherBinding,
    limits: PublisherLimits,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    checkpoint: Option<AckCheckpoint>,
    pending: Option<ReportPublication>,
    fenced: bool,
    coalesce_base_ms: Option<u64>,
    retry_at_ms: Option<u64>,
    next_retry_backoff_ms: u64,
}

impl<S: PublicationStore> std::fmt::Debug for ReportPublisher<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReportPublisher")
            .field("plugin_id", &self.binding.plugin_id)
            .field("generation", &self.binding.binding.generation)
            .field("checkpoint", &self.checkpoint)
            .field("pending", &self.pending)
            .field("fenced", &self.fenced)
            .finish_non_exhaustive()
    }
}

impl<S: PublicationStore> ReportPublisher<S> {
    /// Open a publisher against caller-owned durable state.
    ///
    /// A store bound to another generation is refused with `StaleGeneration`
    /// and is never read for a sequence or rewritten. A pending record is
    /// validated in full and retained for replay; no new sequence is available
    /// until it is acknowledged.
    pub fn open(
        store: S,
        binding: PublisherBinding,
        limits: PublisherLimits,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, ContractError> {
        validate_binding(&binding)?;
        validate_limits(&limits)?;

        let mut publisher = Self {
            store,
            binding,
            limits,
            clock,
            checkpoint: None,
            pending: None,
            fenced: false,
            coalesce_base_ms: None,
            retry_at_ms: None,
            next_retry_backoff_ms: 0,
        };
        publisher.next_retry_backoff_ms = publisher.initial_backoff();

        let loaded = publisher.store.load()?;
        match loaded {
            LoadedPublication::Empty { checkpoint } => {
                if let Some(checkpoint) = checkpoint {
                    publisher.validate_checkpoint(&checkpoint)?;
                    publisher.coalesce_base_ms = Some(checkpoint.acknowledged_at_unix_ms);
                    publisher.checkpoint = Some(checkpoint);
                }
            }
            LoadedPublication::Pending {
                publication,
                checkpoint,
            } => {
                publisher.validate_publication(&publication)?;
                if let Some(checkpoint) = checkpoint.as_ref() {
                    publisher.validate_checkpoint(checkpoint)?;
                    let expected = checkpoint.sequence.checked_add(1).ok_or_else(|| {
                        corrupted(&publisher.binding, "checkpoint sequence overflow")
                    })?;
                    if expected != publication.sequence {
                        return Err(corrupted(
                            &publisher.binding,
                            format!(
                                "pending sequence {} does not follow checkpoint sequence {}",
                                publication.sequence, checkpoint.sequence
                            ),
                        ));
                    }
                } else if publication.sequence != 1 {
                    return Err(corrupted(
                        &publisher.binding,
                        format!(
                            "pending sequence {} has no acknowledged predecessor",
                            publication.sequence
                        ),
                    ));
                }
                publisher.checkpoint = checkpoint;
                publisher.pending = Some(publication);
            }
            LoadedPublication::Fenced { generation } => {
                publisher.fenced = true;
                return Err(FailureCode::StaleGeneration.failure(
                    &publisher.binding.plugin_id,
                    format!(
                        "publication store generation {generation} is fenced; publisher pins {}",
                        publisher.binding.binding.generation
                    ),
                ));
            }
        }
        Ok(publisher)
    }

    /// Seal and durably persist one publication.
    ///
    /// Refuses while a record is pending, after fencing, before the coalescing
    /// window elapses, when the namespace/version is undeclared, or when the
    /// payload exceeds the local bound. A refused seal consumes no sequence.
    pub fn seal(
        &mut self,
        schema_namespace: &str,
        schema_version: u64,
        payload: Value,
        reported_at_unix_ms: u64,
    ) -> Result<ReportPublication, ContractError> {
        self.seal_with_execution(
            schema_namespace,
            schema_version,
            payload,
            reported_at_unix_ms,
            None,
        )
    }

    /// `seal` with an explicit caller-supplied execution reference.
    ///
    /// Without one, `execution_id` is `None`; durability is always `Durable`.
    pub fn seal_with_execution(
        &mut self,
        schema_namespace: &str,
        schema_version: u64,
        payload: Value,
        reported_at_unix_ms: u64,
        execution_id: Option<String>,
    ) -> Result<ReportPublication, ContractError> {
        if self.fenced {
            return Err(FailureCode::StaleGeneration
                .failure(&self.binding.plugin_id, "publisher generation is fenced"));
        }
        if let Some(pending) = self.pending.as_ref() {
            return Err(FailureCode::StateUnavailable.failure(
                &self.binding.plugin_id,
                format!(
                    "publication sequence {} is still pending; acknowledge it before sealing",
                    pending.sequence
                ),
            ));
        }
        if !self.schema_declared(schema_namespace, schema_version) {
            return Err(FailureCode::UnsupportedSchema.failure(
                &self.binding.plugin_id,
                format!("schema {schema_namespace} v{schema_version} is not declared"),
            ));
        }
        let now = (self.clock)();
        if let Some(base) = self.coalesce_base_ms {
            let ready = base.saturating_add(self.limits.coalesce_after_ms);
            if now < ready {
                return Err(FailureCode::StateUnavailable.failure(
                    &self.binding.plugin_id,
                    format!("coalescing until {ready} ms (now {now} ms)"),
                ));
            }
        }
        let bytes = canonical_json(&payload)?.len();
        if bytes > self.limits.max_payload_bytes {
            return Err(FailureCode::InvalidRecord.failure(
                &self.binding.plugin_id,
                format!(
                    "report payload {bytes} bytes exceeds local limit {}",
                    self.limits.max_payload_bytes
                ),
            ));
        }
        let sequence = match self.checkpoint.as_ref() {
            Some(checkpoint) => checkpoint
                .sequence
                .checked_add(1)
                .ok_or_else(|| self.state_failure("report sequence overflow; no new sequence"))?,
            None => 1,
        };
        let publication = ReportPublication {
            format: REPORT_PUBLICATION_FORMAT.to_string(),
            plugin_id: self.binding.plugin_id.clone(),
            binding: self.binding.binding.clone(),
            schema_namespace: schema_namespace.to_string(),
            schema_version,
            sequence,
            reported_at_unix_ms,
            execution_id,
            durability: Durability::Durable,
            payload_digest: canonical_digest(&payload)?,
            payload,
        };
        publication.validate()?;
        self.store.begin(&publication)?;
        self.pending = Some(publication.clone());
        Ok(publication)
    }

    /// Send or replay the pending record with the per-launch report grant.
    pub async fn publish_pending<T: ReportTransport + ?Sized>(
        &mut self,
        client: &mut T,
        grant: &str,
    ) -> Result<PublishOutcome, ContractError> {
        if self.fenced {
            return Ok(PublishOutcome::Fenced);
        }
        let Some(publication) = self.pending.clone() else {
            return Err(self.state_failure("no pending publication to publish"));
        };
        let now = (self.clock)();
        if let Some(retry_at) = self.retry_at_ms {
            if now < retry_at {
                return Ok(PublishOutcome::Pending);
            }
        }

        let request = json!({
            "grant": grant,
            "plugin_id": self.binding.plugin_id,
            "generation": self.binding.binding.generation,
            "publication": publication,
        });
        let duration = Duration::from_millis(self.limits.publish_deadline_ms.max(1));
        let attempt = tokio::time::timeout(duration, client.publish_report(request)).await;
        let response = match attempt {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return self.transport_failure(error),
            Err(_) => {
                self.note_failure(now);
                return Ok(PublishOutcome::Pending);
            }
        };
        let envelope: ReportEnvelope = match serde_json::from_value(response) {
            Ok(envelope) => envelope,
            Err(error) => {
                self.note_failure(now);
                return Err(FailureCode::InvalidRecord.failure(
                    &self.binding.plugin_id,
                    format!("malformed report acknowledgement: {error}"),
                ));
            }
        };
        if let Err(error) = self.validate_ack(&envelope, &publication) {
            self.note_failure(now);
            return Err(error);
        }
        let checkpoint = match self.store.acknowledge(&envelope) {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                self.note_failure(now);
                return Err(error);
            }
        };
        if let Err(error) = self.validate_checkpoint(&checkpoint) {
            self.note_failure(now);
            return Err(error);
        }
        if checkpoint.sequence != publication.sequence
            || checkpoint.payload_digest != publication.payload_digest
            || checkpoint.generation != self.binding.binding.generation
        {
            self.note_failure(now);
            return Err(corrupted(
                &self.binding,
                format!(
                    "store returned checkpoint {:?} for sequence {}",
                    checkpoint, publication.sequence
                ),
            ));
        }
        self.checkpoint = Some(checkpoint);
        self.pending = None;
        self.coalesce_base_ms = Some(now);
        self.retry_at_ms = None;
        self.next_retry_backoff_ms = self.initial_backoff();
        Ok(PublishOutcome::Acknowledged(envelope))
    }

    /// Last acknowledged boundary, if any.
    pub fn checkpoint(&self) -> Option<&AckCheckpoint> {
        self.checkpoint.as_ref()
    }

    /// The immutable pending record, if any.
    pub fn pending_publication(&self) -> Option<&ReportPublication> {
        self.pending.as_ref()
    }

    /// Whether this publisher is generation-fenced.
    pub fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// Earliest clock value at which the next seal is allowed.
    pub fn next_seal_at_ms(&self) -> Option<u64> {
        self.coalesce_base_ms
            .map(|base| base.saturating_add(self.limits.coalesce_after_ms))
    }

    /// Earliest clock value at which the next send attempt is allowed.
    pub fn retry_at_ms(&self) -> Option<u64> {
        self.retry_at_ms
    }

    /// Delay that will be applied to the next failed attempt.
    pub fn next_retry_backoff_ms(&self) -> u64 {
        self.next_retry_backoff_ms
    }

    fn initial_backoff(&self) -> u64 {
        INITIAL_RETRY_BACKOFF_MS.min(self.limits.max_retry_backoff_ms)
    }

    fn note_failure(&mut self, now: u64) {
        self.retry_at_ms = Some(now.saturating_add(self.next_retry_backoff_ms));
        self.next_retry_backoff_ms = self
            .next_retry_backoff_ms
            .saturating_mul(2)
            .min(self.limits.max_retry_backoff_ms);
    }

    fn transport_failure(&mut self, error: IpcError) -> Result<PublishOutcome, ContractError> {
        let now = (self.clock)();
        if error.is_unknown_outcome() {
            self.note_failure(now);
            return Ok(PublishOutcome::Pending);
        }
        match error {
            IpcError::Unavailable(message) => {
                self.note_failure(now);
                let _ = message;
                Ok(PublishOutcome::Pending)
            }
            IpcError::Contract(error) => match agalma_contracts::digest::failure_code(&error) {
                Some(FailureCode::StaleGeneration | FailureCode::AuthorityDenied) => {
                    self.fenced = true;
                    Ok(PublishOutcome::Fenced)
                }
                _ => {
                    self.note_failure(now);
                    Err(error)
                }
            },
            IpcError::Protocol(message) => {
                self.note_failure(now);
                Err(FailureCode::StateUnavailable.failure(
                    &self.binding.plugin_id,
                    format!("report transport protocol error: {message}"),
                ))
            }
        }
    }

    fn validate_publication(&self, publication: &ReportPublication) -> Result<(), ContractError> {
        if publication.plugin_id != self.binding.plugin_id
            || publication.binding != self.binding.binding
        {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.binding.plugin_id,
                "pending publication identity does not match the pinned publisher",
            ));
        }
        if !self.schema_declared(&publication.schema_namespace, publication.schema_version) {
            return Err(FailureCode::UnsupportedSchema.failure(
                &self.binding.plugin_id,
                format!(
                    "pending schema {} v{} is not declared",
                    publication.schema_namespace, publication.schema_version
                ),
            ));
        }
        publication.validate()
    }

    fn validate_ack(
        &self,
        envelope: &ReportEnvelope,
        publication: &ReportPublication,
    ) -> Result<(), ContractError> {
        envelope.validate()?;
        if envelope.plugin_id != publication.plugin_id
            || envelope.binding != publication.binding
            || envelope.schema_namespace != publication.schema_namespace
            || envelope.schema_version != publication.schema_version
            || envelope.sequence != publication.sequence
            || envelope.reported_at_unix_ms != publication.reported_at_unix_ms
            || envelope.execution_id != publication.execution_id
            || envelope.durability != publication.durability
            || envelope.payload_digest != publication.payload_digest
            || envelope.payload != publication.payload
        {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.binding.plugin_id,
                format!(
                    "acknowledgement does not match pending publication identity {:?}",
                    publication.identity()
                ),
            ));
        }
        Ok(())
    }

    fn validate_checkpoint(&self, checkpoint: &AckCheckpoint) -> Result<(), ContractError> {
        if checkpoint.generation != self.binding.binding.generation {
            return Err(FailureCode::StaleGeneration.failure(
                &self.binding.plugin_id,
                format!(
                    "checkpoint generation {} does not match pinned generation {}",
                    checkpoint.generation, self.binding.binding.generation
                ),
            ));
        }
        if checkpoint.sequence == 0 {
            return Err(corrupted(&self.binding, "checkpoint sequence must be >= 1"));
        }
        validate_digest(&checkpoint.payload_digest, &self.binding.plugin_id)
    }

    fn schema_declared(&self, namespace: &str, version: u64) -> bool {
        self.binding
            .declared_schemas
            .iter()
            .any(|schema| schema.namespace == namespace && schema.version == version)
    }

    fn state_failure(&self, message: impl AsRef<str>) -> ContractError {
        FailureCode::StateUnavailable.failure(&self.binding.plugin_id, message.as_ref())
    }
}

fn validate_limits(limits: &PublisherLimits) -> Result<(), ContractError> {
    if limits.max_payload_bytes == 0 || limits.max_payload_bytes > DEFAULT_MAX_PAYLOAD_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            "PublisherLimits",
            format!(
                "max_payload_bytes must be 1..={DEFAULT_MAX_PAYLOAD_BYTES}, got {}",
                limits.max_payload_bytes
            ),
        ));
    }
    if limits.publish_deadline_ms == 0 {
        return Err(FailureCode::InvalidRecord
            .failure("PublisherLimits", "publish_deadline_ms must be positive"));
    }
    if limits.max_retry_backoff_ms == 0 {
        return Err(FailureCode::InvalidRecord
            .failure("PublisherLimits", "max_retry_backoff_ms must be positive"));
    }
    Ok(())
}

fn validate_binding(binding: &PublisherBinding) -> Result<(), ContractError> {
    let id = &binding.plugin_id;
    let valid_id = !id.is_empty()
        && id.len() <= 256
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
        && !id.starts_with(['.', '_', ':', '/', '-']);
    if !valid_id {
        return Err(FailureCode::InvalidRecord
            .failure("PublisherBinding", format!("invalid plugin id `{id}`")));
    }
    let pin = &binding.binding;
    if pin.binding_id.is_empty()
        || pin.api.is_empty()
        || pin.api_version == 0
        || pin.implementation_version.is_empty()
        || pin.generation == 0
    {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            "BindingPin is incomplete (binding_id/api/api_version/implementation_version/generation)",
        ));
    }
    validate_digest(&pin.config_digest, id)?;
    if binding.declared_schemas.is_empty() {
        return Err(
            FailureCode::UnsupportedSchema.failure(id, "no declared report schema was supplied")
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for schema in &binding.declared_schemas {
        if schema.namespace.is_empty() || schema.version == 0 {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                "declared schema namespace/version must be non-empty/positive",
            ));
        }
        if !seen.insert((schema.namespace.as_str(), schema.version)) {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!(
                    "duplicate declared schema {} v{}",
                    schema.namespace, schema.version
                ),
            ));
        }
        // The artifact bytes are not available here; only the digest shape is
        // authoritative locally.
        validate_digest(&schema.schema.digest, &schema.namespace)?;
    }
    Ok(())
}

fn corrupted(binding: &PublisherBinding, message: impl AsRef<str>) -> ContractError {
    FailureCode::StateUnavailable.failure(&binding.plugin_id, message.as_ref())
}
