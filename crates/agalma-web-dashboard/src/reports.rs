//! Dashboard-owned readiness reporting (CW.3).
//!
//! The dashboard publishes its **own** observed readiness through the delivered
//! CW.2 [`agalma_ipc::report_publisher`] primitives. Observation is separate
//! from the browser read path: a dedicated task samples [`RuntimeStatus`] on the
//! frozen five-second cadence, seals one immutable publication, and sends it
//! with the separate per-launch `report.publish` grant. A successful publication
//! never marks the read binding ready and a failed read is never reported as
//! healthy.
//!
//! Persistence is caller-owned: one bounded `DashboardReportState/v1` document
//! under the host's private state root,
//!
//! ```text
//! <state root>/report-outboxes/agalma.web-dashboard/<sha256(plugin+pin)>/state.json
//! ```
//!
//! derived from the launch socket's stable parent (the epoch-specific socket
//! file name, package path and hub ledger are never part of the identity). The
//! document retains the acknowledged canonical envelope plus at most one
//! immutable pending publication within a hard 256 KiB bound. Writes are
//! same-directory temporary-file + flush/sync + rename + directory sync; a lost
//! acknowledgement therefore replays the exact persisted bytes and a corrupt or
//! foreign document fails closed without resetting the sequence.
//!
//! The process snapshot exposes only the acknowledged boundary. Pending content
//! is deliberately absent: a report that the hub has not acknowledged is not
//! observable state.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use agalma_contracts::digest::{canonical_digest, validate_digest};
use agalma_contracts::{
    BindingPin, ContractError, FailureCode, PackageArtifactRef, PluginSnapshotEntry,
    ReportEnvelope, ReportPublication, ReportSchemaRef, REPORT_ENVELOPE_FORMAT,
};
use agalma_ipc::report_publisher::{
    AckCheckpoint, LoadedPublication, PublicationStore, PublishOutcome, PublisherBinding,
    PublisherLimits, ReportPublisher,
};
use agalma_ipc::HubCallbackClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::plugin::{RuntimeStatus, StatusSnapshot};
use crate::PLUGIN_ID;

/// The frozen manifest report namespace.
pub const REPORT_SCHEMA_NAMESPACE: &str = "agalma.web-dashboard.status";
/// The frozen manifest report schema version.
pub const REPORT_SCHEMA_VERSION: u64 = 1;
/// The separate per-launch publication authority. It never implies a read or
/// control capability.
pub const REPORT_PUBLISH_CAPABILITY: &str = "report.publish";
/// Hard bound for the one owned state document.
pub const REPORT_STATE_LIMIT_BYTES: usize = 256 * 1024;
/// Frozen observation/heartbeat cadence.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// State document format.
pub const REPORT_STATE_FORMAT: &str = "DashboardReportState/v1";
/// Private subtree name below the host state root.
pub const REPORT_STATE_SUBTREE: &str = "report-outboxes";

/// Compact, versioned state document retained by the dashboard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredState {
    format: String,
    plugin_id: String,
    binding: BindingPin,
    checkpoint: Option<StoredCheckpoint>,
    acknowledged: Option<ReportEnvelope>,
    pending: Option<ReportPublication>,
}

/// Serializable form of [`AckCheckpoint`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint {
    generation: u64,
    sequence: u64,
    payload_digest: String,
    acknowledged_at_unix_ms: u64,
}

impl StoredCheckpoint {
    fn from_checkpoint(checkpoint: AckCheckpoint) -> Self {
        Self {
            generation: checkpoint.generation,
            sequence: checkpoint.sequence,
            payload_digest: checkpoint.payload_digest,
            acknowledged_at_unix_ms: checkpoint.acknowledged_at_unix_ms,
        }
    }

    fn into_checkpoint(self) -> AckCheckpoint {
        AckCheckpoint {
            generation: self.generation,
            sequence: self.sequence,
            payload_digest: self.payload_digest,
            acknowledged_at_unix_ms: self.acknowledged_at_unix_ms,
        }
    }
}

impl StoredState {
    fn empty(binding: &BindingPin) -> Self {
        Self {
            format: REPORT_STATE_FORMAT.to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding: binding.clone(),
            checkpoint: None,
            acknowledged: None,
            pending: None,
        }
    }
}

