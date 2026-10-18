//! Continuous single-owner queue runtime (`agalma work --watch`, WEB.10).
//!
//! `agalma work --watch` is an explicit continuous mode for **one**
//! repository/state directory. It reuses the existing task selection, claim,
//! frozen admission and synchronous execution pipeline (see [`super`]) and keeps
//! the idle/paused runtime observable while it supervises plugins during long
//! work. It is automatic *consumption* of already-eligible tasks, never automatic
//! task release.
//!
//! ## Responsibilities and ownership
//!
//! Three responsibilities progress independently, and the runtime never fuses
//! them:
//!
//! 1. **The worker** ([`ContinuousRuntime`]) owns queue selection, claim,
//!    admission and serial execution. It runs on the thread that holds the state
//!    directory [`crate::LockGuard`] and is the sole claimant; it never launches a
//!    second `agalma work` process or a parallel builder pool.
//! 2. **The supervision owner** ([`SupervisionHandle`]) constructs the plugin
//!    host in its own native thread and keeps the host health/`supervise_tick`
//!    loop (and hub callback serving) progressing at a nominal 250 ms cadence
//!    while the worker is idle, running or paused. The hub capability socket is
//!    served and confirmed **before** any child is launched or probed, and the
//!    serve task's result is retained and surfaced. `run_task` blocks the worker,
//!    so a supervision *thread* is required; mutable non-`Send` host objects are
//!    constructed inside that thread and never migrate or become global state.
//! 3. **Wake hints** ([`RuntimeControl`]) use a bounded channel of capacity 16
//!    with latest-value coalescing. The channel carries only "recheck" hints; the
//!    durable stop decision is always re-read from [`crate::stop`], so coalescing
//!    a hint can never drop a stop decision. A shutdown request is consulted by
//!    admission and phase-stop checks (see [`WatchStopControl`]) so no claim
//!    races the signal.
//!
//! ## Observation seam
//!
//! [`FactoryRuntimeSource`] implements the WEB.9
//! [`agalma_contracts::RuntimeObservationSource`] trait against the shared
//! [`RuntimeShared`] cell. It publishes immutable [`agalma_contracts::FactoryRuntime`]
//! snapshots: bounded eligible pages (default 50 / max 100 / 256 KiB) sorted by
//! `task_id`, exact decimal timestamps/revisions, a nullable current execution
//! that carries the actually allocated durable execution/phase, explicit
//! per-source availability, and raw permissions that default to `false`. Runtime
//! revisions are process-epoch-scoped monotonic values tied to **queue** changes
//! (never to a host heartbeat); a cursor minted by a previous launch fails closed
//! with [`agalma_contracts::FailureCode::RevisionConflict`]. A transient source
//! failure preserves the process epoch so a later snapshot can still page.
//!
//! ## Startup under a stop
//!
//! Established, readable, compatible storage may start host/read/control
//! services in [`FactoryRuntimeStatus::Paused`] while STOP/latch is active, but
//! **no** proxy, effectful recovery or planning runs until admission resumes.
//! Missing/unreadable/incompatible stopped storage yields
//! [`FactoryRuntimeStatus::Unavailable`] with no create/init/migrate,
//! proxy/planner/worker dispatch or replacement storage ([`classify_startup`]).
//! A stop is never auto-cleared here; only the human `agalma resume` clears it.
//!
//! ## Graceful shutdown
//!
//! [`ContinuousRuntime::run_until`] stops new admission, waits for the current
//! cooperative phase to reach its terminal/parked boundary, then shuts the worker
//! owner down. The caller joins the supervision owner and only then releases the
//! [`crate::LockGuard`]. A fatal worker error is propagated as a failure exit and
//! never silently replaced by a new worker. Forced termination leaves durable
//! non-terminal state for frozen boot recovery; no fake terminal receipt is ever
//! written and the lock is never force-broken.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use agalma_contracts::control_plane::{ExactU64, SourceState};
use agalma_contracts::factory::{
    check_factory_cursor, FactoryEligibleEntry, FactoryEligiblePage, FactoryExecutionObservation,
    FactoryQueueCursor, FactoryRuntime, FactoryRuntimePermissions, FactoryRuntimeRequest,
    FactoryRuntimeSource as FactorySourceRecord, FactoryRuntimeStatus, FactorySourceName,
    FactoryStopObservation, RuntimeObservationSource, FACTORY_QUEUE_CURSOR_FORMAT,
    FACTORY_RUNTIME_FORMAT, FACTORY_TITLE_MAX_BYTES,
};
use agalma_contracts::{
    ClaimEvidence, ContractError, ExecutionPhase, ExecutionState, LedgerApi, Task, TaskQueueApi,
};
use agalma_decision::StaticDecider;

use crate::harness_binding::HarnessHandle;
use crate::ledger_process::LedgerHandle;
use crate::plugin_registry::{DesiredState, LifecycleState};
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::stop::{StopControl, StopDecision};
use crate::taskqueue_process::TaskQueueHandle;
use crate::LockGuard;

/// Idle wait after a successful-but-empty ready read (interruptible).
pub const WATCH_IDLE_WAIT: Duration = Duration::from_secs(5);
/// Initial bounded retry delay after a queue read failure.
pub const WATCH_QUEUE_RETRY_INITIAL: Duration = Duration::from_secs(5);
/// Maximum bounded retry delay after repeated queue read failures.
pub const WATCH_QUEUE_RETRY_MAX: Duration = Duration::from_secs(30);
/// Nominal plugin supervision cadence.
pub const WATCH_SUPERVISION_TICK: Duration = Duration::from_millis(250);
/// Bounded wake-hint channel capacity.
pub const WATCH_NOTIFICATION_CAPACITY: usize = 16;
/// Bounded wait for the hub capability socket to bind before plugin startup.
pub const WATCH_HUB_BIND_TIMEOUT: Duration = Duration::from_secs(2);

/// Index of the `Queue` source in [`FactorySourceName::ALL`] order.
const QUEUE_SOURCE_INDEX: usize = 2;
/// Index of the `Execution` source in [`FactorySourceName::ALL`] order.
const EXECUTION_SOURCE_INDEX: usize = 3;
/// Index of the `Stop` source in [`FactorySourceName::ALL`] order.
const STOP_SOURCE_INDEX: usize = 4;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A stable, opaque, non-path identity for a configured repository/state pair.
///
/// The `FactoryRuntime/v1` contract refuses private absolute paths, so the
/// configured identity is the final path component (never the absolute path).
pub fn opaque_repository_id(repo: &Path) -> String {
    component_identity(repo, "agalma")
}

/// A stable, opaque, non-path identity for the configured state directory.
pub fn opaque_state_id(state_dir: &Path) -> String {
    component_identity(state_dir, "state")
}

fn component_identity(path: &Path, fallback: &str) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned == ".." || cleaned == "." {
        fallback.to_string()
    } else {
        truncate_bytes(&cleaned, agalma_contracts::FACTORY_OPAQUE_ID_MAX_BYTES)
    }
}

/// Truncate `text` to at most `max` UTF-8 bytes on a character boundary.
fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Process-epoch launch identity: opaque, bounded, and never reused across
/// restarts (a restart therefore invalidates older queue cursors).
fn launch_identity() -> String {
    format!("run-{}-{}", std::process::id(), now_ms())
}

// ---------------------------------------------------------------------------
// Shared observation cell
// ---------------------------------------------------------------------------

/// One per-source observation with the time it was actually observed (never the
/// snapshot read time).
#[derive(Clone, Debug)]
struct SourceRecord {
    state: SourceState,
    detail: Option<String>,
    observed_at_unix_ms: Option<u64>,
}

impl Default for SourceRecord {
    fn default() -> Self {
        SourceRecord {
            state: SourceState::Unknown,
            detail: None,
            observed_at_unix_ms: None,
        }
    }
}

/// One published runtime observation (immutable snapshot data).
#[derive(Clone, Debug)]
struct Observation {
    status: FactoryRuntimeStatus,
    reason: Option<String>,
    runtime_instance_id: Option<String>,
    revision: u64,
    current_execution: Option<FactoryExecutionObservation>,
    eligible: Vec<FactoryEligibleEntry>,
    stop: Option<FactoryStopObservation>,
    /// Per-source observation in [`FactorySourceName::ALL`] order.
    sources: Vec<SourceRecord>,
}

impl Default for Observation {
    fn default() -> Self {
        Observation {
            status: FactoryRuntimeStatus::Starting,
            reason: None,
            runtime_instance_id: None,
            revision: 1,
            current_execution: None,
            eligible: Vec::new(),
            stop: None,
            sources: vec![SourceRecord::default(); FactorySourceName::ALL.len()],
        }
    }
}

/// The mutable runtime observation shared between the worker, the supervision
/// owner, the control seam and the snapshot source.
///
/// The mutex is held only to copy a short observation value, never across task
/// execution or slow I/O.
pub struct RuntimeShared {
    inner: Mutex<Observation>,
}

