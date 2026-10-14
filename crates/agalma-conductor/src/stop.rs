//! Read-only stop admission shared by the automatic epic sweep, the concierge
//! intake, the review gate, the shared session seam, and the ordinary work loop
//! (`agalma-r2l`, `agalma-5fy.5`).
//!
//! The factory must never *launch paid planning sessions*, *spawn a confined
//! session*, or *edit its backlog* while stopped. A stop is requested when
//! **either**:
//!
//! - the `<state>/STOP` sentinel file exists, or
//! - the durable ledger kill latch is set.
//!
//! ## Running / Stopped / Unavailable
//!
//! [`StopDecision`] makes the three outcomes explicit; only [`Running`] permits
//! effect admission (`agalma-5fy.5`):
//!
//! - [`StopDecision::Running`] — no sentinel, the latch is readable and clear;
//! - [`StopDecision::Stopped`] — sentinel present or the latch is set;
//! - [`StopDecision::Unavailable`] — the established stop state cannot be
//!   read/bound (unreadable file, incompatible schema, bind/read error).
//!   **Uncertainty is fail-closed**: the callers suppress dispatch exactly as for
//!   [`Stopped`], recording the distinct reason so the suppression is visible.
//!
//! ## Bound reads (accepted policy)
//!
//! Production admission reads the durable latch through the **selected** ledger
//! binding ([`crate::ledger_process::LedgerTransport`]) via the bound
//! [`LedgerApi`] ([`BoundStopControl`]) — never by opening a second/private
//! database implementation. [`bind_stop_control`] binds through
//! [`crate::ledger_process::open_admission_ledger`], which distinguishes
//! **explicit fresh initialization** (no marker, no database → create/initialize
//! and record a durable establishment marker) from **established open** (marker
//! present → open without create/init/migrate; a missing database is lost
//! established state and refuses). A partial/ambiguous initialization refuses. A
//! failed bind/read observation is [`StopDecision::Unavailable`] and denies
//! admission — it can never be misread as a clear latch, and there is **no**
//! private-database fallback.
//!
//! The `STOP` sentinel is observed through an explicit metadata check: only a
//! `NotFound` observation is "no sentinel"; an IO/permission error is
//! [`StopDecision::Unavailable`], never false. A present sentinel short-circuits
//! before any ledger access (so a stop never initializes a database).
//!
//! [`FileStopControl`] is retained as a **legacy/inspection** control for the
//! pre-existing executor poll callers (`crate::phases`) that already hold it; it
//! is **not** used for production planning/review/concierge/session admission,
//! which all route through [`bind_stop_control`].
//!
//! A stop is **read-only**: nothing here (or in the sweep / review / session /
//! intake) ever clears the sentinel or the latch. Clearing is exclusively the
//! human `agalma resume` flow, which clears both, so a persisted stop stays
//! effective until a human explicitly resumes.
//!
//! [`sweep_epics`](crate::sweep::sweep_epics) and
//! [`run_session`](crate::session::run_session) take a `&dyn StopControl`, so
//! tests drive the policy without a harness; [`BoundStopControl`] is the bound
//! implementation, [`bind_stop_control`] the selected-transport production one,
//! and [`FileStopControl`] the legacy path-based one.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use agalma_contracts::LedgerApi;

/// Reason recorded when a stop (sentinel or kill latch) suppresses dispatch.
pub const STOPPED_REASON: &str = "stop policy active (STOP sentinel or kill latch set)";
/// Reason recorded when the durable stop state cannot be read. Fail-closed:
/// dispatch is suppressed just like a [`StopDecision::Stopped`].
pub const UNAVAILABLE_REASON: &str =
    "stop state unavailable (ledger present but unreadable or incompatible); dispatch suppressed";

/// Observe the `STOP` sentinel path explicitly: `Ok(true)` present, `Ok(false)`
/// absent (`NotFound`), `Err` for any other metadata error (uncertainty).
///
/// Uses `symlink_metadata` so a *present* entry that does not resolve (a
/// dangling symlink) still counts as present and denies; only a genuine
/// `NotFound` is treated as absent.
fn sentinel_observation(stop_path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(stop_path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("{}: {err}", stop_path.display())),
    }
}

