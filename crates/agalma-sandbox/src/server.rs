//! Seatbelt `SandboxApi` component server (B1.3).
//!
//! Hosts [`SeatbeltSandbox`] behind the selected transport so other components
//! (the conductor, and the OpenCode harness server) can drive confined process
//! lifecycle across a process boundary instead of embedding the backend.
//!
//! Sandbox-backed methods create processes through `tokio::process`, so
//! `serve_wire` installs a multithread Tokio runtime for both wires (mirroring
//! the OpenCode harness server). Since IPCX.3 the server serves `Connect` by
//! default; `AGALMA_IPC_WIRE=framed` restores the B1 length-prefixed transport.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use agalma_ipc::{register_sandbox, CommandSpec, Server, ServerInfo, Wire};

use crate::seatbelt::SeatbeltSandbox;

/// Binary name of the sandbox component server.
pub const SERVER_BIN: &str = "agalma-sandbox-server";
/// Announced API name (matches the contract trait).
pub const API_NAME: &str = "SandboxApi";
/// Announced API version.
pub const API_VERSION: u32 = 0;
/// Announced implementation name.
pub const IMPL_NAME: &str = "seatbelt";
/// Announced implementation version.
pub const IMPL_VERSION: &str = "seatbelt/v1";

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

/// Launch spec for the sandbox server (caller appends `--socket <path>`).
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Build the sandbox server: the announced identity plus every sandbox method.
pub fn build_server() -> Server {
    let mut server = Server::new(server_info());
    register_sandbox(&mut server, SeatbeltSandbox::new());
    server
}

/// Bind `socket` and serve over the wire selected by `AGALMA_IPC_WIRE`
/// (`connect` default, `framed` optional; IPCX.3) until stopped.
pub fn serve(socket: &Path) -> io::Result<()> {
    serve_wire(socket, Wire::from_env(Wire::Connect))
}

/// Bind `socket` and serve over `wire` until stopped.
pub fn serve_wire(socket: &Path, wire: Wire) -> io::Result<()> {
    let server = build_server();
    agalma_ipc::sandbox_server::serve_sandbox(server, socket, wire)
}

/// Full argv/env entry point used by the server binary.
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let socket = agalma_ipc::socket_from_args(args)?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create socket dir {}: {e}", parent.display()))?;
    }
    serve(&socket).map_err(|e| format!("serve {}: {e}", socket.display()))
}

/// Default socket path beside a run root (used by tests and callers).
pub fn default_socket_for(root: &Path) -> PathBuf {
    root.join("sandbox.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_the_sandbox_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
    }

    #[test]
    fn server_announces_the_sandbox_identity_for_each_method() {
        let server = build_server();
        assert_eq!(server.info().impl_name, IMPL_NAME);
        agalma_ipc::sandbox_server::ensure_all_handlers(&server).expect("all handlers registered");
    }
}