/// The declared schema bytes compiled from the installed package artifact. The
/// manifest pins the identical digest, so the publisher can never announce a
/// namespace/version the installed generation did not declare.
const REPORT_SCHEMA_BYTES: &[u8] = include_bytes!("../package/objects/report-schema.json");

/// The exact declared schema reference from the installed generation.
pub fn report_schema_ref() -> ReportSchemaRef {
    ReportSchemaRef {
        namespace: REPORT_SCHEMA_NAMESPACE.to_string(),
        version: REPORT_SCHEMA_VERSION,
        schema: PackageArtifactRef::new(
            "objects/report-schema.json",
            REPORT_SCHEMA_BYTES,
            "application/json",
        ),
    }
}

/// The publisher binding for the dashboard's own pin.
pub fn publisher_binding(binding: BindingPin) -> PublisherBinding {
    PublisherBinding {
        plugin_id: PLUGIN_ID.to_string(),
        binding,
        declared_schemas: vec![report_schema_ref()],
    }
}

/// The canonical readiness payload (the frozen manifest fields only).
///
/// Source is [`RuntimeStatus`] alone. `url` is the actual loopback base URL
/// once the listener exists and `null` otherwise.
pub fn status_payload(snapshot: &StatusSnapshot) -> Value {
    json!({
        "listener": readiness_word(snapshot.listener),
        "binding": readiness_word(snapshot.binding),
        "url": snapshot.url,
    })
}

fn readiness_word(ready: bool) -> &'static str {
    if ready {
        "ready"
    } else {
        "unavailable"
    }
}