impl Default for RuntimeShared {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeShared {
    pub fn new() -> Self {
        RuntimeShared {
            inner: Mutex::new(Observation::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Observation> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn observation(&self) -> Observation {
        self.lock().clone()
    }

    /// The currently published status (test/diagnostics accessor).
    pub fn status(&self) -> FactoryRuntimeStatus {
        self.lock().status
    }

    /// The current monotonic runtime revision (test/diagnostics accessor).
    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    /// The current process-epoch launch identity, if any.
    pub fn runtime_instance_id(&self) -> Option<String> {
        self.lock().runtime_instance_id.clone()
    }

    /// Advance the queue revision. Only queue-affecting updates call this; a
    /// host heartbeat or a current-execution update never does.
    fn bump(&self, obs: &mut Observation) {
        obs.revision = obs.revision.saturating_add(1);
    }

    fn set_source(&self, obs: &mut Observation, index: usize, state: SourceState, detail: String) {
        if index < obs.sources.len() {
            obs.sources[index] = SourceRecord {
                state,
                detail: if detail.is_empty() { None } else { Some(detail) },
                observed_at_unix_ms: Some(now_ms()),
            };
        }
    }

    /// Seed the launch identity and repository/state identities after startup.
    pub fn initialize(&self, instance: Option<String>, sources: Vec<(SourceState, String)>) {
        let mut obs = self.lock();
        obs.runtime_instance_id = instance;
        obs.sources = sources
            .into_iter()
            .map(|(state, detail)| SourceRecord {
                state,
                detail: if detail.is_empty() { None } else { Some(detail) },
                observed_at_unix_ms: Some(now_ms()),
            })
            .collect();
        self.bump(&mut obs);
    }

    /// Publish `Starting` before the first observation.
    pub fn publish_starting(&self, instance: String) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Starting;
        obs.reason = None;
        obs.runtime_instance_id = Some(instance);
        obs.current_execution = None;
        obs.eligible.clear();
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.bump(&mut obs);
    }

    /// Publish `Recovering`: frozen non-terminal recovery runs before new claims.
    pub fn publish_recovering(&self, instance: String) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Recovering;
        obs.reason = None;
        obs.runtime_instance_id = Some(instance);
        obs.current_execution = None;
        obs.eligible.clear();
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.bump(&mut obs);
    }

    /// Publish the observed stop latch. This is a live heartbeat, not a queue
    /// change, so it never bumps the queue revision.
    pub fn publish_stop(&self, stop: StopDecision) {
        let mut obs = self.lock();
        let (stopped, reason) = match &stop {
            StopDecision::Running => (false, None),
            StopDecision::Stopped(reason) | StopDecision::Unavailable(reason) => {
                (true, Some(reason.clone()))
            }
        };
        obs.stop = Some(FactoryStopObservation {
            stopped,
            reason,
            observed_at_unix_ms: ExactU64::from(now_ms()),
        });
        let state = match stop {
            StopDecision::Running => SourceState::Available,
            StopDecision::Stopped(_) => SourceState::Available,
            StopDecision::Unavailable(_) => SourceState::Unreadable,
        };
        let detail = stop.reason().unwrap_or("").to_string();
        self.set_source(&mut obs, STOP_SOURCE_INDEX, state, detail);
    }

    /// Publish `Paused`: admission is blocked through the existing stop policy.
    /// The queue observation is unknown (no current read), so an unknown total
    /// stays null rather than claiming a successful empty queue.
    pub fn publish_paused(&self, reason: impl Into<String>) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Paused;
        obs.reason = Some(reason.into());
        obs.current_execution = None;
        obs.eligible.clear();
        self.set_source(&mut obs, 1, SourceState::Available, "paused".to_string());
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.bump(&mut obs);
    }

    /// Publish `Idle` after a successful empty ready read.
    pub fn publish_idle(&self, note: Option<String>) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Idle;
        obs.reason = note;
        obs.current_execution = None;
        obs.eligible.clear();
        self.set_source(&mut obs, 1, SourceState::Available, String::new());
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, SourceState::Empty, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, SourceState::Empty, String::new());
        self.bump(&mut obs);
    }

    /// Publish the actual eligible set while `Running` with a current execution.
    pub fn publish_running(
        &self,
        entries: Vec<FactoryEligibleEntry>,
        current: Option<FactoryExecutionObservation>,
    ) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Running;
        obs.reason = None;
        obs.eligible = entries;
        let queue_state = if obs.eligible.is_empty() {
            // A pre-claim read that left no remainder is not a successful empty
            // read: the queue may have received new arrivals. Mark it unknown.
            SourceState::Unknown
        } else {
            SourceState::Available
        };
        obs.current_execution = current;
        let exec_state = if obs.current_execution.is_some() {
            SourceState::Available
        } else {
            SourceState::Unknown
        };
        self.set_source(&mut obs, 1, SourceState::Available, String::new());
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, queue_state, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, exec_state, String::new());
        self.bump(&mut obs);
    }

    /// Publish the truthful post-execution state after one driven task reached
    /// its terminal boundary.
    ///
    /// The claimed task is no longer eligible. The queue observation is the
    /// **known remaining** eligible entries from the last successful read when
    /// any remain, or explicitly [`SourceState::Unknown`] (never a fabricated
    /// successful empty) when none do. The runtime rescans immediately; only a
    /// fresh successful empty read publishes [`FactoryRuntimeStatus::Idle`].
    pub fn publish_after_run(&self, remaining: Vec<FactoryEligibleEntry>) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Running;
        obs.reason = None;
        obs.current_execution = None;
        obs.eligible = remaining;
        let queue_state = if obs.eligible.is_empty() {
            SourceState::Unknown
        } else {
            SourceState::Available
        };
        self.set_source(&mut obs, 1, SourceState::Available, String::new());
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, queue_state, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.bump(&mut obs);
    }

    /// Publish the actually allocated durable current execution and phase. This
    /// is execution progress, not a queue change, so it never bumps the queue
    /// revision.
    pub fn publish_execution(&self, current: Option<FactoryExecutionObservation>) {
        let mut obs = self.lock();
        let state = if current.is_some() {
            SourceState::Available
        } else {
            SourceState::Unknown
        };
        obs.current_execution = current;
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, state, String::new());
    }

    /// Publish `Unavailable` for a source/queue failure. The process-epoch
    /// launch identity is **preserved** so a later successful snapshot can still
    /// page; only the queue/execution observations become unknown.
    pub fn publish_unavailable(&self, reason: impl Into<String>) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Unavailable;
        obs.reason = Some(reason.into());
        obs.current_execution = None;
        obs.eligible.clear();
        self.set_source(&mut obs, 1, SourceState::Unreadable, String::new());
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, SourceState::Unreadable, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.bump(&mut obs);
    }

    /// Publish `Failed` for a fatal worker error. Recovery is explicit restart.
    pub fn publish_failed(&self, reason: impl Into<String>) {
        let mut obs = self.lock();
        obs.status = FactoryRuntimeStatus::Failed;
        obs.reason = Some(reason.into());
        obs.current_execution = None;
        obs.eligible.clear();
        self.set_source(&mut obs, 1, SourceState::Unreadable, String::new());
        self.set_source(&mut obs, QUEUE_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.set_source(&mut obs, EXECUTION_SOURCE_INDEX, SourceState::Unknown, String::new());
        self.bump(&mut obs);
    }

    /// Publish the supervision owner's host health (never a worker authority
    /// source). `state` is explicit: a healthy host is [`SourceState::Available`],
    /// while a start, server or probe failure is [`SourceState::Unavailable`] or
    /// [`SourceState::Unreadable`]. This is a heartbeat and never bumps the queue
    /// revision.
    pub fn publish_host_health(&self, state: SourceState, detail: impl Into<String>) {
        let mut obs = self.lock();
        self.set_source(&mut obs, 0, state, detail.into());
    }
}

// ---------------------------------------------------------------------------
// Snapshot source
// ---------------------------------------------------------------------------

/// The WEB.10 `RuntimeObservationSource` over a [`RuntimeShared`] cell.
pub struct FactoryRuntimeSource {
    shared: Arc<RuntimeShared>,
    repository_id: String,
    state_id: String,
}

impl FactoryRuntimeSource {
    pub fn new(shared: Arc<RuntimeShared>, repository_id: String, state_id: String) -> Self {
        FactoryRuntimeSource {
            shared,
            repository_id,
            state_id,
        }
    }

    /// Build a fixed `Unavailable` source for a state whose storage cannot be
    /// read (missing/unreadable/incompatible while stopped). No ledger is opened
    /// and no storage is created.
    pub fn unavailable(repository_id: String, state_id: String, reason: String) -> Self {
        let shared = Arc::new(RuntimeShared::new());
        shared.publish_unavailable(reason);
        FactoryRuntimeSource {
            shared,
            repository_id,
            state_id,
        }
    }
}

impl RuntimeObservationSource for FactoryRuntimeSource {
    fn snapshot(&self, request: &FactoryRuntimeRequest) -> Result<FactoryRuntime, ContractError> {
        request.validate()?;
        let obs = self.shared.observation();
        let limit = request.effective_limit()? as usize;

        let cursor = check_factory_cursor(
            request.cursor.as_deref(),
            &self.repository_id,
            &self.state_id,
            obs.eligible_revision().as_deref(),
            obs.runtime_instance_id.as_deref(),
        )?;

        // A queue that has no *successful* read yet (Starting/Recovering/Paused/
        // Failed or an unreadable source) reports an unknown page: no entries, no
        // revision and a null total. An unknown source is never a successful
        // empty queue (which reports total `0`).
        if obs.status == FactoryRuntimeStatus::Unavailable
            || obs.status == FactoryRuntimeStatus::Starting
            || !obs.queue_known()
        {
            let page = FactoryEligiblePage {
                entries: Vec::new(),
                revision: None,
                total: None,
                next_cursor: None,
            };
            return self.finish(&obs, page);
        }

        let mut entries = obs.eligible.clone();
        entries.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        let total = entries.len() as u64;
        if let Some(cursor) = &cursor {
            entries.retain(|entry| entry.task_id > cursor.after);
        }
        let truncated = entries.len() > limit;
        entries.truncate(limit);

        // Paging requires a known revision and runtime identity; never panic if
        // they are absent (a transient source failure cannot abort a read).
        let next_cursor = if truncated {
            match (
                entries.last(),
                obs.eligible_revision(),
                obs.runtime_instance_id.clone(),
            ) {
                (Some(last), Some(revision), Some(instance)) => Some(
                    FactoryQueueCursor {
                        format: FACTORY_QUEUE_CURSOR_FORMAT.to_string(),
                        repository_id: self.repository_id.clone(),
                        state_id: self.state_id.clone(),
                        runtime_instance_id: instance,
                        revision,
                        after: last.task_id.clone(),
                    }
                    .encode()?,
                ),
                _ => None,
            }
        } else {
            None
        };

        let page = FactoryEligiblePage {
            entries,
            revision: obs.eligible_revision(),
            total: Some(ExactU64::from(total)),
            next_cursor,
        };
        self.finish(&obs, page)
    }
}