/// Whether dispatch of work is permitted right now.
///
/// Only [`StopDecision::Running`] permits effect admission; [`Stopped`] and
/// [`Unavailable`] deny it. Implementations MUST NOT mutate the `STOP` sentinel
/// or the kill latch: only the human `agalma resume` flow clears a stop.
///
/// [`Stopped`]: StopDecision::Stopped
/// [`Unavailable`]: StopDecision::Unavailable
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopDecision {
    /// No sentinel; the latch is readable and clear. Dispatch is permitted.
    Running,
    /// Sentinel present or the latch is set. Dispatch is suppressed.
    Stopped(String),
    /// The established ledger cannot be bound/read (unreadable / incompatible /
    /// read error). Fail-closed: dispatch is suppressed with a visible reason.
    Unavailable(String),
}

impl StopDecision {
    /// Whether effect admission (dispatch, session/proxy creation, queue
    /// mutation) is permitted. Only [`StopDecision::Running`] returns `true`.
    pub fn permits_admission(&self) -> bool {
        matches!(self, StopDecision::Running)
    }

    /// Whether dispatch is denied (either [`StopDecision::Stopped`] or
    /// [`StopDecision::Unavailable`]).
    pub fn is_denied(&self) -> bool {
        !self.permits_admission()
    }

    /// The visible denial reason, or `None` when running.
    pub fn reason(&self) -> Option<&str> {
        match self {
            StopDecision::Running => None,
            StopDecision::Stopped(reason) | StopDecision::Unavailable(reason) => Some(reason),
        }
    }
}

/// Whether dispatch of work must be suppressed right now.
///
/// Implementations MUST NOT mutate the `STOP` sentinel or the kill latch: only
/// the human `agalma resume` flow clears a stop.
pub trait StopControl {
    /// Whether a stop is currently requested. Never clears state. Fail-closed:
    /// an unreadable established ledger is reported as stopped.
    fn stopped(&self) -> bool;

    /// The full three-way admission decision. The default derives it from
    /// [`StopControl::stopped`]; the production controls override it to
    /// distinguish an unreadable established ledger ([`StopDecision::Unavailable`])
    /// from an explicit stop ([`StopDecision::Stopped`]).
    fn decision(&self) -> StopDecision {
        if self.stopped() {
            StopDecision::Stopped(STOPPED_REASON.to_string())
        } else {
            StopDecision::Running
        }
    }
}

/// Production bound stop control: reads the kill latch through an injected
/// [`LedgerApi`] (`agalma-5fy.5`), never opening SQLite directly.
///
/// An incompatible schema or any read error is [`StopDecision::Unavailable`]
/// (fail-closed); an explicit latch/sentinel is [`StopDecision::Stopped`].
pub struct BoundStopControl<L: LedgerApi> {
    stop_path: PathBuf,
    ledger: L,
}

impl<L: LedgerApi> BoundStopControl<L> {
    /// Build the control around an already-bound ledger handle.
    pub fn new(state_dir: &Path, ledger: L) -> Self {
        BoundStopControl {
            stop_path: state_dir.join("STOP"),
            ledger,
        }
    }

    /// Path of the `STOP` sentinel (diagnostics/tests).
    pub fn stop_path(&self) -> &Path {
        &self.stop_path
    }

    /// Whether the `STOP` sentinel alone is present (ignores the kill latch).
    pub fn sentinel_present(&self) -> bool {
        self.stop_path.exists()
    }

