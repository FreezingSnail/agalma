//! SQLite-backed `LedgerApi` component server (B1.6).
//!
//! Hosts [`SqliteLedger`] behind the framed transport so the conductor can drive
//! canonical durable state across a process boundary instead of embedding the
//! SQLite connection. The server is the **single writer** for one state
//! generation: it binds a Unix socket, owns `<state-dir>/ledger.sqlite`, and
//! refuses to open when another live server holds the same state dir (a
//! pid/lock file under the state dir, [`LOCK_NAME`]).
//!
//! Snapshot transfer (D3) uses files under the state dir, never frames: the
//! `export_state_to`/`import_state_from` handlers write/read the canonical JSON
//! snapshot and only the path crosses the wire.
//!
//! Since IPCX.6 the server serves the Connect transport by default
//! (`AGALMA_IPC_WIRE=framed` restores the B1 length-prefixed transport). The
//! backend is `rusqlite` (synchronous); `serve_wire` installs a multithread
//! Tokio runtime for both branches so server-hosted transports share one code
//! path (mirroring the sandbox server).

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agalma_ipc::{register_ledger, CommandSpec, Server, ServerInfo, Wire};

use crate::SqliteLedger;

/// Binary name of the ledger component server.
pub const SERVER_BIN: &str = "agalma-ledger-server";
/// Announced API name (matches the contract trait).
pub const API_NAME: &str = "LedgerApi";
/// Announced API version.
pub const API_VERSION: u32 = 0;
/// Announced implementation name.
pub const IMPL_NAME: &str = "sqlite";
/// Announced implementation version.
pub const IMPL_VERSION: &str = "rusqlite/v1";
/// Socket file name under the run state dir (short, within `SUN_LEN`).
pub const SOCKET_NAME: &str = "ledger.sock";
/// Single-writer lock file name under the run state dir.
pub const LOCK_NAME: &str = "ledger-server.lock";
/// Database file name under the run state dir.
pub const DB_NAME: &str = "ledger.sqlite";

/// Identity announced in the `READY` handshake line.
pub fn server_info() -> ServerInfo {
    ServerInfo {
        api: API_NAME.to_string(),
        api_version: API_VERSION,
        impl_name: IMPL_NAME.to_string(),
        impl_version: IMPL_VERSION.to_string(),
        capabilities: BTreeSet::new(),
    }
}

/// Launch spec for the ledger server (caller appends `--socket <path>` and
/// `--state-dir <path>`).
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Build the ledger server around an already-open [`SqliteLedger`].
pub fn build_server(ledger: SqliteLedger) -> Server {
    let mut server = Server::new(server_info());
    register_ledger(&mut server, ledger);
    server
}

/// Whether the server creates/initializes the ledger or opens an established one
/// (`agalma-5fy.5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Create the database if absent and run first-open schema initialization.
    Create,
    /// Open an existing database without creating, initializing or migrating.
    Existing,
}

/// Server argv flag selecting [`OpenMode::Existing`].
pub const OPEN_EXISTING_FLAG: &str = "--open-existing";

/// Acquire the single-writer lock for `state_dir`, open the ledger at
/// `<state_dir>/ledger.sqlite`, and bind `socket`; serve over the wire selected
/// by `AGALMA_IPC_WIRE` (`connect` default, `framed` optional; IPCX.6) until
/// stopped.
///
/// Refuses to start when another live server holds the state dir.
pub fn serve(socket: &Path, state_dir: &Path) -> io::Result<()> {
    serve_mode(socket, state_dir, Wire::from_env(Wire::Connect), OpenMode::Create)
}

/// Acquire the single-writer lock for `state_dir`, open the ledger at
/// `<state_dir>/ledger.sqlite`, and bind `socket`; serve over `wire` until
/// stopped.
///
/// Refuses to start when another live server holds the state dir.
pub fn serve_wire(socket: &Path, state_dir: &Path, wire: Wire) -> io::Result<()> {
    serve_mode(socket, state_dir, wire, OpenMode::Create)
}

/// Acquire the single-writer lock for `state_dir`, open the ledger at
/// `<state_dir>/ledger.sqlite` under `mode`, and bind `socket`; serve over
/// `wire` until stopped.
///
/// [`OpenMode::Existing`] opens without creating/initializing/migrating: a
/// missing, corrupt or incompatible established database fails to start.
/// Refuses to start when another live server holds the state dir.
pub fn serve_mode(
    socket: &Path,
    state_dir: &Path,
    wire: Wire,
    mode: OpenMode,
) -> io::Result<()> {
    let lock = StateLock::acquire(state_dir).map_err(io::Error::other)?;
    let ledger = match mode {
        OpenMode::Create => SqliteLedger::open(state_dir.join(DB_NAME)),
        OpenMode::Existing => SqliteLedger::open_existing(state_dir.join(DB_NAME)),
    }
    .map_err(|e| io::Error::other(format!("open ledger: {e}")))?;
    let server = build_server(ledger);
    // Keep the lock alive for the whole serve loop; it is released on process
    // exit (server killed) and removed by Drop on a clean return.
    let result = agalma_ipc::ledger_server::serve_ledger(server, socket, wire);
    drop(lock);
    result
}

