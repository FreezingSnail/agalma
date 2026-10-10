//! Git-backed `WorkspaceApi` component server (B1.4).
//!
//! Hosts [`Workspace`] behind the framed transport so the conductor can drive
//! isolated checkout, integration, tagging, and reconciliation across a process
//! boundary instead of embedding the git backend. The server is rooted at a base
//! directory (`--base-dir <path>`): each execution gets `<base_dir>/runs/<exec>`
//! just as in-process.
//!
//! The backend shells out to the `git` CLI through `std::process`, so the serial
//! serve loop needs no async runtime (mirroring the reference harness server, not
//! the Tokio-based sandbox/OpenCode servers).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use agalma_ipc::{register_workspace, CommandSpec, Server, ServerInfo};

use crate::Workspace;

/// Binary name of the workspace component server.
pub const SERVER_BIN: &str = "agalma-workspace-server";
/// Announced API name (matches the contract trait).
pub const API_NAME: &str = "WorkspaceApi";
/// Announced API version.
pub const API_VERSION: u32 = 0;
/// Announced implementation name.
pub const IMPL_NAME: &str = "git";
/// Announced implementation version.
pub const IMPL_VERSION: &str = "git/v1";

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

/// Launch spec for the workspace server (caller appends `--socket <path>` and
/// `--base-dir <path>`).
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Build the workspace server rooted at `base_dir`: the announced identity plus
/// every workspace operation.
pub fn build_server(base_dir: &Path) -> Server {
    let mut server = Server::new(server_info());
    register_workspace(&mut server, Workspace::new(base_dir));
    server
}

/// Bind `socket` and serve until stopped, hosting a `Workspace` rooted at
/// `base_dir`.
pub fn serve(socket: &Path, base_dir: &Path) -> io::Result<()> {
    let mut server = build_server(base_dir);
    server.serve(socket)
}

/// Full argv/env entry point used by the server binary.
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let socket = agalma_ipc::socket_from_args(args.clone())?;
    let base_dir = base_dir_from_args(&args)?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create socket dir {}: {e}", parent.display()))?;
    }
    std::fs::create_dir_all(&base_dir)
        .map_err(|e| format!("create base dir {}: {e}", base_dir.display()))?;
    serve(&socket, &base_dir).map_err(|e| format!("serve {}: {e}", socket.display()))
}

/// Parse `--base-dir <path>` / `--base-dir=<path>`.
fn base_dir_from_args(args: &[String]) -> Result<PathBuf, String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--base-dir" {
            let value = it
                .next()
                .ok_or_else(|| "--base-dir requires a path argument".to_string())?;
            return Ok(PathBuf::from(value));
        }
        if let Some(value) = arg.strip_prefix("--base-dir=") {
            return Ok(PathBuf::from(value));
        }
    }
    Err("missing required --base-dir <path>".to_string())
}

/// Default socket path beside a run root (used by tests and callers).
pub fn default_socket_for(root: &Path) -> PathBuf {
    root.join("workspace.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_the_workspace_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
    }

    #[test]
    fn server_announces_the_workspace_identity_for_each_method() {
        let server = build_server(Path::new("/run/agalma-state"));
        assert_eq!(server.info().impl_name, IMPL_NAME);
        agalma_ipc::workspace_server::ensure_all_handlers(&server).expect("all handlers registered");
    }

    #[test]
    fn base_dir_arg_parsing() {
        assert_eq!(
            base_dir_from_args(&["--base-dir".into(), "/run/state".into()]).unwrap(),
            PathBuf::from("/run/state")
        );
        assert_eq!(
            base_dir_from_args(&["--base-dir=/run/state".into()]).unwrap(),
            PathBuf::from("/run/state")
        );
        assert!(base_dir_from_args(&[]).is_err());
    }
}
