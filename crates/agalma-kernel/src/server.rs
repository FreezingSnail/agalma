//! Kernel `ExecutionApi` component server (B1.7).
//!
//! Hosts the durable execution kernel (`KernelEngine<LedgerClient>`) behind the
//! framed transport so the conductor can drive the process-ready candidate
//! replacement for the M1 seed executor across a process boundary. The server's
//! ledger dependency is itself the extracted ledger boundary: it connects a
//! [`LedgerClient`] to the ledger component server (D2 derived requirement —
//! dependencies stay dependencies, extraction changes transport only).
//!
//! The process-lifecycle seam is a local process launcher: the engine binds a
//! [`LocalLauncher`] so ownership reclaim/cancellation can confirm termination.
//! There is no sandbox socket and no component-server hop on this edge
//! (`docs/sandbox-removal-design.md` §5.2).
//!
//! ## Boundary gap
//!
//! [`ExecutionApi::step`]/`recover` (effect dispatch/reconciliation) need an
//! activity handler registry that cannot be hosted generically; the engine
//! adapter returns `UnsupportedCapability` for them. The engine-op surface
//! (`apply`/`pending_dispatch`/`load_state`/timers/directives/ownership) is
//! hosted in full. See `docs/b1-execution-evidence.md`.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use agalma_ipc::{
    register_execution, serve_execution, CommandSpec, LedgerClient, Server, ServerInfo, Wire,
};
use agalma_process::LocalLauncher;

use crate::engine::KernelEngine;

/// Binary name of the kernel component server.
pub const SERVER_BIN: &str = "agalma-kernel-server";
/// Announced API name (matches the contract trait).
pub const API_NAME: &str = "ExecutionApi";
/// Announced API version.
pub const API_VERSION: u32 = 0;
/// Announced implementation name.
pub const IMPL_NAME: &str = "kernel";
/// Announced implementation version.
pub const IMPL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Socket file name under the run state dir (short, within `SUN_LEN`).
pub const SOCKET_NAME: &str = "execution.sock";
/// Environment variable naming the ledger component server socket.
pub const LEDGER_SOCKET_ENV: &str = "AGALMA_KERNEL_LEDGER_SOCKET";

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

/// Launch spec for the kernel server (caller appends `--socket <path>` and
/// `--state-dir <path>` plus the ledger socket env).
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Build the kernel server around an already-open engine.
pub fn build_server(engine: KernelEngine<LedgerClient>) -> Server {
    let mut server = Server::new(server_info());
    register_execution(&mut server, engine);
    server
}

/// Connect the ledger client, open the kernel (binding a [`LocalLauncher`] as
/// the process-lifecycle seam), and bind `socket`; serve over the wire selected
/// by `AGALMA_IPC_WIRE` (`connect` default, `framed` optional; IPCX.7) until
/// stopped.
///
/// The same wire backs the kernel's own `LedgerClient` connection, so the nested
/// kernel→ledger edge speaks the transport the supervisor selected for this
/// boundary.
pub fn serve(socket: &Path, state_dir: &Path, ledger_socket: &Path) -> io::Result<()> {
    serve_wire(
        socket,
        state_dir,
        ledger_socket,
        Wire::from_env(Wire::Connect),
    )
}

/// Connect the ledger client, open the kernel (binding a [`LocalLauncher`]), and
/// bind `socket`; serve over `wire` until stopped (IPCX.7).
pub fn serve_wire(
    socket: &Path,
    state_dir: &Path,
    ledger_socket: &Path,
    wire: Wire,
) -> io::Result<()> {
    let ledger = LedgerClient::connect_wire(ledger_socket, state_dir, wire)
        .map_err(|e| io::Error::other(format!("connect ledger server: {e}")))?;
    let engine = KernelEngine::open(ledger)
        .map_err(|e| io::Error::other(format!("open kernel: {e}")))?
        .with_launcher(Box::new(LocalLauncher::new()));
    let server = build_server(engine);
    serve_execution(server, socket, wire)
}

/// Full argv/env entry point used by the server binary.
///
/// Required: `--socket <path>` and `--state-dir <path>`, plus the ledger socket
/// in [`LEDGER_SOCKET_ENV`].
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let socket = agalma_ipc::socket_from_args(args.clone())?;
    let state_dir = state_dir_from_args(&args)?;
    let ledger_socket = std::env::var(LEDGER_SOCKET_ENV)
        .map(PathBuf::from)
        .map_err(|_| format!("missing required {LEDGER_SOCKET_ENV}"))?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create socket dir {}: {e}", parent.display()))?;
    }
    serve(&socket, &state_dir, &ledger_socket)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_the_kernel_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
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
            PathBuf::from("/run/state/execution.sock")
        );
    }
}