/// Full argv/env entry point used by the server binary.
///
/// Required: `--socket <path>` and `--state-dir <path>`. Optional:
/// `--open-existing` (open an established ledger without create/init).
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let socket = agalma_ipc::socket_from_args(args.clone())?;
    let state_dir = state_dir_from_args(&args)?;
    let mode = if args.iter().any(|arg| arg == OPEN_EXISTING_FLAG) {
        OpenMode::Existing
    } else {
        OpenMode::Create
    };
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create socket dir {}: {e}", parent.display()))?;
    }
    serve_mode(&socket, &state_dir, Wire::from_env(Wire::Connect), mode)
        .map_err(|e| format!("serve {}: {e}", socket.display()))
}

/// Parse `--state-dir <path>` / `--state-dir=<path>`.
pub fn state_dir_from_args(args: &[String]) -> Result<PathBuf, String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--state-dir" {
            let value = it
                .next()
                .ok_or_else(|| "--state-dir requires a path argument".to_string())?;
            return Ok(PathBuf::from(value));
        }
        if let Some(value) = arg.strip_prefix("--state-dir=") {
            return Ok(PathBuf::from(value));
        }
    }
    Err("missing required --state-dir <path>".to_string())
}

/// Default socket path beside a run root (used by tests and callers).
pub fn default_socket_for(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET_NAME)
}

/// The single-writer lock held by a running server for one state dir.
///
/// `acquire` fails when a live pid already holds the lock; a stale lock (dead
/// pid) is reclaimed. Removed on drop.
#[derive(Debug)]
struct StateLock {
    path: PathBuf,
}

impl StateLock {
    fn acquire(state_dir: &Path) -> Result<StateLock, String> {
        std::fs::create_dir_all(state_dir)
            .map_err(|e| format!("cannot create state dir {}: {e}", state_dir.display()))?;
        let path = state_dir.join(LOCK_NAME);
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(StateLock { path });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && !holder_alive(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return Err(format!(
                        "another ledger server holds the state dir (lock {})",
                        path.display()
                    ));
                }
                Err(err) => {
                    return Err(format!("cannot create lock {}: {err}", path.display()));
                }
            }
        }
        Err(format!("cannot acquire lock {}", path.display()))
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn holder_alive(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_testkit::ScratchDir;

    #[test]
    fn command_names_the_ledger_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
    }

    #[test]
    fn server_announces_the_ledger_identity_for_each_method() {
        let dir = ScratchDir::new("ledger-server-handlers");
        let ledger = SqliteLedger::open(dir.join(DB_NAME)).expect("open ledger");
        let server = build_server(ledger);
        assert_eq!(server.info().impl_name, IMPL_NAME);
        agalma_ipc::ledger_server::ensure_all_handlers(&server).expect("all handlers registered");
    }

    #[test]
    fn state_dir_arg_parsing() {
        assert_eq!(
            state_dir_from_args(&["--state-dir".into(), "/run/state".into()]).unwrap(),
            PathBuf::from("/run/state")
        );
        assert_eq!(
            state_dir_from_args(&["--state-dir=/run/state".into()]).unwrap(),
            PathBuf::from("/run/state")
        );
        assert!(state_dir_from_args(&[]).is_err());
    }

    #[test]
    fn default_socket_is_under_the_state_dir() {
        assert_eq!(
            default_socket_for(Path::new("/run/state")),
            PathBuf::from("/run/state/ledger.sock")
        );
    }

    #[test]
    fn state_lock_refuses_a_live_holder_and_reclaims_a_stale_one() {
        // Run dir under `target/test-runs/` (never a system temp directory).
        let dir = ScratchDir::new("ledger-server-lock");

        let held = StateLock::acquire(&dir).expect("first acquire");
        // A second acquire while the same (live) pid holds it is refused.
        let err = StateLock::acquire(&dir).expect_err("second acquire refused");
        assert!(err.contains("another ledger server"), "{err}");

        // A dead pid's lock is reclaimed.
        drop(held);
        std::fs::write(dir.join(LOCK_NAME), "999999999").unwrap();
        let reclaimed = StateLock::acquire(&dir).expect("stale lock reclaimed");
        drop(reclaimed);
    }
}