    /// The three-way decision, reading the bound ledger.
    pub fn evaluate(&self) -> StopDecision {
        match sentinel_observation(&self.stop_path) {
            Ok(true) => return StopDecision::Stopped(STOPPED_REASON.to_string()),
            Ok(false) => {}
            Err(err) => {
                return StopDecision::Unavailable(format!(
                    "{UNAVAILABLE_REASON}: sentinel observation: {err}"
                ))
            }
        }
        match self.ledger.schema_version() {
            Ok(version) if version == agalma_execution::LEDGER_SCHEMA_VERSION => {}
            Ok(version) => {
                return StopDecision::Unavailable(format!(
                    "{UNAVAILABLE_REASON}: incompatible schema {version} (expected {})",
                    agalma_execution::LEDGER_SCHEMA_VERSION
                ))
            }
            Err(err) => return StopDecision::Unavailable(format!("{UNAVAILABLE_REASON}: {err}")),
        }
        match self.ledger.kill_latch() {
            Ok(true) => StopDecision::Stopped(STOPPED_REASON.to_string()),
            Ok(false) => StopDecision::Running,
            Err(err) => StopDecision::Unavailable(format!("{UNAVAILABLE_REASON}: {err}")),
        }
    }
}

impl<L: LedgerApi> StopControl for BoundStopControl<L> {
    fn stopped(&self) -> bool {
        self.evaluate().is_denied()
    }

    fn decision(&self) -> StopDecision {
        self.evaluate()
    }
}

/// Fail-closed control used when the selected ledger binding cannot even be
/// constructed: a `STOP` sentinel still reports [`StopDecision::Stopped`];
/// otherwise the fixed bind error is [`StopDecision::Unavailable`]. There is no
/// fallback to a private database.
pub struct UnavailableStopControl {
    stop_path: PathBuf,
    reason: String,
}

impl UnavailableStopControl {
    /// Build the control with a visible bind-failure reason.
    pub fn new(state_dir: &Path, reason: impl Into<String>) -> Self {
        UnavailableStopControl {
            stop_path: state_dir.join("STOP"),
            reason: reason.into(),
        }
    }

    /// Whether the `STOP` sentinel alone is present.
    pub fn sentinel_present(&self) -> bool {
        self.stop_path.exists()
    }
}

impl StopControl for UnavailableStopControl {
    fn stopped(&self) -> bool {
        true
    }

    fn decision(&self) -> StopDecision {
        match sentinel_observation(&self.stop_path) {
            Ok(true) => StopDecision::Stopped(STOPPED_REASON.to_string()),
            Ok(false) => StopDecision::Unavailable(self.reason.clone()),
            Err(err) => StopDecision::Unavailable(format!(
                "{UNAVAILABLE_REASON}: sentinel observation: {err}"
            )),
        }
    }
}

/// Legacy path-based stop control (`<state>/STOP` sentinel or the durable ledger
/// kill latch), retained for callers that already hold it.
///
/// **Not used for production admission** (`agalma-5fy.5`): it opens SQLite
/// directly and cannot observe the selected ledger service. Production callers
/// use [`bind_stop_control`]. An **absent** ledger is a fresh state and is
/// [`Running`]; an **established** ledger that cannot be read (unreadable file,
/// incompatible schema, read error) is fail-closed. A metadata error other than
/// "not found" is also fail-closed rather than mistaken for fresh.
pub struct FileStopControl {
    stop_path: PathBuf,
    ledger_path: PathBuf,
}

/// The state of the legacy ledger file: absent (fresh), established (read it),
/// or an inaccessible/errored path (fail closed).
enum LegacyLedger {
    Absent,
    Established,
    Inaccessible(String),
}

impl FileStopControl {
    /// Build the control for a conductor state dir.
    pub fn new(state_dir: &Path) -> Self {
        FileStopControl {
            stop_path: state_dir.join("STOP"),
            ledger_path: state_dir.join("ledger.sqlite"),
        }
    }

    /// Path of the `STOP` sentinel (diagnostics/tests).
    pub fn stop_path(&self) -> &Path {
        &self.stop_path
    }

    /// Whether the `STOP` sentinel alone is present (ignores the kill latch).
    pub fn sentinel_present(&self) -> bool {
        self.stop_path.exists()
    }

