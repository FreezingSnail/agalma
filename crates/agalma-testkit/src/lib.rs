//! Agalma dev-only test scratch helper.
//!
//! [`ScratchDir`] owns a unique directory that is removed on success (its
//! [`Drop`]) and kept on panic (forensics). By default it lives under
//! `<workspace>/target/test-runs`; it never uses a system temp directory
//! (repo rule: scratch lives under `target/test-runs`, never `/tmp`).
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
//!
//! # Run-root and retention configuration
//!
//! Two environment variables adjust the default behavior (both unset preserve
//! the original behavior exactly):
//!
//! - `AGALMA_TEST_RUN_ROOT` — an **absolute** directory used as the allocation
//!   root instead of `<workspace>/target/test-runs`. Retained fixtures and long
//!   socket paths can be redirected to a persistent per-bead run root. A
//!   relative, empty, or `..`-containing root is rejected with a clear
//!   [`ConfigError`].
//! - `AGALMA_TEST_RETAIN=1` — keep every successfully created scratch directory
//!   (evidence retention) instead of deleting it on success. `0` or unset keeps
//!   the default cleanup; any other value is rejected.
//!
//! The configured root is used **verbatim** and never canonicalized, so a short
//! repository-local symlink can be supplied as the root while the physical bytes
//! land in a longer persistent run root. Paths returned by [`ScratchDir`] keep
//! that short alias prefix, which is what keeps Unix socket paths inside the
//! platform `SUN_LEN` limit.
//!
//! A scratch `name` must be a single normal path component (validated by
//! `validate_name`).
//! Names such as `..`, `a/b`, `/abs`, or the empty string are refused before any
//! allocation, so [`ScratchDir::new_in`] can never silently escape the
//! configured root.
//!
//! Allocation is exclusive: [`ScratchDir::new`] never deletes a pre-existing
//! directory, and a bounded number of attempts is made before giving up.
//!
//! The resolution logic lives in the pure [`ScratchConfig::resolve`] layer and
//! is exercised directly, so tests never mutate the process-global environment.

use std::ffi::OsStr;
use std::fmt;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Environment variable naming an optional absolute allocation root.
pub const RUN_ROOT_ENV: &str = "AGALMA_TEST_RUN_ROOT";

/// Environment variable enabling retention of successful scratch directories.
pub const RETAIN_ENV: &str = "AGALMA_TEST_RETAIN";

/// How long [`retry_connect`] keeps retrying before giving up: the same budget
/// the per-test file-existence gates used (500 × 10 ms).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on exclusive allocation attempts before a scratch directory is
/// considered impossible to create. A collision only happens against a stale
/// directory from a previous run, so a handful of attempts is plenty; the bound
/// prevents an unbounded retry loop.
const ALLOC_ATTEMPTS: u32 = 64;

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