/// The stable parent the host state root is derived from.
///
/// `plugin_socket_path` uses `<state root>/plugin-sockets/<epoch>.sock`, or a
/// stable per-state-root hashed base under the platform temp dir when the
/// direct path would exceed `SUN_LEN`. Both parents are stable for a given host
/// state root; neither the epoch-specific socket filename nor the installed
/// package/config path participates.
pub fn report_state_root(socket: &Path) -> PathBuf {
    let parent = socket.parent().unwrap_or_else(|| Path::new("."));
    if parent
        .file_name()
        .is_some_and(|name| name == "plugin-sockets")
    {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

/// The identity directory for one full plugin/binding pin.
pub fn report_identity_dir(
    state_root: &Path,
    binding: &BindingPin,
) -> Result<PathBuf, ContractError> {
    let identity = canonical_digest(&json!({
        "plugin_id": PLUGIN_ID,
        "binding": binding,
    }))?;
    let hex = identity.strip_prefix("sha256:").unwrap_or(&identity);
    Ok(state_root
        .join(REPORT_STATE_SUBTREE)
        .join(PLUGIN_ID)
        .join(hex))
}

/// The one state document path for a launch socket and binding pin.
pub fn report_state_path(socket: &Path, binding: &BindingPin) -> Result<PathBuf, ContractError> {
    Ok(report_identity_dir(&report_state_root(socket), binding)?.join("state.json"))
}

/// Caller-owned single-writer durable store for the dashboard publication.
pub struct FilePublicationStore {
    path: PathBuf,
    directory: PathBuf,
    state_root: PathBuf,
    binding: BindingPin,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    loaded: Option<StoredState>,
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl FilePublicationStore {
    /// Bind the store to one private state root and full binding pin.
    pub fn open_in_state_root(
        state_root: &Path,
        binding: BindingPin,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, ContractError> {
        let directory = report_identity_dir(state_root, &binding)?;
        Ok(Self {
            path: directory.join("state.json"),
            directory,
            state_root: state_root.to_path_buf(),
            binding,
            clock,
            loaded: None,
        })
    }

    /// Bind the store to the launch socket's stable state root.
    pub fn open_for_socket(
        socket: &Path,
        binding: BindingPin,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, ContractError> {
        Self::open_in_state_root(&report_state_root(socket), binding, clock)
    }

    /// The exact owned state document.
    pub fn state_path(&self) -> &Path {
        &self.path
    }

    /// The last acknowledged canonical envelope, if any.
    pub fn acknowledged(&mut self) -> Result<Option<ReportEnvelope>, ContractError> {
        Ok(self.state()?.acknowledged.clone())
    }

    fn state(&mut self) -> Result<StoredState, ContractError> {
        if let Some(state) = &self.loaded {
            return Ok(state.clone());
        }
        let state = self.read_committed()?;
        self.loaded = Some(state.clone());
        Ok(state)
    }

    fn read_committed(&self) -> Result<StoredState, ContractError> {
        validate_existing_dirs(&self.state_root, &self.directory)?;
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoredState::empty(&self.binding));
            }
            Err(error) => {
                return Err(state_failure(format!(
                    "cannot inspect report state {}: {error}",
                    self.path.display()
                )))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(state_failure(format!(
                "report state {} is not a regular file",
                self.path.display()
            )));
        }
        if metadata.len() > REPORT_STATE_LIMIT_BYTES as u64 {
            return Err(state_failure(format!(
                "report state {} exceeds {REPORT_STATE_LIMIT_BYTES} bytes",
                self.path.display()
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&self.path)
            .and_then(|file| {
                file.take(REPORT_STATE_LIMIT_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|error| {
                state_failure(format!(
                    "cannot read report state {}: {error}",
                    self.path.display()
                ))
            })?;
        if bytes.len() > REPORT_STATE_LIMIT_BYTES {
            return Err(state_failure(format!(
                "report state {} exceeds {REPORT_STATE_LIMIT_BYTES} bytes",
                self.path.display()
            )));
        }
        let state: StoredState = serde_json::from_slice(&bytes).map_err(|error| {
            state_failure(format!(
                "corrupt report state {}: {error}",
                self.path.display()
            ))
        })?;
        validate_state_shape(&state)?;
        Ok(state)
    }

    fn write_committed(&self, state: &StoredState) -> Result<(), ContractError> {
        validate_state(state, &self.binding)?;
        let bytes = serde_json::to_vec(state)
            .map_err(|error| state_failure(format!("cannot encode report state: {error}")))?;
        if bytes.len() > REPORT_STATE_LIMIT_BYTES {
            return Err(state_failure(format!(
                "report state would exceed {REPORT_STATE_LIMIT_BYTES} bytes"
            )));
        }
        // Create and validate every directory below the caller-owned root. A
        // symlinked component is refused, so this writer can never escape its
        // private subtree or overwrite another binding's files.
        ensure_state_dirs(&self.state_root, &self.directory)?;
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_symlink() {
                return Err(state_failure(format!(
                    "refusing symlinked report state file {}",
                    self.path.display()
                )));
            }
        }
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let temporary = self
            .directory
            .join(format!(".state.json.tmp-{}-{sequence}", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| {
                    state_failure(format!(
                        "cannot create report state temporary {}: {error}",
                        temporary.display()
                    ))
                })?;
            file.write_all(&bytes)
                .and_then(|_| file.flush())
                .and_then(|_| file.sync_all())
                .map_err(|error| {
                    state_failure(format!(
                        "cannot persist report state temporary {}: {error}",
                        temporary.display()
                    ))
                })?;
            drop(file);
            fs::rename(&temporary, &self.path).map_err(|error| {
                state_failure(format!(
                    "cannot commit report state {}: {error}",
                    self.path.display()
                ))
            })?;
            File::open(&self.directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    state_failure(format!(
                        "cannot sync report state directory {}: {error}",
                        self.directory.display()
                    ))
                })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn commit(&mut self, state: StoredState) -> Result<(), ContractError> {
        self.write_committed(&state)?;
        self.loaded = Some(state);
        Ok(())
    }
}

impl PublicationStore for FilePublicationStore {
    fn load(&mut self) -> Result<LoadedPublication, ContractError> {
        let state = self.state()?;
        if state.binding != self.binding {
            if state.binding.generation != self.binding.generation {
                return Ok(LoadedPublication::Fenced {
                    generation: state.binding.generation,
                });
            }
            return Err(FailureCode::IntegrityMismatch.failure(
                PLUGIN_ID,
                "report state binding does not match the pinned installation",
            ));
        }
        let checkpoint = state
            .checkpoint
            .clone()
            .map(StoredCheckpoint::into_checkpoint);
        Ok(match state.pending {
            Some(publication) => LoadedPublication::Pending {
                publication,
                checkpoint,
            },
            None => LoadedPublication::Empty { checkpoint },
        })
    }

    fn begin(&mut self, publication: &ReportPublication) -> Result<(), ContractError> {
        let mut state = self.state()?;
        if state.pending.is_some() {
            return Err(state_failure("a pending report publication already exists"));
        }
        if publication.plugin_id != PLUGIN_ID
            || publication.binding != self.binding
            || publication.binding.generation != self.binding.generation
        {
            return Err(FailureCode::StaleGeneration.failure(
                PLUGIN_ID,
                "report publication is not bound to this generation",
            ));
        }
        publication.validate()?;
        state.pending = Some(publication.clone());
        self.commit(state)
    }

    fn acknowledge(&mut self, envelope: &ReportEnvelope) -> Result<AckCheckpoint, ContractError> {
        let mut state = self.state()?;
        let pending = state
            .pending
            .clone()
            .ok_or_else(|| state_failure("no pending report publication to acknowledge"))?;
        if envelope.format != REPORT_ENVELOPE_FORMAT
            || envelope.plugin_id != pending.plugin_id
            || envelope.binding != pending.binding
            || envelope.schema_namespace != pending.schema_namespace
            || envelope.schema_version != pending.schema_version
            || envelope.sequence != pending.sequence
            || envelope.reported_at_unix_ms != pending.reported_at_unix_ms
            || envelope.execution_id != pending.execution_id
            || envelope.durability != pending.durability
            || envelope.payload_digest != pending.payload_digest
            || envelope.payload != pending.payload
        {
            return Err(FailureCode::IntegrityMismatch.failure(
                PLUGIN_ID,
                "acknowledgement does not match the pending report identity",
            ));
        }
        envelope.validate()?;
        let checkpoint = AckCheckpoint {
            generation: pending.binding.generation,
            sequence: pending.sequence,
            payload_digest: pending.payload_digest.clone(),
            acknowledged_at_unix_ms: (self.clock)(),
        };
        state.checkpoint = Some(StoredCheckpoint::from_checkpoint(checkpoint.clone()));
        state.acknowledged = Some(envelope.clone());
        state.pending = None;
        self.commit(state)?;
        Ok(checkpoint)
    }
}

fn validate_state(state: &StoredState, expected: &BindingPin) -> Result<(), ContractError> {
    validate_state_shape(state)?;
    if state.binding != *expected {
        return Err(FailureCode::IntegrityMismatch.failure(
            PLUGIN_ID,
            "report state binding does not match the pinned installation",
        ));
    }
    Ok(())
}

/// Validate the document's internal consistency independently of the pin the
/// caller is opening. A foreign generation is fenced by [`FilePublicationStore::load`]
/// after this check rather than treated as corrupt.
fn validate_state_shape(state: &StoredState) -> Result<(), ContractError> {
    if state.format != REPORT_STATE_FORMAT {
        return Err(state_failure(format!(
            "unsupported report state format `{}`",
            state.format
        )));
    }
    if state.plugin_id != PLUGIN_ID {
        return Err(FailureCode::IntegrityMismatch.failure(
            PLUGIN_ID,
            format!(
                "report state belongs to `{}`, not `{PLUGIN_ID}`",
                state.plugin_id
            ),
        ));
    }
    match (&state.checkpoint, &state.acknowledged) {
        (None, None) => {}
        (Some(checkpoint), Some(envelope)) => {
            if checkpoint.generation != state.binding.generation
                || envelope.binding != state.binding
                || envelope.plugin_id != PLUGIN_ID
                || envelope.sequence != checkpoint.sequence
                || envelope.payload_digest != checkpoint.payload_digest
                || envelope.schema_namespace != REPORT_SCHEMA_NAMESPACE
                || envelope.schema_version != REPORT_SCHEMA_VERSION
            {
                return Err(state_failure(
                    "acknowledged report boundary is inconsistent with its checkpoint",
                ));
            }
            if checkpoint.sequence == 0 {
                return Err(state_failure("acknowledged report sequence must be >= 1"));
            }
            validate_digest(&checkpoint.payload_digest, PLUGIN_ID)?;
            envelope.validate()?;
        }
        _ => {
            return Err(state_failure(
                "report state checkpoint and acknowledged envelope must both be present or absent",
            ))
        }
    }
    if let Some(pending) = &state.pending {
        if pending.plugin_id != PLUGIN_ID
            || pending.binding != state.binding
            || pending.schema_namespace != REPORT_SCHEMA_NAMESPACE
            || pending.schema_version != REPORT_SCHEMA_VERSION
        {
            return Err(FailureCode::IntegrityMismatch.failure(
                PLUGIN_ID,
                "pending report identity does not match this installation",
            ));
        }
        let expected_sequence = match &state.checkpoint {
            Some(checkpoint) => checkpoint
                .sequence
                .checked_add(1)
                .ok_or_else(|| state_failure("report sequence overflow"))?,
            None => 1,
        };
        if pending.sequence != expected_sequence {
            return Err(state_failure(format!(
                "pending report sequence {} does not follow acknowledged sequence {}",
                pending.sequence,
                expected_sequence - 1
            )));
        }
        pending.validate()?;
    }
    Ok(())
}

/// Create and validate every directory component below the caller-owned root.
/// A symlinked intermediate directory is refused rather than followed.
fn ensure_state_dirs(root: &Path, directory: &Path) -> Result<(), ContractError> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| state_failure("report state directory is outside its private root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        ensure_real_dir(&current)?;
    }
    Ok(())
}

fn validate_existing_dirs(root: &Path, directory: &Path) -> Result<(), ContractError> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| state_failure("report state directory is outside its private root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(state_failure(format!(
                        "refusing symlinked report state directory {}",
                        current.display()
                    )));
                }
                if !metadata.is_dir() {
                    return Err(state_failure(format!(
                        "report state path {} is not a directory",
                        current.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(state_failure(format!(
                    "cannot inspect report state directory {}: {error}",
                    current.display()
                )))
            }
        }
    }
    Ok(())
}

fn ensure_real_dir(path: &Path) -> Result<(), ContractError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(state_failure(format!(
                    "refusing symlinked report state directory {}",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(state_failure(format!(
                    "report state path {} is not a directory",
                    path.display()
                )))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure_real_dir(path)
            }
            Err(error) => Err(state_failure(format!(
                "cannot create report state directory {}: {error}",
                path.display()
            ))),
        },
        Err(error) => Err(state_failure(format!(
            "cannot inspect report state directory {}: {error}",
            path.display()
        ))),
    }
}

fn state_failure(message: impl Into<String>) -> ContractError {
    FailureCode::StateUnavailable.failure(PLUGIN_ID, message.into())
}

/// The acknowledged report boundary shared with `plugin.Snapshot`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObservedReport {
    /// Acknowledged sequence watermark (`0` before the first acknowledgement).
    pub watermark: u64,
    /// Acknowledged canonical payload, never a pending sample.
    pub payload: Option<Value>,
    /// Canonical digest of `payload`.
    pub payload_digest: Option<String>,
}

impl ObservedReport {
    /// The acknowledged snapshot entry, if any.
    pub fn entry(&self) -> Option<PluginSnapshotEntry> {
        let payload = self.payload.clone()?;
        let payload_digest = self.payload_digest.clone()?;
        Some(PluginSnapshotEntry {
            schema_namespace: REPORT_SCHEMA_NAMESPACE.to_string(),
            schema_version: REPORT_SCHEMA_VERSION,
            payload,
            payload_digest,
        })
    }

    fn acknowledge(&mut self, envelope: &ReportEnvelope) {
        self.watermark = envelope.sequence;
        self.payload = Some(envelope.payload.clone());
        self.payload_digest = Some(envelope.payload_digest.clone());
    }
}

/// Process-shared acknowledged report observation.
#[derive(Debug, Default)]
pub struct ReportObservation {
    inner: Mutex<ObservedReport>,
}

impl ReportObservation {
    /// A fresh observation with no acknowledged report.
    pub fn new() -> Self {
        Self::default()
    }

    /// The acknowledged boundary only (pending content is never included).
    pub fn observed(&self) -> ObservedReport {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Record one hub acknowledgement.
    pub fn acknowledge(&self, envelope: &ReportEnvelope) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .acknowledge(envelope);
    }
}

/// A spawned reporting task with a bounded cancel/join handle.
pub struct ReportingHandle {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ReportingHandle {
    /// Cancel and join the reporter. Any pending publication stays durable.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Start the production reporting task for a supervised dashboard launch.
///
/// Absent report authority is not an error: the dashboard simply does not
/// report. A state/grant failure degrades reporting only; the listener and read
/// path continue to serve.
pub fn start_reporting(
    socket: &Path,
    hub_socket: PathBuf,
    report_grant: String,
    status: Arc<RuntimeStatus>,
    observation: Arc<ReportObservation>,
) -> Result<ReportingHandle, String> {
    let binding = status.binding_pin();
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(unix_now_ms);
    let mut store = FilePublicationStore::open_for_socket(socket, binding.clone(), clock.clone())
        .map_err(|error| format!("report state unavailable: {error}"))?;
    if let Some(envelope) = store
        .acknowledged()
        .map_err(|error| format!("report state unavailable: {error}"))?
    {
        observation.acknowledge(&envelope);
    }
    let limits = PublisherLimits {
        coalesce_after_ms: REPORT_INTERVAL.as_millis() as u64,
        ..PublisherLimits::default()
    };
    let publisher = ReportPublisher::open(store, publisher_binding(binding), limits, clock)
        .map_err(|error| format!("report publisher refused: {error}"))?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(reporting_loop(
        hub_socket,
        report_grant,
        publisher,
        status,
        observation,
        shutdown_rx,
    ));
    Ok(ReportingHandle {
        shutdown: shutdown_tx,
        task,
    })
}

async fn reporting_loop(
    hub_socket: PathBuf,
    report_grant: String,
    mut publisher: ReportPublisher<FilePublicationStore>,
    status: Arc<RuntimeStatus>,
    observation: Arc<ReportObservation>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut client: Option<HubCallbackClient> = None;
    let mut backoff = Duration::from_millis(250);
    let max_backoff = Duration::from_secs(5);

    loop {
        if *shutdown.borrow() {
            return;
        }
        let wait = if publisher.pending_publication().is_none() {
            let now = unix_now_ms();
            if publisher.next_seal_at_ms().is_some_and(|ready| now < ready) {
                Duration::from_millis(
                    publisher
                        .next_seal_at_ms()
                        .unwrap_or(now)
                        .saturating_sub(now)
                        .clamp(1, 5_000),
                )
            } else {
                let payload = status_payload(&status.snapshot());
                match publisher.seal(REPORT_SCHEMA_NAMESPACE, REPORT_SCHEMA_VERSION, payload, now) {
                    Ok(_) => Duration::from_millis(1),
                    Err(error) => {
                        eprintln!("agalma-web-dashboard: report seal pending: {error}");
                        backoff = (backoff * 2).min(max_backoff);
                        backoff
                    }
                }
            }
        } else if let Some(retry_at) = publisher.retry_at_ms() {
            let now = unix_now_ms();
            if now < retry_at {
                Duration::from_millis(retry_at.saturating_sub(now).clamp(1, 5_000))
            } else {
                Duration::from_millis(1)
            }
        } else {
            Duration::from_millis(1)
        };

        if wait > Duration::from_millis(1) {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = tokio::time::sleep(wait) => {}
            }
            continue;
        }

        if publisher.pending_publication().is_none() {
            continue;
        }

        if client.is_none() {
            match HubCallbackClient::connect(&hub_socket).await {
                Ok(connected) => {
                    client = Some(connected);
                    backoff = Duration::from_millis(250);
                }
                Err(error) => {
                    eprintln!("agalma-web-dashboard: report hub unavailable: {error}");
                    backoff = (backoff * 2).min(max_backoff);
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return;
                            }
                        }
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    continue;
                }
            }
        }
        let Some(connected) = client.as_mut() else {
            continue;
        };
        match publisher.publish_pending(connected, &report_grant).await {
            Ok(PublishOutcome::Acknowledged(envelope)) => {
                observation.acknowledge(&envelope);
                backoff = Duration::from_millis(250);
            }
            Ok(PublishOutcome::Pending) => {
                eprintln!("agalma-web-dashboard: report publication pending");
                client = None;
            }
            Ok(PublishOutcome::Fenced) => {
                eprintln!("agalma-web-dashboard: report publication fenced");
                return;
            }
            Err(error) => {
                eprintln!("agalma-web-dashboard: report publication failed: {error}");
                client = None;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
