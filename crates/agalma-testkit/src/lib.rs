//! Agalma dev-only test scratch helper.
//!
//! [`ScratchDir`] owns a unique directory under `<workspace>/target/test-runs`
//! that is removed on success (its [`Drop`]) and kept on panic (forensics). It
//! never uses a system temp directory (repo rule: scratch lives under
//! `target/test-runs`, never `/tmp`).
//!
//! ```ignore
//! let dir = ScratchDir::new("case");
//! let state = dir.child("state");
//! // `dir` (and everything under it) is removed when the test returns;
//! // a panicking test keeps it for inspection.
//! ```
//!
//! Callers that need the directory to survive a successful test call
//! [`ScratchDir::keep`] or [`ScratchDir::into_path`].

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long [`retry_connect`] keeps retrying before giving up: the same budget
/// the per-test file-existence gates used (500 × 10 ms).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Retry a fallible connect attempt until it succeeds or [`CONNECT_TIMEOUT`]
/// elapses, returning the last error on timeout.
///
/// A server spawned on a background thread can leave its socket file visible
/// (created by `bind`) a beat before the listener is ready to accept, and a
/// failed `listen` under fd pressure can even leave a stale file behind. A
/// plain file-existence poll therefore races; retrying the real connect closes
/// the window deterministically without sleeping longer than necessary.
pub fn retry_connect<T, E, F>(mut attempt: F) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
{
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(err);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// A process-local sequence, appended to the name so two `ScratchDir`s created
/// in the same nanosecond (parallel tests) can never collide on one path.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A unique, self-cleaning scratch directory.
///
/// Derefs to [`Path`] (and implements [`AsRef<Path>`]) so it can stand in for a
/// `Path`/`&Path` at existing call sites.
pub struct ScratchDir {
    path: PathBuf,
    armed: bool,
}

impl ScratchDir {
    /// `<workspace>/target/test-runs/<name>-<pid>-<nanos>-<seq>`; creates it
    /// fresh. The trailing sequence guarantees uniqueness across parallel tests
    /// in one process (a bare nanosecond stamp can repeat).
    pub fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = workspace_root()
            .join("target")
            .join("test-runs")
            .join(format!("{name}-{}-{nanos}-{seq}", std::process::id()));
        // Best-effort reset so the directory is always created fresh.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("create scratch dir {}: {e}", path.display()));
        ScratchDir { path, armed: true }
    }

    /// The scratch directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// mkdir + return a subpath (still covered by the guard).
    pub fn child(&self, name: &str) -> PathBuf {
        let child = self.path.join(name);
        std::fs::create_dir_all(&child)
            .unwrap_or_else(|e| panic!("create scratch child {}: {e}", child.display()));
        child
    }

    /// Disarm cleanup (dir stays on disk).
    pub fn keep(&mut self) {
        self.armed = false;
    }

    /// Disarm and return the path.
    pub fn into_path(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }
}

impl Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.armed && !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The workspace root: `crates/agalma-testkit` is two levels below it.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn removes_on_success() {
        let path;
        {
            let dir = ScratchDir::new("unit-success");
            path = dir.path().to_path_buf();
            assert!(path.is_dir(), "created fresh");
        }
        assert!(!path.exists(), "removed when the guard drops on success");
    }

    #[test]
    fn keeps_on_panic() {
        let captured: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let writer = Arc::clone(&captured);
        let _ = std::thread::spawn(move || {
            let dir = ScratchDir::new("unit-panic");
            *writer.lock().expect("capture") = Some(dir.path().to_path_buf());
            panic!("intentional: exercise the keep-on-panic path");
        })
        .join();
        let path = captured
            .lock()
            .expect("capture")
            .clone()
            .expect("path captured before the panic");
        assert!(path.is_dir(), "kept when the guard drops during a panic");
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn keep_and_into_path_disarm_cleanup() {
        let mut dir = ScratchDir::new("unit-keep");
        let path = dir.path().to_path_buf();
        dir.keep();
        drop(dir);
        assert!(path.is_dir(), "keep() leaves the dir on disk");
        let _ = std::fs::remove_dir_all(&path);

        let path = ScratchDir::new("unit-into").into_path();
        assert!(path.is_dir(), "into_path() returns the (kept) dir");
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn retry_connect_retries_until_ready() {
        let attempts = std::cell::Cell::new(0u32);
        let value = retry_connect(|| {
            let n = attempts.get() + 1;
            attempts.set(n);
            if n < 3 {
                Err("not ready")
            } else {
                Ok(n)
            }
        });
        assert_eq!(value, Ok(3), "returns the first successful attempt");
    }

    #[test]
    fn retry_connect_returns_success_without_waiting() {
        let value: Result<u8, &str> = retry_connect(|| Ok(7));
        assert_eq!(value, Ok(7));
    }

    #[test]
    fn child_deref_and_asref() {
        let dir = ScratchDir::new("unit-child");
        let child = dir.child("sub");
        assert!(child.is_dir(), "child() mkdirs the subpath");

        // Deref to Path.
        let deref: &Path = &dir;
        assert_eq!(deref, dir.path());
        assert!(dir.join("sub").is_dir());

        // AsRef<Path> (both owned and borrowed).
        fn is_dir(p: impl AsRef<Path>) -> bool {
            p.as_ref().is_dir()
        }
        assert!(is_dir(&dir));
        assert!(is_dir(&child));
    }
}