impl Observation {
    /// The opaque revision string pinned by a queue cursor. Includes the launch
    /// identity so a cursor minted by a previous process fails closed.
    fn eligible_revision(&self) -> Option<String> {
        self.runtime_instance_id
            .as_ref()
            .map(|instance| format!("{instance}/{}", self.revision))
    }

    /// Whether the queue source has a *successful* read (available or empty).
    fn queue_known(&self) -> bool {
        matches!(
            self.sources.get(QUEUE_SOURCE_INDEX).map(|s| s.state),
            Some(SourceState::Available | SourceState::Empty)
        )
    }
}

impl FactoryRuntimeSource {
    fn finish(
        &self,
        obs: &Observation,
        eligible: FactoryEligiblePage,
    ) -> Result<FactoryRuntime, ContractError> {
        let observed = ExactU64::from(now_ms());
        let mut sources = Vec::with_capacity(FactorySourceName::ALL.len());
        for (index, name) in FactorySourceName::ALL.iter().enumerate() {
            let record = obs.sources.get(index).cloned().unwrap_or_default();
            sources.push(FactorySourceRecord {
                source: *name,
                state: record.state,
                detail: record.detail,
                observed_at_unix_ms: record.observed_at_unix_ms.map(ExactU64::from),
            });
        }
        let record = FactoryRuntime {
            format: FACTORY_RUNTIME_FORMAT.to_string(),
            repository_id: self.repository_id.clone(),
            state_id: self.state_id.clone(),
            runtime_instance_id: obs.runtime_instance_id.clone(),
            observed_at_unix_ms: observed,
            status: obs.status,
            reason: obs.reason.clone(),
            sources,
            stop: obs.stop.clone(),
            current_execution: obs.current_execution.clone(),
            eligible,
            permissions: FactoryRuntimePermissions::default(),
        };
        record.validate()?;
        Ok(record)
    }
}

// ---------------------------------------------------------------------------
// Control seam (bounded wake hints)
// ---------------------------------------------------------------------------

/// A bounded runtime control handle.
///
/// `request_recheck` pushes a hint onto a capacity-[`WATCH_NOTIFICATION_CAPACITY`]
/// channel with latest-value coalescing; the durable stop decision is always
/// re-read from [`crate::stop`], so a coalesced hint never drops a stop decision.
pub struct RuntimeControl {
    shutdown: AtomicBool,
    hints: Mutex<VecDeque<()>>,
    wake: Condvar,
}

impl Default for RuntimeControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeControl {
    pub fn new() -> Self {
        RuntimeControl {
            shutdown: AtomicBool::new(false),
            hints: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
        }
    }

    /// Request a stop-policy recheck. Bounded: a full channel coalesces the hint
    /// (the oldest pending hint is dropped) rather than growing without bound.
    pub fn request_recheck(&self) {
        let mut hints = self
            .hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if hints.len() >= WATCH_NOTIFICATION_CAPACITY {
            hints.pop_front();
        }
        hints.push_back(());
        self.wake.notify_all();
    }

    /// Request graceful shutdown.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Wait up to `timeout` for a recheck hint or shutdown; returns early on
    /// either.
    pub fn wait(&self, timeout: Duration) {
        let mut hints = self
            .hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_shutdown() {
            return;
        }
        if hints.pop_front().is_some() {
            return;
        }
        let (mut hints, _) = self
            .wake
            .wait_timeout(hints, timeout)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hints.pop_front();
    }
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

/// Terminal outcome of one driven task (a public projection of [`super::Terminal`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Done,
    Parked(String),
    Blocked,
}

/// A sink for truthful current-execution observations (`WEB.10`). Batch mode
/// passes `None`; the continuous runtime wires a [`RuntimeShared`]-backed sink.
pub trait ExecutionObserver: Send + Sync {
    /// Observe the durable execution identity and its current phase/state.
    fn observe(
        &self,
        execution_id: &str,
        task_id: &str,
        generation: u32,
        phase: ExecutionPhase,
        state: ExecutionState,
    );
}

/// The injectable worker environment.
///
/// Production ([`QueueDriver`]) reuses [`super::run_task`] and the triage
/// selection; tests supply deterministic in-memory drivers. The driver owns all
/// mutable worker handles (queue/executor/admission) and is the only claimant.
pub trait RuntimeDriver {
    /// Run boot-only frozen recovery before any new claim.
    fn recover(&mut self) -> Result<(), ContractError>;
    /// The current three-way stop decision (never clears state).
    fn stop_decision(&self) -> StopDecision;
    /// Read the eligible ready list. `Err` is a queue failure, never an empty
    /// queue.
    fn read_ready(&mut self) -> Result<Vec<Task>, ContractError>;
    /// Claim one eligible task atomically.
    fn claim(&mut self, task: &Task) -> Result<ClaimEvidence, ContractError>;
    /// Execute one claimed task synchronously.
    fn run(&mut self, task: &Task, evidence: &ClaimEvidence) -> Result<RunOutcome, ContractError>;
    /// Shut the worker owner down (release execution/queue resources).
    fn shutdown(&mut self) -> Result<(), ContractError>;
    /// Select one task from the ready list. The default is deterministic
    /// (smallest `task_id`); production overrides with the pinned triage policy.
    fn select_task(&mut self, tasks: &[Task]) -> Result<Task, ContractError> {
        let mut sorted: Vec<&Task> = tasks.iter().collect();
        sorted.sort_by(|a, b| a.task_id.as_str().cmp(b.task_id.as_str()));
        sorted
            .first()
            .map(|task| (*task).clone())
            .ok_or_else(|| ContractError::KnownFailure("no eligible task to select".to_string()))
    }
    /// An observed current execution, if the driver tracks one.
    fn current_execution(&self) -> Option<FactoryExecutionObservation> {
        None
    }
    /// A durable admission hold (an unresolved review drift or a fail-closed
    /// hold check). While held, admission is paused and no task is claimed.
    fn admission_hold(&self) -> Option<String> {
        None
    }
}

/// The next action the loop should take after one [`ContinuousRuntime::tick`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopAction {
    /// A task was claimed and driven to a terminal boundary.
    Ran,
    /// The successful ready read was empty.
    Idle,
    /// Admission is stopped/unavailable/held; wait without new claims.
    Paused,
    /// A queue/source failure; retry after a bounded backoff.
    Retry,
    /// A fatal worker error; publish `Failed` and require explicit restart.
    Fatal,
}

/// The single-owner continuous runtime.
pub struct ContinuousRuntime<D: RuntimeDriver> {
    driver: D,
    shared: Arc<RuntimeShared>,
}

impl<D: RuntimeDriver> ContinuousRuntime<D> {
    pub fn new(driver: D, shared: Arc<RuntimeShared>) -> Self {
        ContinuousRuntime { driver, shared }
    }

    pub fn shared(&self) -> &Arc<RuntimeShared> {
        &self.shared
    }

    /// Drive exactly one scan: recheck stop/admission, read the ready set, claim
    /// and run one eligible task. Never panics on a source failure; the outcome
    /// is published on the shared cell.
    pub fn tick(&mut self) -> LoopAction {
        let decision = self.driver.stop_decision();
        self.shared.publish_stop(decision.clone());
        match decision {
            StopDecision::Running => {}
            StopDecision::Stopped(reason) => {
                self.shared.publish_paused(reason);
                return LoopAction::Paused;
            }
            StopDecision::Unavailable(reason) => {
                self.shared.publish_unavailable(reason);
                return LoopAction::Paused;
            }
        }
        // A durable admission hold (unresolved review drift or a fail-closed
        // hold check) blocks new claims without claiming an empty queue.
        if let Some(reason) = self.driver.admission_hold() {
            self.shared.publish_paused(reason);
            return LoopAction::Paused;
        }

        let ready = match self.driver.read_ready() {
            Ok(tasks) => tasks,
            Err(err) => {
                self.shared
                    .publish_unavailable(format!("queue read failed: {err}"));
                return LoopAction::Retry;
            }
        };
        if ready.is_empty() {
            self.shared.publish_idle(None);
            return LoopAction::Idle;
        }

        let mut entries: Vec<FactoryEligibleEntry> = ready.iter().map(entry_from_task).collect();
        entries.sort_by(|a, b| a.task_id.cmp(&b.task_id));

        let task = match self.driver.select_task(&ready) {
            Ok(task) => task,
            Err(err) => {
                eprintln!("agalma: work --watch task selection failed: {err}");
                self.shared
                    .publish_failed(format!("task selection failed: {err}"));
                return LoopAction::Fatal;
            }
        };

        // Recheck stop/admission immediately before claiming: a signal that
        // arrived during selection must deny this claim.
        let decision = self.driver.stop_decision();
        self.shared.publish_stop(decision.clone());
        if decision.is_denied() {
            let reason = decision.reason().unwrap_or("stop policy active").to_string();
            self.shared.publish_paused(reason);
            return LoopAction::Paused;
        }

        let evidence = match self.driver.claim(&task) {
            Ok(evidence) => evidence,
            Err(err) => {
                self.shared
                    .publish_unavailable(format!("claim failed: {err}"));
                return LoopAction::Retry;
            }
        };

        // The claimed task is no longer eligible; publish the remaining eligible
        // set (never the claimed identity). The driver's observer publishes the
        // actual durable execution/phase once it is allocated.
        entries.retain(|entry| entry.task_id != task.task_id.as_str());
        self.shared
            .publish_running(entries.clone(), self.driver.current_execution());
        match self.driver.run(&task, &evidence) {
            Ok(_outcome) => {
                // The claimed task reached its terminal/park boundary. Preserve
                // the known remaining eligible entries (or mark the queue
                // unknown when none remain); never fabricate a successful empty
                // queue from the pre-claim read. The loop rescans immediately.
                self.shared.publish_after_run(entries);
                LoopAction::Ran
            }
            Err(err) => {
                eprintln!("agalma: work --watch task execution failed: {err}");
                self.shared
                    .publish_failed(format!("task execution failed: {err}"));
                LoopAction::Fatal
            }
        }
    }

