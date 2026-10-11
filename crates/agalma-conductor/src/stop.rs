//! Read-only stop policy shared by the automatic epic sweep, the concierge
//! intake, and the ordinary work loop (`agalma-r2l`).
//!
//! The factory must never *launch paid planning sessions* or *edit its
//! backlog* while stopped. A stop is requested when **either**:
//!
//! - the `<state>/STOP` sentinel file exists, or
//! - the durable ledger kill latch is set.
//!
//! A stop is **read-only**: nothing here (or in the sweep / session / intake)
//! ever clears the sentinel or the latch. Clearing is exclusively the human
//! `agalma resume` flow, which clears both, so a persisted stop stays effective
//! until a human explicitly resumes.
//!
//! [`sweep_epics`](crate::sweep::sweep_epics) and
//! [`run_session`](crate::session::run_session) take a `&dyn StopControl`, so
//! tests drive the policy without a harness; [`FileStopControl`] is the
//! production implementation.

use std::path::{Path, PathBuf};

use agalma_contracts::LedgerApi;

/// Whether dispatch of work must be suppressed right now.
///
/// Implementations MUST NOT mutate the `STOP` sentinel or the kill latch: only
/// the human `agalma resume` flow clears a stop.
pub trait StopControl {
    /// Whether a stop is currently requested. Never clears state.
    fn stopped(&self) -> bool;
}

/// Production stop control: the `<state>/STOP` sentinel **or** the durable
/// ledger kill latch. Never clears either.
pub struct FileStopControl {
    stop_path: PathBuf,
    ledger_path: PathBuf,
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
}

impl StopControl for FileStopControl {
    fn stopped(&self) -> bool {
        if self.stop_path.exists() {
            return true;
        }
        // No ledger yet → no latch to read (and never create one as a side
        // effect of a read-only check). A read error (locked/incompatible) is
        // treated as "not latched": the `STOP` sentinel is the primary control,
        // and the executor independently enforces the latch on dispatch.
        if !self.ledger_path.exists() {
            return false;
        }
        agalma_ledger::SqliteLedger::open(&self.ledger_path)
            .ok()
            .and_then(|ledger| ledger.kill_latch().ok())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_testkit::ScratchDir;

    fn scratch(name: &str) -> ScratchDir {
        ScratchDir::new(&format!("stop-{name}"))
    }

    #[test]
    fn sentinel_alone_requests_stop_without_touching_the_ledger() {
        let dir = scratch("sentinel");
        let control = FileStopControl::new(&dir);
        assert!(!control.stopped(), "no stop by default");
        std::fs::write(control.stop_path(), b"stop").expect("write STOP");
        assert!(control.stopped(), "sentinel requests stop");
        // The read-only check must not create a ledger as a side effect.
        assert!(!dir.join("ledger.sqlite").exists(), "no ledger created");
        // The sentinel is never cleared implicitly.
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
        assert!(!control.sentinel_present(), "no STOP file present");

        // Re-reading does not clear the latch.
        assert!(control.stopped(), "latch retained across reads");
        let reopened =
            agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite")).expect("reopen");
        assert!(
            reopened.kill_latch().expect("read latch"),
            "latch still set"
        );
    }

    #[test]
    fn cleared_latch_and_absent_sentinel_is_not_stopped() {
        let dir = scratch("clear");
        let mut ledger =
            agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger.set_kill_latch(false).expect("clear latch");
        drop(ledger);
        assert!(
            !FileStopControl::new(&dir).stopped(),
            "clear → dispatch allowed"
        );
    }
}