    /// Classify the ledger path without treating a metadata error as fresh, and
    /// without treating a present-but-unresolved entry (dangling symlink) as
    /// absent.
    fn classify(&self) -> LegacyLedger {
        match std::fs::symlink_metadata(&self.ledger_path) {
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::metadata(&self.ledger_path) {
                Ok(m) if m.is_file() => LegacyLedger::Established,
                Ok(_) => LegacyLedger::Inaccessible("ledger path is not a file".to_string()),
                Err(err) => {
                    LegacyLedger::Inaccessible(format!("unresolved ledger entry: {err}"))
                }
            },
            Ok(meta) if meta.is_file() => LegacyLedger::Established,
            Ok(_) => LegacyLedger::Inaccessible("ledger path is not a file".to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => LegacyLedger::Absent,
            Err(err) => LegacyLedger::Inaccessible(err.to_string()),
        }
    }

    /// Read the established legacy ledger's latch through a direct open.
    fn read_ledger(&self) -> StopDecision {
        match agalma_ledger::SqliteLedger::open(&self.ledger_path) {
            Ok(ledger) => {
                match ledger.schema_version() {
                    Ok(version) if version == agalma_execution::LEDGER_SCHEMA_VERSION => {}
                    Ok(version) => {
                        return StopDecision::Unavailable(format!(
                            "{UNAVAILABLE_REASON}: incompatible schema {version} (expected {})",
                            agalma_execution::LEDGER_SCHEMA_VERSION
                        ))
                    }
                    Err(err) => {
                        return StopDecision::Unavailable(format!("{UNAVAILABLE_REASON}: {err}"))
                    }
                }
                match ledger.kill_latch() {
                    Ok(true) => StopDecision::Stopped(STOPPED_REASON.to_string()),
                    Ok(false) => StopDecision::Running,
                    Err(err) => {
                        StopDecision::Unavailable(format!("{UNAVAILABLE_REASON}: {err}"))
                    }
                }
            }
            Err(err) => StopDecision::Unavailable(format!("{UNAVAILABLE_REASON}: {err}")),
        }
    }
}

impl StopControl for FileStopControl {
    fn stopped(&self) -> bool {
        match sentinel_observation(&self.stop_path) {
            Ok(true) => return true,
            // A metadata error on the sentinel is uncertainty: fail closed.
            Err(_) => return true,
            Ok(false) => {}
        }
        match self.classify() {
            LegacyLedger::Absent => false,
            LegacyLedger::Established => self.read_ledger().is_denied(),
            // A metadata error (permission, IO) is error-not-fresh: fail closed.
            LegacyLedger::Inaccessible(_) => true,
        }
    }

    fn decision(&self) -> StopDecision {
        match sentinel_observation(&self.stop_path) {
            Ok(true) => return StopDecision::Stopped(STOPPED_REASON.to_string()),
            Ok(false) => {}
            Err(err) => {
                return StopDecision::Unavailable(format!(
                    "{UNAVAILABLE_REASON}: sentinel observation: {err}"
                ))
            }
        }
        match self.classify() {
            LegacyLedger::Absent => StopDecision::Running,
            LegacyLedger::Established => self.read_ledger(),
            LegacyLedger::Inaccessible(err) => {
                StopDecision::Unavailable(format!("{UNAVAILABLE_REASON}: {err}"))
            }
        }
    }
}

/// Sentinel-only control returned when the `STOP` file already exists: a stop is
/// requested without touching (or initializing) the ledger.
struct SentinelStopControl;

impl StopControl for SentinelStopControl {
    fn stopped(&self) -> bool {
        true
    }