    /// Run boot recovery once **under admitted authority**, then loop until
    /// `control` requests shutdown.
    ///
    /// Boot-only phases (recovery/reap/reconcile/review and the epic sweep) run
    /// in [`RuntimeDriver::recover`]; the loop never repeats them per idle poll.
    /// A paused/unavailable startup serves observations without running the
    /// effectful boot, and a later resume runs the boot gate before the first
    /// claim.
    pub fn run_until(&mut self, control: &RuntimeControl) -> LoopAction {
        let instance = self
            .shared
            .runtime_instance_id()
            .unwrap_or_else(launch_identity);
        let mut boot_done = false;
        let mut backoff = WATCH_QUEUE_RETRY_INITIAL;
        loop {
            if control.is_shutdown() {
                break;
            }
            if !boot_done {
                // Effectful boot is admitted only while stop authority is
                // Running. While boot has not completed, a denied stop must
                // **not** be re-read through `tick()`: a resume that arrives
                // between this read and a second read could otherwise claim and
                // run a task before recovery. Publish the denied observation,
                // wait, and let the next Running read complete recovery before
                // any claim.
                let decision = self.driver.stop_decision();
                self.shared.publish_stop(decision.clone());
                if decision.is_denied() {
                    match decision {
                        StopDecision::Stopped(reason) => self.shared.publish_paused(reason),
                        StopDecision::Unavailable(reason) => {
                            self.shared.publish_unavailable(reason)
                        }
                        StopDecision::Running => unreachable!("denied above"),
                    }
                    control.wait(WATCH_IDLE_WAIT);
                    continue;
                }
                self.shared.publish_recovering(instance.clone());
                if let Err(err) = self.driver.recover() {
                    self.shared
                        .publish_failed(format!("boot recovery failed: {err}"));
                    let _ = self.driver.shutdown();
                    return LoopAction::Fatal;
                }
                boot_done = true;
            }
            match self.tick() {
                LoopAction::Ran => {
                    // A completed execution with possibly-queued rows rescans
                    // immediately; only a successful empty read waits.
                    backoff = WATCH_QUEUE_RETRY_INITIAL;
                }
                LoopAction::Idle | LoopAction::Paused => {
                    backoff = WATCH_QUEUE_RETRY_INITIAL;
                    control.wait(WATCH_IDLE_WAIT);
                }
                LoopAction::Retry => {
                    control.wait(backoff);
                    backoff = (backoff * 2).min(WATCH_QUEUE_RETRY_MAX);
                }
                LoopAction::Fatal => {
                    let _ = self.driver.shutdown();
                    return LoopAction::Fatal;
                }
            }
        }

        // Graceful shutdown: stop new admission first, then join the worker
        // owner. The caller joins the supervision owner and releases the lock.
        self.shared
            .publish_paused("shutdown: no new admission".to_string());
        let _ = self.driver.shutdown();
        LoopAction::Paused
    }
}

fn entry_from_task(task: &Task) -> FactoryEligibleEntry {
    FactoryEligibleEntry {
        task_id: task.task_id.as_str().to_string(),
        title: truncate_bytes(&task.title, FACTORY_TITLE_MAX_BYTES),
        priority: task.priority,
        family: task.family,
    }
}

// ---------------------------------------------------------------------------
// Startup classification (established-under-stop vs fresh no-init)
// ---------------------------------------------------------------------------

/// How continuous startup observed the state directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupMode {
    /// No stop is active; ordinary admission may proceed.
    Running,
    /// STOP/latch is active and established storage is readable and compatible;
    /// serve observations in `Paused` mode with no new claims.
    Paused(String),
    /// STOP/latch is active but storage is missing/unreadable/incompatible;
    /// serve `Unavailable` with no create/init/migrate and no dispatch.
    Unavailable(String),
}

/// Classify continuous startup without creating, initializing or migrating any
/// storage.
///
/// Only the entry-aware `STOP` observation and a read-only bind of *existing*
/// storage are used; established storage is never created here. A present
/// sentinel short-circuits before any ledger access. A latch-only stop (no
/// sentinel) is not visible to this read-only classifier: production re-reads it
/// through the bound control after opening admission storage, and a latch can
/// only exist on already-established storage.
pub fn classify_startup(state_dir: &Path) -> StartupMode {
    match crate::stop::observe_sentinel(state_dir) {
        StopDecision::Running => StartupMode::Running,
        StopDecision::Stopped(reason) => match established_storage_usable(state_dir) {
            Ok(()) => StartupMode::Paused(reason),
            Err(err) => StartupMode::Unavailable(err),
        },
        StopDecision::Unavailable(reason) => StartupMode::Unavailable(reason),
    }
}

/// Whether established storage can be opened read-only at the current schema,
/// without create/init/migrate.
fn established_storage_usable(state_dir: &Path) -> Result<(), String> {
    let ledger = LedgerHandle::from_transport_existing(
        state_dir,
        crate::ledger_process::LedgerTransport::from_env(),
    )
    .map_err(|err| {
        format!("no readable established storage while admission stopped: {err}")
    })?;
    match ledger.schema_version() {
        Ok(version) if version == agalma_execution::LEDGER_SCHEMA_VERSION => Ok(()),
        Ok(version) => Err(format!(
            "incompatible ledger schema {version} (expected {}) while admission stopped",
            agalma_execution::LEDGER_SCHEMA_VERSION
        )),
        Err(err) => Err(format!(
            "established ledger unreadable while admission stopped: {err}"
        )),
    }
}

// ---------------------------------------------------------------------------
// Supervision owner
// ---------------------------------------------------------------------------

/// A plugin host/probe supervision owner constructed and used in its own thread.
///
/// The trait deliberately does **not** require `Send`: the production owner holds
/// non-`Send` host objects and is constructed *inside* its supervision thread
/// ([`SupervisionHandle::spawn_host`]). Test owners that are moved into
/// [`SupervisionHandle::spawn`] must be `Send`.
pub trait SupervisionOwner {
    /// One supervision tick (host health/`supervise_tick`/hub serving).
    fn tick(&mut self) -> Result<(), ContractError>;
    /// Shut the owned group down and release owned resources.
    fn shutdown(&mut self) -> Result<(), ContractError>;
}

/// The registration context handed to a [`HostRegistrar`] once the production
/// plugin host and its capability router have been assembled inside the
/// supervision owner.
///
/// A later task (`WEB.13`) uses it to bind a factory adapter/control to the
/// **actual** router that serves the hub capability socket, using the selected
/// ledger and the runtime observation/control seam. It exposes only those
/// handles, never the WEB.10 runtime's private state.
pub struct HostRegistration<'a> {
    /// Opaque configured repository identity.
    pub repository_id: &'a str,
    /// Opaque configured state-directory identity.
    pub state_id: &'a str,
    /// The configured state directory the supervision owner was built over,
    /// borrowed unchanged from [`HostSupervision::state_dir`]. This is the exact
    /// path (already canonicalized by the watch entry point) used for the
    /// state-scoped factory-control owner guard and STOP sentinel; it is never
    /// derived from the opaque [`Self::state_id`].
    pub state_dir: &'a Path,
    /// The selected durable ledger the host was assembled over.
    pub ledger: &'a LedgerHandle,
    /// The exact router served on the hub capability socket.
    pub router: &'a Arc<crate::plugin_router::RouterService>,
    /// The runtime observation cell (immutable snapshots + host health).
    pub shared: &'a Arc<RuntimeShared>,
    /// The bounded runtime control seam (wake hints + shutdown).
    pub control: &'a Arc<RuntimeControl>,
}

/// A composition hook that registers a factory adapter/control on the assembled
/// production plugin host.
///
/// Invoked exactly once, inside the supervision owner's thread, after the hub
/// router exists and **before** any child is started, so a child that calls back
/// during startup already reaches the registered service.
pub trait HostRegistrar: Send + Sync {
    fn register(&self, registration: &HostRegistration<'_>) -> Result<(), ContractError>;
}

/// Owned registration inputs moved into the supervision thread.
///
/// [`SupervisionHandle::spawn_host`] consumes this and builds the borrowed
/// [`HostRegistration`] only after the host is assembled.
pub struct HostRegistrationContext {
    pub repository_id: String,
    pub state_id: String,
    pub shared: Arc<RuntimeShared>,
    pub control: Arc<RuntimeControl>,
    pub registrar: Arc<dyn HostRegistrar>,
}

/// A joinable native-thread supervision owner.
pub struct SupervisionHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<(), ContractError>>>,
}