/// A rejected run-root / retention / name configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `AGALMA_TEST_RUN_ROOT` was set to an empty string.
    EmptyRoot,
    /// `AGALMA_TEST_RUN_ROOT` was not an absolute path.
    RelativeRoot(std::ffi::OsString),
    /// `AGALMA_TEST_RUN_ROOT` contained a `..` component.
    MalformedRoot(PathBuf),
    /// `AGALMA_TEST_RUN_ROOT` exists but is not a directory.
    RootNotDir(PathBuf),
    /// `AGALMA_TEST_RETAIN` was set to something other than `0` or `1`.
    InvalidRetain(std::ffi::OsString),
    /// A scratch name was not a single normal path component.
    InvalidName(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::EmptyRoot => write!(
                f,
                "{RUN_ROOT_ENV} is set but empty; expected an absolute run root"
            ),
            ConfigError::RelativeRoot(raw) => write!(
                f,
                "{RUN_ROOT_ENV}={raw:?} is relative; expected an absolute path"
            ),
            ConfigError::MalformedRoot(path) => write!(
                f,
                "{RUN_ROOT_ENV}={} contains '..'; expected a fixed absolute root",
                path.display()
            ),
            ConfigError::RootNotDir(path) => write!(
                f,
                "{RUN_ROOT_ENV}={} exists but is not a directory",
                path.display()
            ),
            ConfigError::InvalidRetain(raw) => write!(
                f,
                "{RETAIN_ENV}={raw:?} is invalid; use 1 to retain, 0 or unset to clean up"
            ),
            ConfigError::InvalidName(name) => write!(
                f,
                "scratch name {name:?} is not a single path component; use a plain name without '/', '.', or '..'"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Resolved scratch allocation policy: where directories are created and whether
/// successful ones are retained.
///
/// Construct it from the environment with [`ScratchConfig::from_env`], or from
/// explicit inputs with [`ScratchConfig::resolve`] / [`ScratchConfig::new`]. The
/// explicit constructors are pure and let tests exercise root selection and the
/// retention policy without touching the process-global environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchConfig {
    root: PathBuf,
    retain: bool,
}

impl ScratchConfig {
    /// Validate an explicit root and retention choice.
    pub fn new(root: impl Into<PathBuf>, retain: bool) -> Result<Self, ConfigError> {
        let root = validate_root(root.into().as_os_str())?;
        Ok(ScratchConfig { root, retain })
    }

    /// Pure resolution: `root`/`retain` are the optional environment values and
    /// `default_root` is used when no root override is supplied. Never reads or
    /// writes the process environment.
    pub fn resolve(
        root: Option<&OsStr>,
        retain: Option<&OsStr>,
        default_root: PathBuf,
    ) -> Result<Self, ConfigError> {
        let root = match root {
            Some(raw) => validate_root(raw)?,
            None => validate_root(default_root.as_os_str())?,
        };
        let retain = parse_retain(retain)?;
        Ok(ScratchConfig { root, retain })
    }

    /// Read the two environment variables and resolve them against the default
    /// root. The only function here that touches the environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let root = std::env::var_os(RUN_ROOT_ENV);
        let retain = std::env::var_os(RETAIN_ENV);
        Self::resolve(root.as_deref(), retain.as_deref(), Self::default_root())
    }

    /// The configured allocation root, preserved verbatim (a symlink alias is
    /// not canonicalized away).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether successful scratch directories are retained.
    pub fn retain(&self) -> bool {
        self.retain
    }

    /// The default allocation root: `<workspace>/target/test-runs`.
    pub fn default_root() -> PathBuf {
        workspace_root().join("target").join("test-runs")
    }
}

/// Reject a root that is empty, relative, contains `..`, or is an existing
/// non-directory. An existing directory (or symlink to one) is accepted and
/// returned unchanged so a short alias survives into the returned paths.
fn validate_root(raw: &OsStr) -> Result<PathBuf, ConfigError> {
    if raw.is_empty() {
        return Err(ConfigError::EmptyRoot);
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(ConfigError::RelativeRoot(raw.to_os_string()));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ConfigError::MalformedRoot(path.to_path_buf()));
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if !meta.is_dir() {
            return Err(ConfigError::RootNotDir(path.to_path_buf()));
        }
    }
    Ok(path.to_path_buf())
}

/// Reject a scratch name that is not a single normal path component.
///
/// A validated name joined onto the configured root always yields a direct
/// child of that root, so an allocation can never escape it. Rejected: the empty
/// string, `.`, `..`, any platform path separator (including a trailing one),
/// the NUL byte, and anything `Path::components` cannot reduce to one `Normal`
/// component.
///
/// The raw separator/NUL/`..` checks are explicit and run *before* the parser
/// because `Path::components` normalizes away trailing separators and `.`/`..`
/// components — `a/` and `a/.` would otherwise reduce to the single `Normal`
/// component `a` and be accepted, then formatted into a nested path.
fn validate_name(name: &str) -> Result<(), ConfigError> {
    let invalid = || ConfigError::InvalidName(name.to_string());
    if name.is_empty() || name == "." || name == ".." {
        return Err(invalid());
    }
    if name.chars().any(std::path::is_separator) || name.contains('\0') {
        return Err(invalid());
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(invalid()),
    }
}