    fn decision(&self) -> StopDecision {
        StopDecision::Stopped(STOPPED_REASON.to_string())
    }
}

/// Bind the production stop control for `state_dir` through the **selected**
/// ledger transport ([`crate::ledger_process::LedgerTransport`]).
///
/// A present `STOP` sentinel short-circuits (no ledger access, no
/// initialization); a sentinel metadata error is fail-closed. Otherwise the
/// selected binding distinguishes explicit fresh initialization from established
/// open via [`crate::ledger_process::open_admission_ledger`]. A bind/read failure
/// is [`UnavailableStopControl`] (no private database fallback). Returns a shared
/// handle so the same control can be passed through the session seam.
pub fn bind_stop_control(state_dir: &Path) -> Rc<dyn StopControl> {
    bind_stop_control_with(
        state_dir,
        crate::ledger_process::LedgerTransport::from_env(),
    )
}

/// Entry-aware `STOP` observation at a command/admission boundary, without
/// touching the ledger (`agalma-5fy.5`).
///
/// A **present** entry (including a dangling symlink) is [`StopDecision::Stopped`];
/// only a genuine `NotFound` is [`StopDecision::Running`]; any other metadata
/// error is [`StopDecision::Unavailable`]. Production callers use this before
/// [`crate::ledger_process::open_admission_ledger`] so a stop never initializes a
/// database.
pub fn observe_sentinel(state_dir: &Path) -> StopDecision {
    match sentinel_observation(&state_dir.join("STOP")) {
        Ok(true) => StopDecision::Stopped(STOPPED_REASON.to_string()),
        Ok(false) => StopDecision::Running,
        Err(err) => StopDecision::Unavailable(format!(
            "{UNAVAILABLE_REASON}: sentinel observation: {err}"
        )),
    }
}

/// [`bind_stop_control`] with an explicit selected transport (used by the
/// process/in-process policy tests).
pub fn bind_stop_control_with(
    state_dir: &Path,
    transport: crate::ledger_process::LedgerTransport,
) -> Rc<dyn StopControl> {
    // A present sentinel needs no ledger read; do not initialize one. A sentinel
    // metadata error is uncertainty and fails closed.
    match sentinel_observation(&state_dir.join("STOP")) {
        Ok(true) => return Rc::new(SentinelStopControl),
        Ok(false) => {}
        Err(err) => {
            return Rc::new(UnavailableStopControl::new(
                state_dir,
                format!("{UNAVAILABLE_REASON}: sentinel observation: {err}"),
            ))
        }
    }
    match crate::ledger_process::open_admission_ledger(state_dir, transport) {
        Ok(handle) => Rc::new(BoundStopControl::new(state_dir, handle)),
        Err(err) => Rc::new(UnavailableStopControl::new(
            state_dir,
            format!("{UNAVAILABLE_REASON}: {err}"),
        )),
    }
}

/// The production stop decision for a command boundary, read through the
/// selected ledger binding.
pub fn decision_for_state_dir(state_dir: &Path) -> StopDecision {
    bind_stop_control(state_dir).decision()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_testkit::ScratchDir;

    fn scratch(name: &str) -> ScratchDir {
        ScratchDir::new(&format!("stop-{name}"))
    }

    /// Legacy control: sentinel alone requests stop without creating a ledger.
    #[test]
    fn sentinel_alone_requests_stop_without_touching_the_ledger() {
        let dir = scratch("sentinel");
        let control = FileStopControl::new(&dir);
        assert_eq!(control.decision(), StopDecision::Running, "no stop by default");
        std::fs::write(control.stop_path(), b"stop").expect("write STOP");
        assert!(control.stopped(), "sentinel requests stop");
        assert!(matches!(control.decision(), StopDecision::Stopped(_)));
        assert!(!dir.join("ledger.sqlite").exists(), "no ledger created");
        assert!(control.sentinel_present(), "sentinel retained");
    }

    #[test]
    fn kill_latch_alone_requests_stop() {
        let dir = scratch("latch");
        let mut ledger =
            agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger.set_kill_latch(true).expect("set latch");
        drop(ledger);

        let control = FileStopControl::new(&dir);
        assert!(control.stopped(), "kill latch requests stop");
        assert!(matches!(control.decision(), StopDecision::Stopped(_)));
        assert!(!control.sentinel_present(), "no STOP file present");

        assert!(control.stopped(), "latch retained across reads");
        let reopened =
            agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite")).expect("reopen");
        assert!(reopened.kill_latch().expect("read latch"), "latch still set");
    }

    #[test]
    fn cleared_latch_and_absent_sentinel_is_not_stopped() {
        let dir = scratch("clear");
        let mut ledger =
            agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger.set_kill_latch(false).expect("clear latch");
        drop(ledger);
        assert!(!FileStopControl::new(&dir).stopped(), "clear → dispatch allowed");
    }

    /// Legacy control: a fresh absent ledger is `Running` and is not created.
    #[test]
    fn legacy_absent_ledger_is_running_and_not_created() {
        let dir = scratch("legacy-absent");
        assert!(!dir.join("ledger.sqlite").exists());
        let control = FileStopControl::new(&dir);
        assert_eq!(control.decision(), StopDecision::Running);
        assert!(!dir.join("ledger.sqlite").exists(), "read-only check never creates");
    }

    /// Legacy control: an established ledger that cannot be read is fail-closed.
    #[test]
    fn legacy_unreadable_ledger_is_unavailable_not_running() {
        let dir = scratch("corrupt");
        std::fs::write(dir.join("ledger.sqlite"), b"not a sqlite database")
            .expect("write corrupt ledger");
        let control = FileStopControl::new(&dir);
        assert!(control.stopped(), "corrupt established ledger is fail-closed");
        match control.decision() {
            StopDecision::Unavailable(reason) => assert!(reason.contains("unavailable"), "{reason}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// Legacy control: a metadata error (here: the ledger path is a directory)
    /// is fail-closed, never mistaken for fresh.
    #[test]
    fn legacy_metadata_error_is_unavailable_not_fresh() {
        let dir = scratch("metadata-error");
        std::fs::create_dir_all(dir.join("ledger.sqlite")).expect("dir at ledger path");
        let control = FileStopControl::new(&dir);
        // A readable directory passes metadata() as "established"; the direct
        // open then fails, so the decision is Unavailable.
        assert!(control.stopped(), "unreadable established path is fail-closed");
        assert!(matches!(control.decision(), StopDecision::Unavailable(_)));
    }

    /// Legacy control: an incompatible schema is `Unavailable`.
    #[test]
    fn legacy_incompatible_schema_is_unavailable() {
        let dir = scratch("incompatible");
        let path = dir.join("ledger.sqlite");
        drop(agalma_ledger::SqliteLedger::open(&path).expect("open ledger"));
        {
            let conn = rusqlite::Connection::open(&path).expect("reopen raw");
            conn.execute("UPDATE meta SET value='999' WHERE key='schema_version'", [])
                .expect("force schema version");
        }
        let control = FileStopControl::new(&dir);
        assert!(control.stopped(), "incompatible schema is fail-closed");
        assert!(matches!(control.decision(), StopDecision::Unavailable(_)));
    }

    /// The bound control reads the latch through the trait and reports a schema
    /// mismatch as `Unavailable`; an explicit stop still wins.
    #[test]
    fn bound_control_reads_running_stopped_and_unavailable() {
        let dir = scratch("bound");

        let mut handle =
            crate::ledger_process::LedgerHandle::in_process(&dir).expect("bind ledger");
        {
            let stop = BoundStopControl::new(&dir, handle.clone());
            assert_eq!(stop.decision(), StopDecision::Running);
        }
        handle.set_kill_latch(true).expect("set latch");
        let stop = BoundStopControl::new(&dir, handle.clone());
        assert!(matches!(stop.decision(), StopDecision::Stopped(_)));
        drop(stop);
        handle.set_kill_latch(false).expect("clear latch");
        drop(handle);

        std::fs::write(dir.join("ledger.sqlite"), b"garbage").expect("corrupt");
        let handle = crate::ledger_process::LedgerHandle::in_process(&dir);
        // A corrupt file cannot be bound → the production control is the
        // fail-closed UnavailableStopControl (no private fallback).
        assert!(handle.is_err(), "corrupt file cannot bind");
        match UnavailableStopControl::new(&dir, "bind failed").decision() {
            StopDecision::Unavailable(_) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