/// Drive `owner` at `cadence` until `shutdown` is set, then shut it down. The
/// first tick/shutdown failure is retained and returned so a caller can surface
/// an owner failure instead of discarding it.
fn run_supervision<O: SupervisionOwner>(
    owner: &mut O,
    shutdown: &AtomicBool,
    cadence: Duration,
) -> Result<(), ContractError> {
    let mut failure: Option<ContractError> = None;
    while !shutdown.load(Ordering::SeqCst) {
        if let Err(err) = owner.tick() {
            eprintln!("agalma: supervision tick failed: {err}");
            if failure.is_none() {
                failure = Some(err);
            }
        }
        let deadline = Instant::now() + cadence;
        while Instant::now() < deadline {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
        }
    }
    if let Err(err) = owner.shutdown() {
        eprintln!("agalma: supervision shutdown failed: {err}");
        if failure.is_none() {
            failure = Some(err);
        }
    }
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

impl SupervisionHandle {
    fn from_thread(
        shutdown: Arc<AtomicBool>,
        thread: std::thread::JoinHandle<Result<(), ContractError>>,
    ) -> Self {
        SupervisionHandle {
            shutdown,
            thread: Some(thread),
        }
    }

    /// Spawn a `Send` owner on a dedicated native thread, ticking at `cadence`
    /// until shutdown is requested; the owner's `shutdown` runs before the thread
    /// ends. Used by tests with deterministic owners.
    pub fn spawn<O: SupervisionOwner + Send + 'static>(mut owner: O, cadence: Duration) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || run_supervision(&mut owner, &flag, cadence));
        SupervisionHandle::from_thread(shutdown, thread)
    }

    /// Spawn the production plugin-host supervision owner. The non-`Send` host is
    /// constructed **inside** the thread from the `Send` handles below, so no
    /// mutable host object migrates across threads.
    pub fn spawn_host(
        state_dir: PathBuf,
        roots: crate::cli::PluginRootArgs,
        ledger: LedgerHandle,
        shared: Arc<RuntimeShared>,
        registration: Option<HostRegistrationContext>,
        cadence: Duration,
    ) -> Self {
        Self::spawn_host_with_factory_control(
            state_dir,
            roots,
            ledger,
            shared,
            registration,
            cadence,
            None,
        )
    }

    /// Spawn the production plugin-host supervision owner with the optional
    /// explicit factory control-surface selector (`WEB.13`). `None` keeps the
    /// cockpit read-only.
    pub fn spawn_host_with_factory_control(
        state_dir: PathBuf,
        roots: crate::cli::PluginRootArgs,
        ledger: LedgerHandle,
        shared: Arc<RuntimeShared>,
        registration: Option<HostRegistrationContext>,
        cadence: Duration,
        factory_control_plugin: Option<String>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            let mut owner = HostSupervision::new(state_dir, roots, ledger, shared, registration);
            owner.factory_control_plugin = factory_control_plugin;
            run_supervision(&mut owner, &flag, cadence)
        });
        SupervisionHandle::from_thread(shutdown, thread)
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Join the owner thread, propagating a panic or the owner's first failure.
    pub fn join(&mut self) -> Result<(), ContractError> {
        if let Some(thread) = self.thread.take() {
            let outcome = thread
                .join()
                .map_err(|_| ContractError::KnownFailure("supervision owner panicked".to_string()))?;
            return outcome;
        }
        Ok(())
    }
}

impl Drop for SupervisionHandle {
    fn drop(&mut self) {
        self.request_shutdown();
        let _ = self.join();
    }
}

/// The production supervision owner: builds the plugin host **inside** its own
/// thread, serves the hub capability socket for that host **before** starting
/// any child, applies the optional [`HostRegistrar`] seam to the served router,
/// and keeps `supervise_tick` progressing.
pub struct HostSupervision {
    state_dir: PathBuf,
    roots: crate::cli::PluginRootArgs,
    ledger: LedgerHandle,
    shared: Arc<RuntimeShared>,
    host: Option<crate::composition::PluginHost>,
    probe: Option<crate::plugin_router::RpcHealthProbe>,
    runtime: Option<tokio::runtime::Runtime>,
    serve: Option<tokio::task::JoinHandle<()>>,
    registration: Option<HostRegistrationContext>,
    /// The optional explicit factory control-surface selector (`WEB.13`): the
    /// installed plugin id allowed to receive `factory.control/v1` from its own
    /// current durable approval pin. `None` keeps the cockpit read-only.
    factory_control_plugin: Option<String>,
    state: HostState,
    /// Set when a crash or unready desired plugin was observed; cleared only
    /// after a subsequent successful probe confirms readiness again, so a
    /// transient crash never publishes `Available`/`health=ok` prematurely.
    degraded: bool,
}

/// The supervision owner's assembly state. An assembly/start/server failure is
/// an explicit unavailable observation, never silently reported healthy.
#[derive(Clone, Debug)]
enum HostState {
    Unstarted,
    Started,
    Unavailable(String),
}

/// The host health a tick must publish for the observed crash/unready counts.
///
/// A crash tick or an unready desired plugin is never a healthy host: health is
/// [`SourceState::Available`] only when no child crashed **and** every desired
/// `Running` plugin is `Ready`. A probe/start/serve failure is
/// [`SourceState::Unreadable`] until a later successful probe clears it.
pub fn host_health_state(crashed: usize, unready: usize) -> SourceState {
    if crashed == 0 && unready == 0 {
        SourceState::Available
    } else {
        SourceState::Unreadable
    }
}

/// The plugin ids the host currently wants `Running` and that have an installed
/// generation (the set the supervisor is responsible for).
fn desired_running_plugins(host: &crate::composition::PluginHost) -> Vec<String> {
    host.registry
        .plugin_ids()
        .into_iter()
        .filter(|id| host.registry.desired(id) == DesiredState::Running)
        .filter(|id| {
            host.registry
                .entry(id)
                .is_some_and(|entry| {
                    entry.pending_generation.is_some() || entry.active_generation.is_some()
                })
        })
        .map(str::to_string)
        .collect()
}

/// The desired `Running` plugins whose observed lifecycle is not yet `Ready`.
fn unready_desired_plugins(
    host: &crate::composition::PluginHost,
) -> Vec<(String, LifecycleState)> {
    desired_running_plugins(host)
        .into_iter()
        .filter_map(|id| {
            let entry = host.registry.entry(&id)?;
            (entry.observed != LifecycleState::Ready).then_some((id, entry.observed))
        })
        .collect()
}