/// Parse `AGALMA_TEST_RETAIN`: unset/`0` => false, `1` => true, else rejected.
fn parse_retain(raw: Option<&OsStr>) -> Result<bool, ConfigError> {
    match raw {
        None => Ok(false),
        Some(value) => match value.to_str() {
            Some("1") => Ok(true),
            Some("0") => Ok(false),
            _ => Err(ConfigError::InvalidRetain(value.to_os_string())),
        },
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
    retain: bool,
}

impl ScratchDir {
    /// Allocate a fresh directory using the environment configuration.
    ///
    /// Panics with a clear message if `AGALMA_TEST_RUN_ROOT` /
    /// `AGALMA_TEST_RETAIN` are set but malformed, or if `name` is not a single
    /// path component; unset variables keep the default root and cleanup
    /// behavior.
    pub fn new(name: &str) -> Self {
        let config = ScratchConfig::from_env()
            .unwrap_or_else(|err| panic!("invalid scratch configuration: {err}"));
        Self::new_in(&config, name)
    }

    /// Allocate a fresh directory under an explicit, already-resolved config.
    ///
    /// The `name` is validated as a single normal component first, so this can
    /// never escape `config.root()`. Creates `config.root()` if missing, then
    /// creates a unique child **exclusively** (never deleting existing evidence)
    /// and retries a bounded number of times on collision.
    pub fn new_in(config: &ScratchConfig, name: &str) -> Self {
        validate_name(name).unwrap_or_else(|err| panic!("invalid scratch name: {err}"));
        let path = allocate(config.root(), name);
        ScratchDir {
            path,
            armed: true,
            retain: config.retain(),
        }
    }

    /// The scratch directory path. Preserves any configured symlink alias.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this directory will be retained on a successful drop.
    pub fn retained(&self) -> bool {
        self.retain
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

/// Create a directory under `root`, trying each candidate name in turn and
/// stopping at the first name that does not already exist. Existing entries are
/// never deleted, and the number of attempts is bounded by [`ALLOC_ATTEMPTS`].
fn create_exclusive<I>(root: &Path, candidates: I) -> PathBuf
where
    I: IntoIterator<Item = String>,
{
    std::fs::create_dir_all(root)
        .unwrap_or_else(|e| panic!("create scratch root {}: {e}", root.display()));
    let mut attempts = 0u32;
    for name in candidates {
        if attempts >= ALLOC_ATTEMPTS {
            break;
        }
        attempts += 1;
        let candidate = root.join(&name);
        debug_assert_eq!(
            candidate.parent(),
            Some(root),
            "candidate {candidate:?} escaped root {}",
            root.display()
        );
        match std::fs::create_dir(&candidate) {
            Ok(()) => return candidate,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => panic!("create scratch dir {}: {err}", candidate.display()),
        }
    }
    panic!(
        "scratch dir allocation exhausted {ALLOC_ATTEMPTS} attempts under {}",
        root.display()
    );
}

/// Allocate a unique scratch directory under `root`. Panics if `name` is not a
/// single normal component (defense in depth; [`ScratchDir::new_in`] validates
/// before calling).
fn allocate(root: &Path, name: &str) -> PathBuf {
    validate_name(name).unwrap_or_else(|err| panic!("invalid scratch name: {err}"));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    create_exclusive(
        root,
        std::iter::repeat_with(move || {
            let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            format!("{name}-{pid}-{nanos}-{seq}")
        }),
    )
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
        if self.armed && !self.retain && !std::thread::panicking() {
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
    use std::collections::HashSet;
    use std::ffi::OsStr;
    use std::sync::{Arc, Mutex};

    // ----------------------------------------------------------------------
    // Deterministic generator. Replay scheme:
    //   generator: SplitMix64 (state += 0x9E3779B97F4A7C15, then the standard
    //              splitmix64 mix), modulo reduced for range selection.
    //   seeds (fixed, documented):
    //     exclusive-skip  0xA6A14D5800000002  (128 cases)
    //     uniqueness      0xA6A14D5800000003  (128 cases)
    //     confinement     0xA6A14D5800000004  (128 cases)
    //     relative-root   0xA6A14D5800000005  (128 cases)
    //     names           0xA6A14D5800000006  (128 cases)
    //   Replay with: cargo test --offline --locked -p agalma-testkit --lib tests::
    //
    // Generated property cases allocate under the *configured* run root
    // (AGALMA_TEST_RUN_ROOT when set, else the default) and intentionally leave
    // their evidence in place; cleanup is only asserted for cases that opt into
    // non-retention, and those keep an input snapshot in a retained parent.
    // ----------------------------------------------------------------------

    /// Deterministic replay note written into retained fixture evidence.
    const REPLAY_NOTE: &str = "\
agalma-mxk agalma-testkit root/retention acceptance
generator: SplitMix64 (state += 0x9E3779B97F4A7C15, then standard mix)
seeds:
  exclusive-skip  0xA6A14D5800000002 (128 cases)
  uniqueness      0xA6A14D5800000003 (128 cases)
  confinement     0xA6A14D5800000004 (128 cases)
  relative-root   0xA6A14D5800000005 (128 cases)
  names           0xA6A14D5800000006 (128 cases)
replay: cargo test --offline --locked -p agalma-testkit --lib tests::
";

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
    }

    /// The configured persistent per-bead run root, or the default when unset.
    /// Read-only: no test mutates the process environment.
    fn configured_root() -> PathBuf {
        std::env::var_os(RUN_ROOT_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(ScratchConfig::default_root)
    }

    /// An explicit retention config rooted at the configured run root.
    fn scratch_config(retain: bool) -> ScratchConfig {
        ScratchConfig::new(configured_root(), retain).expect("configured run root is valid")
    }

    /// A retained scratch parent under the configured run root. Evidence placed
    /// beneath it survives the intentionally cleaned-up child cases.
    fn retained_parent(tag: &str) -> ScratchDir {
        ScratchDir::new_in(&scratch_config(true), tag)
    }

    /// Shortest reliable repository-local base for the short alias symlink.
    ///
    /// In a named worktree (`<repo>/target/worktrees/<name>`) the shared cargo
    /// target root is two levels up, so `<repo>/target/test-runs` is shorter
    /// than the worktree-local default root; otherwise fall back to the
    /// configured root.
    fn short_alias_base() -> PathBuf {
        let worktree = workspace_root();
        let is_worktree = worktree
            .parent()
            .and_then(Path::file_name)
            .map(|n| n == "worktrees")
            .unwrap_or(false);
        if is_worktree {
            if let Some(shared) = worktree.parent().and_then(Path::parent) {
                return shared.join("test-runs");
            }
        }
        configured_root()
    }

    /// Allocate a short alias symlink exclusively: never delete an existing
    /// alias or evidence, retry a bounded number of candidate names.
    fn allocate_alias(base: &Path, target: &Path, pid: u32) -> PathBuf {
        use std::os::unix::fs::symlink;
        for attempt in 0..64u32 {
            let candidate = base.join(format!("atk{pid}-{attempt}"));
            match symlink(target, &candidate) {
                Ok(()) => return candidate,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => panic!("create alias {}: {err}", candidate.display()),
            }
        }
        panic!("alias allocation exhausted under {}", base.display());
    }

    fn now_nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    // ----- existing lifecycle coverage -----

    #[test]
    fn removes_on_success() {
        // Explicit cleanup policy inside a retained run parent: the child is
        // removed on success while the parent and its replay snapshot stay as
        // evidence. Uses an explicit config, so it is independent of
        // AGALMA_TEST_RETAIN.
        let parent = retained_parent("unit-success");
        let snapshots = parent.child("snapshots");
        std::fs::write(snapshots.join("removes_on_success.txt"), REPLAY_NOTE)
            .expect("snapshot replay info");
        let config =
            ScratchConfig::new(parent.path().to_path_buf(), false).expect("valid parent root");
        let path;
        {
            let dir = ScratchDir::new_in(&config, "unit-success");
            path = dir.path().to_path_buf();
            assert!(path.is_dir(), "created fresh");
        }
        assert!(
            !path.exists(),
            "removed when the non-retained guard drops on success"
        );
        assert!(parent.path().is_dir(), "retained parent preserved");
        assert!(
            snapshots.join("removes_on_success.txt").is_file(),
            "replay snapshot retained"
        );
    }

    #[test]
    fn keeps_on_panic() {
        let parent = retained_parent("unit-panic");
        let config =
            ScratchConfig::new(parent.path().to_path_buf(), false).expect("valid parent root");
        let captured: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let writer = Arc::clone(&captured);
        let _ = std::thread::spawn(move || {
            let dir = ScratchDir::new_in(&config, "unit-panic");
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
    }

    #[test]
    fn keep_and_into_path_disarm_cleanup() {
        let parent = retained_parent("unit-keep");
        let config =
            ScratchConfig::new(parent.path().to_path_buf(), false).expect("valid parent root");

        let mut dir = ScratchDir::new_in(&config, "unit-keep");
        let path = dir.path().to_path_buf();
        dir.keep();
        drop(dir);
        assert!(path.is_dir(), "keep() leaves the dir on disk");

        let path = ScratchDir::new_in(&config, "unit-into").into_path();
        assert!(path.is_dir(), "into_path() returns the (kept) dir");
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
        // Exercises the environment-driven `new` entry point.
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

    // ----- configuration layer (pure; no global-env mutation) -----

    #[test]
    fn resolve_refuses_relative_empty_and_malformed_roots() {
        let default = ScratchConfig::default_root();

        assert!(matches!(
            ScratchConfig::resolve(Some(OsStr::new("relative/run/root")), None, default.clone()),
            Err(ConfigError::RelativeRoot(_))
        ));
        assert!(matches!(
            ScratchConfig::resolve(Some(OsStr::new("")), None, default.clone()),
            Err(ConfigError::EmptyRoot)
        ));
        assert!(matches!(
            ScratchConfig::resolve(Some(OsStr::new("/absolute/../escape")), None, default.clone()),
            Err(ConfigError::MalformedRoot(_))
        ));

        // Unset root resolves to the default with cleanup semantics.
        let cfg = ScratchConfig::resolve(None, None, default.clone()).expect("default resolves");
        assert_eq!(cfg.root(), default.as_path());
        assert!(!cfg.retain(), "unset retain => cleanup");

        // Retention parsing accepts only 0/1.
        assert!(!ScratchConfig::resolve(None, Some(OsStr::new("0")), default.clone())
            .expect("0 is valid")
            .retain());
        assert!(ScratchConfig::resolve(None, Some(OsStr::new("1")), default.clone())
            .expect("1 is valid")
            .retain());
        assert!(matches!(
            ScratchConfig::resolve(None, Some(OsStr::new("yes")), default.clone()),
            Err(ConfigError::InvalidRetain(_))
        ));

        // An existing non-directory root is refused. The fixture lives under a
        // retained run parent and is left in place as run evidence.
        let parent = retained_parent("prop-notdir");
        let file = parent.path().join("file-not-dir");
        std::fs::write(&file, b"x").expect("write file");
        assert!(matches!(
            ScratchConfig::resolve(
                Some(file.as_os_str()),
                None,
                ScratchConfig::default_root()
            ),
            Err(ConfigError::RootNotDir(_))
        ));
    }

    #[test]
    fn default_mode_config_cleans_on_success() {
        // Regression coverage for the default (no env override) policy: a config
        // resolved with no overrides is not retained and removes on success.
        let parent = retained_parent("default-mode");
        let config = ScratchConfig::resolve(None, None, parent.path().to_path_buf())
            .expect("default resolve on an absolute root");
        assert!(!config.retain(), "unset AGALMA_TEST_RETAIN means cleanup");
        let dir = ScratchDir::new_in(&config, "default-case");
        let path = dir.path().to_path_buf();
        drop(dir);
        assert!(
            !path.exists(),
            "default mode removes the scratch dir on success"
        );
    }

    #[test]
    fn generated_relative_roots_are_refused() {
        let mut rng = Rng::new(0xA6A1_4D58_0000_0005);
        for case in 0..128u64 {
            let components = 1 + rng.below(4);
            let mut rel = String::new();
            for i in 0..components {
                if i > 0 {
                    rel.push('/');
                }
                rel.push((b'a' + rng.below(26) as u8) as char);
            }
            let got = ScratchConfig::resolve(
                Some(OsStr::new(&rel)),
                None,
                ScratchConfig::default_root(),
            );
            assert!(
                matches!(got, Err(ConfigError::RelativeRoot(_))),
                "case {case}: {rel:?} must be refused as relative"
            );
        }
    }

    // ----- generated allocation properties -----

    #[test]
    fn generated_names_are_confined_or_rejected() {
        // Boundary coverage: valid single-component names are confined to the
        // root; separator/relative/empty names are refused before allocation.
        let parent = retained_parent("prop-names");
        let base = parent.path().to_path_buf();
        let mut rng = Rng::new(0xA6A1_4D58_0000_0006);
        let mut valid = 0u64;
        let mut invalid = 0u64;

        for case in 0..128u64 {
            let name = match rng.below(9) {
                0 => format!("n{case}"),
                1 => format!("a{case}/b"),
                2 => "..".to_string(),
                3 => String::new(),
                4 => ".".to_string(),
                5 => format!("/abs{case}"),
                6 => format!("a{case}/../../b"),
                7 => format!("t{case}/"),
                _ => format!("n{case}\0"),
            };
            match validate_name(&name) {
                Ok(()) => {
                    valid += 1;
                    let child = base.join(&name);
                    assert_eq!(
                        child.parent(),
                        Some(base.as_path()),
                        "case {case}: valid name {name:?} stays a direct child"
                    );
                    assert!(
                        child.starts_with(&base),
                        "case {case}: valid name {name:?} stays confined"
                    );
                }
                Err(ConfigError::InvalidName(_)) => invalid += 1,
                Err(other) => panic!("case {case}: unexpected error {other:?}"),
            }
        }

        assert!(
            valid > 0 && invalid > 0,
            "generator exercised both boundaries (valid={valid}, invalid={invalid})"
        );
    }

    #[test]
    fn name_boundaries_reject_separators_dots_and_nul() {
        // Raw boundary cases asserted independently of the component parser.
        for bad in [
            "", ".", "..", "/", "a/", "a/.", "a/..", "a/b", "/a", "a//b", "a/./b",
            "a\0", "\0", "a\0b",
        ] {
            assert!(
                matches!(validate_name(bad), Err(ConfigError::InvalidName(_))),
                "must reject {bad:?}"
            );
        }
        // Names that only *look* like dot/separator edge cases stay normal.
        for good in ["a", "a-b", "a.b", "..a", "a..", "...", "a b", "a\tb"] {
            assert!(validate_name(good).is_ok(), "must accept {good:?}");
        }
    }

    #[test]
    fn invalid_name_allocation_is_refused_without_escaping() {
        let parent = retained_parent("prop-escape");
        let base = parent.path().to_path_buf();
        let config = ScratchConfig::new(base.clone(), false).expect("valid parent root");
        for bad in ["../escape", "a/b", "..", ".", "", "/abs", "x/../../y"] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ScratchDir::new_in(&config, bad)
            }));
            assert!(result.is_err(), "invalid name {bad:?} must be refused");
        }
        // No allocation succeeded, so the retained parent is still empty.
        let entries = std::fs::read_dir(&base).expect("read parent").count();
        assert_eq!(entries, 0, "no scratch dir was created inside the root");
    }

    #[test]
    fn generated_exclusive_allocation_skips_existing_and_never_clobbers() {
        let parent = retained_parent("prop-exclusive");
        let root = parent.path().to_path_buf();
        let mut rng = Rng::new(0xA6A1_4D58_0000_0002);

        for case in 0..128u64 {
            let decoy_name = format!("d{case}-{}", rng.next_u64());
            let decoy = root.join(&decoy_name);
            std::fs::create_dir_all(&decoy).expect("plant decoy evidence");
            std::fs::write(decoy.join("keep"), b"evidence").expect("decoy payload");

            let fresh_a = format!("f{case}a-{}", rng.next_u64());
            let fresh_b = format!("f{case}b-{}", rng.next_u64());
            let chosen = create_exclusive(
                &root,
                [decoy_name.clone(), fresh_a.clone(), fresh_b.clone()],
            );

            assert_eq!(
                chosen,
                root.join(&fresh_a),
                "case {case}: allocation skips the existing candidate"
            );
            assert!(
                decoy.is_dir() && decoy.join("keep").is_file(),
                "case {case}: existing evidence not clobbered"
            );
            assert!(chosen.is_dir(), "case {case}: fresh dir created exclusively");
            // Evidence retained intentionally; no cleanup here.
        }
    }

    #[test]
    fn generated_allocations_are_unique() {
        // Retained so the 128 distinct allocations remain as evidence.
        let config = scratch_config(true);
        let mut rng = Rng::new(0xA6A1_4D58_0000_0003);
        let mut seen = HashSet::new();

        for case in 0..128u64 {
            let name = format!("u{case}-{}", rng.next_u64());
            let dir = ScratchDir::new_in(&config, &name);
            assert!(
                seen.insert(dir.path().to_path_buf()),
                "case {case}: allocation path is unique"
            );
            drop(dir);
        }

        assert_eq!(seen.len(), 128, "all 128 allocations were distinct");
        for path in &seen {
            assert!(path.is_dir(), "retained allocation present: {path:?}");
        }
    }

    #[test]
    fn generated_root_confinement_and_retention_policy() {
        let parent = retained_parent("prop-conf");
        let base = parent.path().to_path_buf();
        let snapshots = parent.child("snapshots");
        let mut rng = Rng::new(0xA6A1_4D58_0000_0004);

        for case in 0..128u64 {
            let root = base.join(format!("case-{case}-{}", rng.next_u64()));
            std::fs::create_dir_all(&root).expect("mkdir generated root");
            let retain = rng.next_u64() & 1 == 1;

            // Retain the generated inputs so an intentional cleanup assertion
            // still leaves a replayable snapshot behind.
            std::fs::write(
                snapshots.join(format!("case-{case}.txt")),
                format!("case={case}\nretain={retain}\nroot={}\n", root.display()),
            )
            .expect("write input snapshot");

            let config = ScratchConfig::new(root.clone(), retain).expect("valid root");
            let dir = ScratchDir::new_in(&config, "case");
            assert_eq!(
                dir.retained(),
                retain,
                "case {case}: guard reports retention"
            );
            let path = dir.path().to_path_buf();
            assert_eq!(
                path.parent(),
                Some(root.as_path()),
                "case {case}: confined to root"
            );
            assert!(path.starts_with(&root), "case {case}: prefix is the root");

            drop(dir);
            assert_eq!(
                path.exists(),
                retain,
                "case {case}: retain={retain} governs cleanup"
            );
        }
    }

    #[test]
    fn env_configuration_drives_root_and_retention() {
        // Reading the environment is race-free; tests never mutate it.
        let config = ScratchConfig::from_env().expect("environment configuration is valid");
        let root = config.root().to_path_buf();
        std::fs::create_dir_all(&root).expect("create configured run root");

        // Retain a replay snapshot at the configured root (survives cleanup).
        std::fs::write(
            root.join(format!("replay-env-{}.txt", std::process::id())),
            REPLAY_NOTE,
        )
        .expect("write replay snapshot");

        let dir = ScratchDir::new_in(&config, "env-evidence");
        let path = dir.path().to_path_buf();
        assert!(
            path.starts_with(&root),
            "allocated under the configured root"
        );

        // A fixture file lives beneath a real fixture directory (not on a dir).
        let fixture = dir.child("fixtures");
        std::fs::write(fixture.join("replay.txt"), REPLAY_NOTE).expect("write fixture replay");

        let retain = config.retain();
        drop(dir);
        assert_eq!(
            path.exists(),
            retain,
            "retention follows AGALMA_TEST_RETAIN (root {})",
            root.display()
        );
    }

    #[test]
    fn short_alias_to_long_retained_root_binds_a_real_socket() {
        use std::os::unix::net::UnixListener;

        // Physical evidence lives under the configured persistent run root;
        // this long path is exactly what overflows macOS SUN_LEN.
        let long_base = configured_root();
        std::fs::create_dir_all(&long_base).expect("create configured run root");
        let pid = std::process::id();
        let long_root = long_base.join(format!("agalma-testkit-alias-long-{pid}-{}", now_nanos()));
        std::fs::create_dir_all(&long_root).expect("create long retained root");

        // A short repository-local alias points at the long root. Allocation is
        // exclusive and never removes an existing alias or its evidence.
        let alias_base = short_alias_base();
        std::fs::create_dir_all(&alias_base).expect("create alias base");
        let alias = allocate_alias(&alias_base, &long_root, pid);

        let config = ScratchConfig::new(alias.clone(), true).expect("alias is a valid dir root");
        assert_eq!(
            config.root(),
            alias.as_path(),
            "symlink alias is preserved, not canonicalized"
        );

        let dir = ScratchDir::new_in(&config, "s");
        let returned = dir.path().to_path_buf();
        assert!(
            returned.starts_with(&alias),
            "returned path keeps the short alias prefix: {returned:?}"
        );

        let socket = returned.join("s.sock");
        assert!(
            socket.as_os_str().len() < 104,
            "socket path must fit macOS SUN_LEN: {socket:?} ({} bytes)",
            socket.as_os_str().len()
        );

        let listener =
            UnixListener::bind(&socket).expect("bind a real UnixListener on the short alias path");
        drop(listener);

        // The physical fixture lands under the long root via the alias.
        let physical = long_root
            .join(returned.file_name().expect("scratch basename"))
            .join("s.sock");
        assert!(
            physical.exists(),
            "fixture physically retained at the configured root: {physical:?}"
        );

        // Retained: the tree survives dropping the guard.
        let retained_path = returned.clone();
        drop(dir);
        assert!(retained_path.is_dir(), "retained scratch dir survives drop");
        assert!(long_root.is_dir(), "long run root retained as evidence");
    }
}