/// Render the unready desired plugins for a bounded health detail line.
fn format_unready(unready: &[(String, LifecycleState)]) -> String {
    unready
        .iter()
        .map(|(id, state)| format!("{id}={state:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

impl HostSupervision {
    pub fn new(
        state_dir: PathBuf,
        roots: crate::cli::PluginRootArgs,
        ledger: LedgerHandle,
        shared: Arc<RuntimeShared>,
        registration: Option<HostRegistrationContext>,
    ) -> Self {
        HostSupervision {
            state_dir,
            roots,
            ledger,
            shared,
            host: None,
            probe: None,
            runtime: None,
            serve: None,
            registration,
            factory_control_plugin: None,
            state: HostState::Unstarted,
            degraded: false,
        }
    }

    fn mark_unavailable(&mut self, detail: String) {
        self.shared
            .publish_host_health(SourceState::Unreadable, detail.clone());
        self.state = HostState::Unavailable(detail);
    }

    fn ensure_started(&mut self) -> Result<(), ContractError> {
        if !matches!(self.state, HostState::Unstarted) {
            return Ok(());
        }
        let Some(host) = crate::composition::assemble_work_plugin_host_with_control(
            &self.state_dir,
            &self.roots,
            &self.ledger,
            self.factory_control_plugin.as_deref(),
        ) else {
            // Assembly failure is an explicit unavailable observation, never a
            // fabricated healthy host. It never blocks factory work.
            self.mark_unavailable("plugin host: assembly failed".to_string());
            return Ok(());
        };
        // Serve and confirm the hub capability socket *before* any child is
        // launched or probed, so a child that binds back during startup always
        // reaches the hub.
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                let detail = format!("supervision runtime unavailable: {err}");
                self.mark_unavailable(detail.clone());
                return Err(ContractError::KnownFailure(detail));
            }
        };
        let hub_socket = host.hub_socket.clone();
        let router = host.router.clone();
        let serve = runtime.spawn(async move {
            if let Err(err) = crate::plugin_router::serve_hub(&hub_socket, router).await {
                eprintln!("agalma: hub capability socket stopped: {err}");
            }
        });
        if !wait_for_hub_socket(&host.hub_socket, WATCH_HUB_BIND_TIMEOUT) {
            serve.abort();
            let detail = format!(
                "hub capability socket {} was not bound before plugin startup",
                host.hub_socket.display()
            );
            self.mark_unavailable(detail.clone());
            return Err(ContractError::KnownFailure(detail));
        }
        // WEB.13 composition seam: bind a factory adapter/control to the actual
        // served router and the selected ledger *before* any child starts, so a
        // child that calls back during startup already reaches it.
        if let Some(registration) = self.registration.take() {
            let context = HostRegistration {
                repository_id: &registration.repository_id,
                state_id: &registration.state_id,
                state_dir: &self.state_dir,
                ledger: &self.ledger,
                router: &host.router,
                shared: &registration.shared,
                control: &registration.control,
            };
            if let Err(err) = registration.registrar.register(&context) {
                serve.abort();
                let detail = format!("plugin host registration failed: {err}");
                self.mark_unavailable(detail.clone());
                return Err(ContractError::KnownFailure(detail));
            }
        }
        let mut host = host;
        let mut probe = crate::plugin_router::RpcHealthProbe::new(self.state_dir.clone());
        // Retain the hub server/runtime/host *before* starting children so a
        // start failure can never leak a detached serve task; the failure is
        // surfaced and the host is marked unavailable.
        self.runtime = Some(runtime);
        self.serve = Some(serve);
        if let Err(err) = host.start_desired(&mut probe) {
            let detail = format!("plugin host start failed: {err:?}");
            self.probe = Some(probe);
            self.host = Some(host);
            self.mark_unavailable(detail.clone());
            return Err(ContractError::KnownFailure(detail));
        }
        // `start_desired` returns only the plugins that were health-confirmed
        // Ready. A desired `Running` plugin that did not become Ready is an
        // explicit start failure — never a healthy no-op that a later tick
        // silently reports as `Available`.
        let unready = unready_desired_plugins(&host);
        if !unready.is_empty() {
            let detail = format!(
                "plugin host start failed: {} desired plugin(s) not ready: {}",
                unready.len(),
                format_unready(&unready)
            );
            self.probe = Some(probe);
            self.host = Some(host);
            self.mark_unavailable(detail.clone());
            return Err(ContractError::KnownFailure(detail));
        }
        self.probe = Some(probe);
        self.host = Some(host);
        self.state = HostState::Started;
        Ok(())
    }
}

/// Wait (bounded) for the hub capability socket to accept connections before a
/// child is started.
fn wait_for_hub_socket(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if hub_socket_ready(path) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn hub_socket_ready(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn hub_socket_ready(path: &Path) -> bool {
    path.exists()
}

impl SupervisionOwner for HostSupervision {
    fn tick(&mut self) -> Result<(), ContractError> {
        self.ensure_started()?;
        // An assembly/start/serve failure is terminal for this owner: keep
        // reporting it explicitly (never overwrite it with a healthy no-op) and
        // wait for the owner to be restarted.
        if let HostState::Unavailable(reason) = &self.state {
            let reason = reason.clone();
            self.shared
                .publish_host_health(SourceState::Unreadable, reason);
            return Ok(());
        }
        // A stopped hub server is an owner failure, not a silent no-op.
        if self.serve.as_ref().is_some_and(|serve| serve.is_finished()) {
            self.mark_unavailable(
                "plugin host: hub capability socket stopped".to_string(),
            );
            return Err(ContractError::KnownFailure(
                "hub capability socket stopped".to_string(),
            ));
        }
        let (Some(host), Some(probe)) = (self.host.as_mut(), self.probe.as_mut()) else {
            match &self.state {
                HostState::Unavailable(reason) => {
                    let reason = reason.clone();
                    self.shared
                        .publish_host_health(SourceState::Unreadable, reason);
                }
                _ => self
                    .shared
                    .publish_host_health(SourceState::Available, "plugin host: no configured roots"),
            }
            return Ok(());
        };
        match host.supervise_tick(probe) {
            Ok(outcome) => {
                // A crash tick is a failure observation until the restart is
                // health-confirmed: never publish `Available`/`health=ok` while
                // a replacement is still pending.
                let mut unready = unready_desired_plugins(host);
                if !unready.is_empty() {
                    // A failed/absent probe keeps the host degraded; only a
                    // successful probe for every unready desired plugin clears
                    // the failure.
                    unready.retain(|(id, _)| {
                        !matches!(host.confirm_ready_health(id, probe), Ok(true))
                    });
                }
                if !outcome.crashed.is_empty() || !unready.is_empty() {
                    self.degraded = true;
                    let mut detail = format!("installed={}", host.installed.len());
                    if !outcome.crashed.is_empty() {
                        detail.push_str(&format!(" crashed={} restarting", outcome.crashed.len()));
                    }
                    if !unready.is_empty() {
                        detail.push_str(&format!(
                            " not_ready={} ({})",
                            unready.len(),
                            format_unready(&unready)
                        ));
                    }
                    debug_assert_eq!(
                        host_health_state(outcome.crashed.len(), unready.len()),
                        SourceState::Unreadable
                    );
                    self.shared
                        .publish_host_health(SourceState::Unreadable, detail);
                    return Ok(());
                }
                if self.degraded {
                    // Recovery is only claimed after a successful probe of every
                    // desired `Running` plugin.
                    let recovered = desired_running_plugins(host)
                        .iter()
                        .all(|id| matches!(host.confirm_ready_health(id, probe), Ok(true)));
                    if !recovered {
                        self.shared.publish_host_health(
                            SourceState::Unreadable,
                            format!(
                                "installed={} degraded: readiness probe not confirmed",
                                host.installed.len()
                            ),
                        );
                        return Ok(());
                    }
                    self.degraded = false;
                }
                debug_assert_eq!(
                    host_health_state(outcome.crashed.len(), unready.len()),
                    SourceState::Available
                );
                self.shared.publish_host_health(
                    SourceState::Available,
                    format!("installed={} health=ok", host.installed.len()),
                );
                Ok(())
            }
            Err(err) => {
                self.degraded = true;
                self.shared.publish_host_health(
                    SourceState::Unreadable,
                    format!("plugin host probe failure: {err:?}"),
                );
                Err(err)
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), ContractError> {
        // Stop and join the hub server owner before releasing the host.
        if let Some(serve) = self.serve.take() {
            serve.abort();
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(500));
        }
        if let Some(mut host) = self.host.take() {
            for (plugin_id, stop) in host.shutdown() {
                eprintln!(
                    "agalma: plugin `{plugin_id}` stopped: gone={} ({})",
                    stop.process_group_gone, stop.detail
                );
            }
        }
        self.probe = None;
        self.state = HostState::Unstarted;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cooperative watch stop control
// ---------------------------------------------------------------------------

/// Wraps the bound stop control so a shutdown request (for example a SIGTERM)
/// denies admission immediately and coerces in-flight work to its existing
/// cooperative phase boundary. It never mutates durable stop state itself.
struct WatchStopControl {
    bound: Rc<dyn StopControl>,
    control: Arc<RuntimeControl>,
}

impl WatchStopControl {
    fn new(bound: Rc<dyn StopControl>, control: Arc<RuntimeControl>) -> Self {
        WatchStopControl { bound, control }
    }
}

impl StopControl for WatchStopControl {
    fn stopped(&self) -> bool {
        self.control.is_shutdown() || self.bound.stopped()
    }

    fn decision(&self) -> StopDecision {
        if self.control.is_shutdown() {
            return StopDecision::Stopped("cooperative shutdown requested".to_string());
        }
        self.bound.decision()
    }
}

// ---------------------------------------------------------------------------
// Production driver and entry point
// ---------------------------------------------------------------------------

/// The production worker environment reusing the existing selection/claim/
/// `run_task` pipeline.
pub struct QueueDriver {
    repo: PathBuf,
    state_dir: PathBuf,
    ledger: LedgerHandle,
    queue: TaskQueueHandle,
    decider: StaticDecider<LedgerHandle>,
    harness: HarnessHandle,
    stop: Rc<dyn StopControl>,
    observer: Arc<dyn ExecutionObserver>,
    builder_selector: Option<String>,
    override_model: Option<String>,
    /// The provider proxy, created only once admission is Running (deferred from
    /// a Paused startup so effectful services are never started under a stop).
    proxy: Option<ProviderProxy>,
    paid_available: bool,
    proxy_url: Option<String>,
    services_ready: bool,
    /// Set when boot review found an unresolved epic drift or the hold check
    /// failed closed: ordinary dispatch is held (no claims) until a later boot.
    hold_reason: Option<String>,
}

impl QueueDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: PathBuf,
        state_dir: PathBuf,
        ledger: LedgerHandle,
        queue: TaskQueueHandle,
        decider: StaticDecider<LedgerHandle>,
        harness: HarnessHandle,
        stop: Rc<dyn StopControl>,
        shared: Arc<RuntimeShared>,
        builder_selector: Option<String>,
        override_model: Option<String>,
    ) -> Self {
        let observer: Arc<dyn ExecutionObserver> =
            Arc::new(RuntimeExecutionObserver::new(Arc::clone(&shared)));
        QueueDriver {
            repo,
            state_dir,
            ledger,
            queue,
            decider,
            harness,
            stop,
            observer,
            builder_selector,
            override_model,
            proxy: None,
            paid_available: false,
            proxy_url: None,
            services_ready: false,
            hold_reason: None,
        }
    }

    /// Create the effectful provider proxy once, only when admission is Running.
    /// A scripted (non-live) activity mode never starts a proxy. Idempotent.
    fn ensure_admitted_services(&mut self) -> Result<(), ContractError> {
        if self.services_ready {
            return Ok(());
        }
        if crate::phases::ActivityMode::from_env() == crate::phases::ActivityMode::Live {
            let proxy = ProviderProxy::start(ProxyConfig::from_env())
                .map_err(|e| ContractError::KnownFailure(format!("provider proxy: {e}")))?;
            let auth = proxy.auth_state();
            if let Err(err) = crate::provider_auth::record_audit(&self.state_dir, &auth) {
                eprintln!("agalma: provider_auth audit: {err}");
            } else if auth.present {
                eprintln!(
                    "agalma: paid provider credential present (host={}, fingerprint={})",
                    auth.host,
                    auth.fingerprint.as_deref().unwrap_or("-")
                );
            } else {
                eprintln!(
                    "agalma: paid provider credential absent ({}); paid routes disabled",
                    auth.reason.as_deref().unwrap_or("unknown")
                );
            }
            self.paid_available = auth.present;
            self.proxy_url = Some(proxy.url());
            self.proxy = Some(proxy);
        }
        self.services_ready = true;
        Ok(())
    }
}

impl RuntimeDriver for QueueDriver {
    fn recover(&mut self) -> Result<(), ContractError> {
        // Boot-only phases run once, before any new claim, in the established
        // order: frozen recovery, reap, projection reconcile, then the epic
        // sweep and review gates and the review hold. Reuses the existing
        // work-loop helpers.
        self.ensure_admitted_services()?;
        super::recover_inflight(
            &self.repo,
            &self.state_dir,
            &self.ledger,
            self.paid_available,
            self.proxy_url.clone(),
            &mut self.queue,
            self.harness.clone(),
            Rc::clone(&self.stop),
        )?;
        crate::reap::reap_terminal(&self.state_dir, &self.ledger);
        match crate::reconcile::reconcile_projection(&self.ledger, &mut self.queue) {
            Ok(summary) => eprintln!("agalma: {}", summary.render()),
            Err(err) => eprintln!("agalma: boot reconcile failed: {err}"),
        }
        match crate::sweep::run_sweep_with_stop(
            &self.repo,
            &self.state_dir,
            self.paid_available,
            self.proxy_url.clone(),
            self.override_model.as_deref(),
            Rc::clone(&self.stop),
        ) {
            Ok(report) if report.skipped_reason.is_none() => eprintln!(
                "agalma: epic sweep: {} candidate(s), {} approved, {} rejected, {} escalated, {} timed out",
                report.epics_seen.len(),
                report.approved.len(),
                report.rejected.len(),
                report.escalated.len(),
                report.timed_out.len()
            ),
            Ok(_) => {}
            Err(err) => eprintln!("agalma: epic sweep failed: {err}"),
        }
        match crate::review::run_review_with_stop(
            &self.repo,
            &self.state_dir,
            self.paid_available,
            self.proxy_url.clone(),
            self.override_model.as_deref(),
            Rc::clone(&self.stop),
        ) {
            Ok(report) if report.skipped_reason.is_none() => eprintln!(
                "agalma: review gate: {} epic(s) reviewed, {} approved, {} drift, {} escalated",
                report.epics_seen.len(),
                report.approved.len(),
                report.drift.len(),
                report.escalated.len()
            ),
            Ok(_) => {}
            Err(err) => eprintln!("agalma: review gate failed: {err}"),
        }
        // Hold ordinary dispatch while any epic's review drift is unresolved. A
        // hold-check **read error** fails closed: an unresolved gate cannot be
        // silently treated as "no hold".
        let hold = match crate::review::dispatch_hold(&agalma_taskqueue::BdTaskQueue::new(&self.repo))
        {
            Ok(Some(epic)) => Some(format!(
                "ordinary dispatch held: epic {epic} has unresolved review drift"
            )),
            Ok(None) => None,
            Err(err) => Some(format!(
                "review hold check failed (fail closed): {err}; holding dispatch"
            )),
        };
        if let Some(reason) = &hold {
            eprintln!("agalma: {reason}");
        }
        self.hold_reason = hold;
        Ok(())
    }

    fn stop_decision(&self) -> StopDecision {
        self.stop.decision()
    }

    fn admission_hold(&self) -> Option<String> {
        self.hold_reason.clone()
    }

    fn read_ready(&mut self) -> Result<Vec<Task>, ContractError> {
        Ok(self.queue.ready_tasks()?.tasks)
    }

    fn claim(&mut self, task: &Task) -> Result<ClaimEvidence, ContractError> {
        self.queue.claim(&task.task_id)
    }

    fn select_task(&mut self, tasks: &[Task]) -> Result<Task, ContractError> {
        if tasks.len() == 1 {
            return Ok(tasks[0].clone());
        }
        super::select_task(tasks, &mut self.decider)
    }

    fn run(&mut self, task: &Task, evidence: &ClaimEvidence) -> Result<RunOutcome, ContractError> {
        let terminal = super::run_task(
            &self.repo,
            &self.state_dir,
            &self.ledger,
            self.builder_selector.as_deref(),
            self.override_model.as_deref(),
            self.paid_available,
            self.proxy_url.clone(),
            task,
            evidence,
            &mut self.queue,
            self.harness.clone(),
            Rc::clone(&self.stop),
            Some(&*self.observer),
        )?;
        Ok(match terminal {
            super::Terminal::Done => RunOutcome::Done,
            super::Terminal::Parked(reason) => RunOutcome::Parked(reason),
            super::Terminal::Blocked => RunOutcome::Blocked,
        })
    }

    fn shutdown(&mut self) -> Result<(), ContractError> {
        Ok(())
    }
}

/// The [`RuntimeShared`]-backed execution observer used by [`QueueDriver`].
///
/// Public so the watchdog/observation seam can be exercised against the real
/// snapshot source without a paid provider or a full execution pipeline.
pub struct RuntimeExecutionObserver {
    shared: Arc<RuntimeShared>,
}

impl RuntimeExecutionObserver {
    pub fn new(shared: Arc<RuntimeShared>) -> Self {
        RuntimeExecutionObserver { shared }
    }
}

impl ExecutionObserver for RuntimeExecutionObserver {
    fn observe(
        &self,
        execution_id: &str,
        task_id: &str,
        generation: u32,
        phase: ExecutionPhase,
        state: ExecutionState,
    ) {
        self.shared
            .publish_execution(Some(FactoryExecutionObservation {
                execution_id: execution_id.to_string(),
                task_id: task_id.to_string(),
                generation: ExactU64::from(generation as u64),
                phase,
                state,
                updated_at_unix_ms: Some(ExactU64::from(now_ms())),
            }));
    }
}

/// Reject the explicitly-invalid `--watch` combinations. Returns the reason when
/// the invocation is invalid.
pub fn watch_conflict(args: &crate::cli::WorkArgs) -> Option<String> {
    if !args.watch {
        return None;
    }
    if args.once {
        return Some("--watch cannot be combined with --once".to_string());
    }
    if args.max_tasks.is_some() {
        return Some("--watch cannot be combined with --max-tasks".to_string());
    }
    None
}

/// The observation/control/registration handles a composition root registers for
/// the watch runtime (`WEB.13` handoff).
///
/// A caller can register the WEB.9 observation source (and the bounded control
/// seam) without editing the runtime internals: `run_watch_with` hands these to
/// its `on_compose` callback once the runtime identities are resolved and before
/// the worker loop begins. The callback may also install a [`HostRegistrar`],
/// which the supervision owner applies to the **actual** production plugin
/// router — without reaching into the WEB.10 runtime's private state.
pub struct WatchComposition {
    pub repository_id: String,
    pub state_id: String,
    pub shared: Arc<RuntimeShared>,
    pub control: Arc<RuntimeControl>,
    registrar: Mutex<Option<Arc<dyn HostRegistrar>>>,
}

impl WatchComposition {
    pub fn new(
        repository_id: String,
        state_id: String,
        shared: Arc<RuntimeShared>,
        control: Arc<RuntimeControl>,
    ) -> Self {
        WatchComposition {
            repository_id,
            state_id,
            shared,
            control,
            registrar: Mutex::new(None),
        }
    }

    /// The WEB.9 observation source over this runtime's shared cell.
    pub fn source(&self) -> FactoryRuntimeSource {
        FactoryRuntimeSource::new(
            Arc::clone(&self.shared),
            self.repository_id.clone(),
            self.state_id.clone(),
        )
    }

    /// Install the host registration hook (`WEB.13` seam).
    ///
    /// The hook receives the assembled host's served router plus the selected
    /// ledger and the runtime observation/control handles, and runs inside the
    /// supervision owner before any child starts. Replacing a previously
    /// installed hook is allowed; the latest one wins.
    pub fn register(&self, registrar: Arc<dyn HostRegistrar>) {
        *self
            .registrar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(registrar);
    }

    /// Take the installed registration hook, if any.
    pub fn take_registrar(&self) -> Option<Arc<dyn HostRegistrar>> {
        self.registrar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Run `agalma work --watch`: the continuous single-owner queue runtime.
///
/// The production watch path installs the **composition-owned** registration
/// hook ([`crate::composition::register_work_plugin_host`]) so a later
/// composition change (WEB.13, which owns `composition.rs` only) can bind the
/// factory adapter/control to the served plugin router without touching these
/// runtime internals.
pub fn run_watch(args: crate::cli::WorkArgs) -> Result<ExitCode, ContractError> {
    run_watch_with(args, crate::composition::register_work_plugin_host)
}

/// Run `agalma work --watch` and hand the observation/control handles to
/// `on_compose` before the worker loop starts (`WEB.13` composition seam).
pub fn run_watch_with(
    args: crate::cli::WorkArgs,
    on_compose: impl FnOnce(&WatchComposition),
) -> Result<ExitCode, ContractError> {
    if let Some(reason) = watch_conflict(&args) {
        return Err(ContractError::KnownFailure(reason));
    }

    let state_dir = crate::config::resolve_state_dir(args.state_dir.as_deref())
        .map_err(|e| ContractError::KnownFailure(e.to_string()))?;
    std::fs::create_dir_all(&state_dir).map_err(|e| {
        ContractError::KnownFailure(format!(
            "cannot create state dir {}: {e}",
            state_dir.display()
        ))
    })?;
    let state_dir = std::fs::canonicalize(&state_dir).unwrap_or(state_dir);
    let repo = std::fs::canonicalize(&args.repo)
        .map_err(|e| ContractError::KnownFailure(format!("repo {}: {e}", args.repo.display())))?;

    // One worker owns the state directory before services/claims.
    let _lock = LockGuard::acquire(&state_dir).map_err(ContractError::KnownFailure)?;

    let repository_id = opaque_repository_id(&repo);
    let state_id = opaque_state_id(&state_dir);
    let shared = Arc::new(RuntimeShared::new());

    // Startup observation: classify established-under-stop vs fresh, without
    // initializing anything.
    let startup = classify_startup(&state_dir);
    if let StartupMode::Unavailable(reason) = &startup {
        shared.publish_unavailable(reason.clone());
        eprintln!("agalma: work --watch unavailable: {reason}");
        return Ok(ExitCode::FAILURE);
    }

    // Open admission storage through the selected binding. A stopped state
    // already required readable established storage; a running fresh state is
    // explicitly initialized here (and records the establishment marker). A
    // stopped state never creates/initializes/migrates storage.
    let ledger = if matches!(startup, StartupMode::Paused(_)) {
        LedgerHandle::from_transport_existing(
            &state_dir,
            crate::ledger_process::LedgerTransport::from_env(),
        )?
    } else {
        crate::ledger_process::open_admission_ledger(
            &state_dir,
            crate::ledger_process::LedgerTransport::from_env(),
        )
        .map_err(ContractError::KnownFailure)?
    };
    {
        let version = ledger.schema_version()?;
        if version != agalma_execution::LEDGER_SCHEMA_VERSION {
            return Err(ContractError::KnownFailure(format!(
                "incompatible ledger schema {version} (expected {}) at {}",
                agalma_execution::LEDGER_SCHEMA_VERSION,
                state_dir.join("ledger.sqlite").display()
            )));
        }
    }

    // Cooperative shutdown seam: a signal denies new admission immediately
    // through the wrapped stop control, and in-flight work reaches its existing
    // cooperative phase boundary. The latch itself is never auto-cleared.
    let control = Arc::new(RuntimeControl::new());
    let bound: Rc<dyn StopControl> =
        Rc::new(crate::stop::BoundStopControl::new(&state_dir, ledger.clone()));
    let stop: Rc<dyn StopControl> = Rc::new(WatchStopControl::new(bound, Arc::clone(&control)));

    // WEB.13 composition seam: hand the observation/control handles to the
    // caller before the worker loop starts.
    let composition = WatchComposition::new(
        repository_id.clone(),
        state_id.clone(),
        Arc::clone(&shared),
        Arc::clone(&control),
    );
    on_compose(&composition);
    let registration = composition.take_registrar().map(|registrar| {
        HostRegistrationContext {
            repository_id: repository_id.clone(),
            state_id: state_id.clone(),
            shared: Arc::clone(&shared),
            control: Arc::clone(&control),
            registrar,
        }
    });

    // A latch-only stop (no sentinel) also pauses; a stop is never auto-cleared.
    let mode = match startup {
        StartupMode::Unavailable(reason) => StartupMode::Unavailable(reason),
        StartupMode::Paused(reason) => StartupMode::Paused(reason),
        StartupMode::Running => match stop.decision() {
            StopDecision::Running => StartupMode::Running,
            StopDecision::Stopped(reason) => StartupMode::Paused(reason),
            StopDecision::Unavailable(reason) => StartupMode::Unavailable(reason),
        },
    };
    if let StartupMode::Unavailable(reason) = &mode {
        shared.publish_unavailable(reason.clone());
        eprintln!("agalma: work --watch unavailable: {reason}");
        return Ok(ExitCode::FAILURE);
    }

    let instance = launch_identity();
    shared.initialize(
        Some(instance.clone()),
        vec![
            (SourceState::Available, "runtime started".to_string()),
            (SourceState::Available, String::new()),
            (SourceState::Unknown, String::new()),
            (SourceState::Unknown, String::new()),
            (SourceState::Available, String::new()),
        ],
    );
    shared.publish_starting(instance.clone());

    // The supervision owner is constructed and used in its own native thread so
    // host health/hub serving keeps progressing while `run_task` blocks the
    // worker. Allowed under a stop: observation services only.
    let mut supervision = SupervisionHandle::spawn_host_with_factory_control(
        state_dir.clone(),
        args.plugin_roots.clone(),
        ledger.clone(),
        Arc::clone(&shared),
        registration,
        WATCH_SUPERVISION_TICK,
        args.factory_control_plugin.clone(),
    );
    eprintln!(
        "agalma: work --watch repository_id={repository_id} state_id={state_id} instance={instance}"
    );

    // Signal handling: deny new admission through the control seam and drain
    // gracefully (join owners, release the lock); never force-unlock or exit.
    install_watch_signal_handler(state_dir.clone(), Arc::clone(&control));

    // Paused-established start: serve in Paused without new claims; effectful
    // services and recovery are deferred until admission resumes.
    if let StartupMode::Paused(reason) = &mode {
        shared.publish_paused(reason.clone());
        eprintln!("agalma: work --watch paused: {reason}");
    }

    // Worker environment: queue/decider/harness only. The provider proxy and the
    // boot-only recovery/planning phases are created inside `recover`, which the
    // runtime invokes only under admitted authority. A Paused startup therefore
    // starts no proxy and runs no recovery until resume.
    let queue = TaskQueueHandle::from_transport(
        &state_dir,
        &repo,
        crate::taskqueue_process::TaskQueueTransport::from_env(),
    )?;
    let decider = StaticDecider::new(ledger.clone());
    let harness = crate::harness_binding::harness_handle(
        crate::harness_binding::resolve_selection(args.harness.as_deref())?,
    );
    let override_model = crate::routing::explicit_model(args.model.as_deref());
    let builder_selector = crate::config::resolve_builder_model(args.builder_model.as_deref());

    let driver = QueueDriver::new(
        repo.clone(),
        state_dir.clone(),
        ledger.clone(),
        queue,
        decider,
        harness,
        Rc::clone(&stop),
        Arc::clone(&shared),
        builder_selector,
        override_model,
    );
    let mut runtime = ContinuousRuntime::new(driver, Arc::clone(&shared));
    let outcome = runtime.run_until(&control);

    // Graceful shutdown order: the worker already stopped new admission and
    // reached a terminal/park boundary; join the supervision owner, then release
    // the lock. No fake terminal receipt; the lock is never force-broken.
    supervision.request_shutdown();
    let supervision_result = supervision.join();
    drop(runtime);
    drop(_lock);

    if let Err(err) = supervision_result {
        eprintln!("agalma: work --watch supervision failed: {err}");
        return Ok(ExitCode::FAILURE);
    }
    if outcome == LoopAction::Fatal {
        eprintln!("agalma: work --watch failed fatally; explicit restart required");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Install the watch-specific signal handler: deny new admission through the
/// control seam so no claim races the signal and in-flight work reaches its
/// cooperative phase boundary, then drain gracefully instead of hard-exiting.
fn install_watch_signal_handler(state_dir: PathBuf, control: Arc<RuntimeControl>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("agalma: cannot install SIGINT handler: {err}");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("agalma: cannot install SIGTERM handler: {err}");
                return;
            }
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        if let Some(reason) = crate::stop::observe_sentinel(&state_dir).reason() {
            eprintln!("agalma: signal received (stop already active: {reason}); draining");
        } else {
            eprintln!("agalma: signal received; denying new admission and draining");
        }
        control.request_shutdown();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_identities_are_non_path_components() {
        assert_eq!(opaque_repository_id(Path::new("/a/b/origin")), "origin");
        assert_eq!(opaque_state_id(Path::new("/a/b/state")), "state");
        assert_eq!(opaque_repository_id(Path::new("")), "agalma");
    }

    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        assert_eq!(truncate_bytes("abc", 2), "ab");
        assert_eq!(truncate_bytes("éé", 3), "é");
        assert_eq!(truncate_bytes("abc", 5), "abc");
    }

    #[test]
    fn control_channel_is_bounded_and_coalesces() {
        let control = RuntimeControl::new();
        for _ in 0..100 {
            control.request_recheck();
        }
        let hints = control.hints.lock().unwrap();
        assert!(hints.len() <= WATCH_NOTIFICATION_CAPACITY);
    }

    #[test]
    fn watch_conflict_rejects_once_and_max_tasks() {
        let mut args = crate::cli::WorkArgs {
            repo: PathBuf::from("."),
            once: false,
            max_tasks: None,
            state_dir: None,
            model: None,
            builder_model: None,
            harness: None,
            plugin_roots: crate::cli::PluginRootArgs::default(),
            watch: false,
            factory_control_plugin: None,
        };
        assert!(watch_conflict(&args).is_none());
        args.watch = true;
        assert!(watch_conflict(&args).is_none());
        args.once = true;
        assert!(watch_conflict(&args).is_some());
        args.once = false;
        args.max_tasks = Some(3);
        assert!(watch_conflict(&args).is_some());
    }

    #[test]
    fn host_health_does_not_bump_the_queue_revision() {
        let shared = RuntimeShared::new();
        shared.initialize(Some("inst".to_string()), Vec::new());
        let before = shared.revision();
        shared.publish_host_health(SourceState::Available, "installed=0 health=ok");
        assert_eq!(shared.revision(), before, "host heartbeat is not a queue change");
    }

    #[test]
    fn transient_unavailable_preserves_the_runtime_instance() {
        let shared = RuntimeShared::new();
        shared.initialize(Some("epoch-1".to_string()), Vec::new());
        shared.publish_unavailable("queue read failed".to_string());
        assert_eq!(
            shared.runtime_instance_id().as_deref(),
            Some("epoch-1"),
            "a transient source failure preserves the process epoch"
        );
    }

    #[test]
    fn watch_stop_control_denies_admission_on_shutdown() {
        struct Running;
        impl StopControl for Running {
            fn stopped(&self) -> bool {
                false
            }
            fn decision(&self) -> StopDecision {
                StopDecision::Running
            }
        }
        let control = Arc::new(RuntimeControl::new());
        let stop = WatchStopControl::new(Rc::new(Running), Arc::clone(&control));
        assert_eq!(stop.decision(), StopDecision::Running);
        control.request_shutdown();
        assert!(stop.decision().is_denied(), "a signal denies new admission");
    }
}
