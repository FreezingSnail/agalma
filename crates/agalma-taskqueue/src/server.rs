//! `bd`-backed `TaskQueueApi` component server (B1.5).
//!
//! Hosts [`BdTaskQueue`] behind the selected transport so the conductor can drive
//! backlog intake, atomic claims, and status projection across a process
//! boundary instead of embedding the `bd` CLI adapter. The server is rooted at
//! the repository owning the backlog (`--repo <path>`): the adapter targets
//! `<repo>/.beads` with `bd --db`, exactly as in-process.
//!
//! Since IPCX.5 the server serves `Connect` by default;
//! `AGALMA_IPC_WIRE=framed` restores the B1 length-prefixed transport.
//!
//! The backend shells out to the `bd` CLI through `std::process`, so the serial
//! serve loop needs no async runtime (mirroring the reference harness and
//! workspace servers, not the Tokio-based sandbox/OpenCode servers).

use std::io;
use std::path::{Path, PathBuf};

use agalma_contracts::REVIEW_ADMISSION_CAPABILITY;
use agalma_ipc::{register_taskqueue, CommandSpec, Server, ServerInfo, Wire};

use crate::BdTaskQueue;

/// Binary name of the task-queue component server.
pub const SERVER_BIN: &str = "agalma-taskqueue-server";
/// Announced API name (matches the contract trait).
pub const API_NAME: &str = "TaskQueueApi";
/// Announced API version.
pub const API_VERSION: u32 = 0;
/// Announced implementation name.
pub const IMPL_NAME: &str = "bd";
/// Announced implementation version.
pub const IMPL_VERSION: &str = "bd/v1";
/// Environment variable overriding the `bd` executable the server shells to.
pub const BD_ENV: &str = "AGALMA_TASKQUEUE_BD";

/// Identity announced in the `READY` handshake line.
///
/// Advertises [`REVIEW_ADMISSION_CAPABILITY`]: this `bd`-backed server computes
/// the prerequisite-aware review admission projection, so a client may call
/// `review_admission` against it.
pub fn server_info() -> ServerInfo {
    ServerInfo {
        api: API_NAME.to_string(),
        api_version: API_VERSION,
        impl_name: IMPL_NAME.to_string(),
        impl_version: IMPL_VERSION.to_string(),
        capabilities: [REVIEW_ADMISSION_CAPABILITY.to_string()]
            .into_iter()
            .collect(),
    }
}

/// Launch spec for the task-queue server (caller appends `--socket <path>` and
/// `--repo <path>`).
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Build the task-queue server rooted at `repo`: the announced identity plus
/// every task-queue operation.
pub fn build_server(repo: &Path) -> Server {
    build_server_with(BdTaskQueue::new(repo))
}

/// Build the task-queue server from an explicit adapter (used to override the
/// `bd` executable in tests).
pub fn build_server_with(queue: BdTaskQueue) -> Server {
    let mut server = Server::new(server_info());
    register_taskqueue(&mut server, queue);
    server
}

/// Bind `socket` and serve over the wire selected by `AGALMA_IPC_WIRE`
/// (`connect` default, `framed` optional; IPCX.5), hosting a `BdTaskQueue`
/// rooted at `repo`.
pub fn serve(socket: &Path, repo: &Path) -> io::Result<()> {
    serve_wire(socket, repo, Wire::from_env(Wire::Connect))
}

/// Bind `socket` and serve over `wire` until stopped, hosting a `BdTaskQueue`
/// rooted at `repo`.
pub fn serve_wire(socket: &Path, repo: &Path, wire: Wire) -> io::Result<()> {
    let server = build_server(repo);
    agalma_ipc::taskqueue_server::serve_taskqueue(server, socket, wire)
}

/// Full argv/env entry point used by the server binary.
///
/// Required: `--socket <path>` and `--repo <path>` (the repository whose
/// `.beads` store is served). Optional: `--bd <program>` (or `AGALMA_TASKQUEUE_BD`)
/// selects the `bd` executable.
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let socket = agalma_ipc::socket_from_args(args.clone())?;
    let repo = repo_from_args(&args)?;
    let program = program_from_args(&args);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create socket dir {}: {e}", parent.display()))?;
    }
    let mut queue = BdTaskQueue::new(&repo);
    if let Some(program) = program {
        queue = queue.with_program(program);
    }
    let server = build_server_with(queue);
    agalma_ipc::taskqueue_server::serve_taskqueue(server, &socket, Wire::from_env(Wire::Connect))
        .map_err(|e| format!("serve {}: {e}", socket.display()))
}

/// Parse `--repo <path>` / `--repo=<path>`.
fn repo_from_args(args: &[String]) -> Result<PathBuf, String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--repo" {
            let value = it
                .next()
                .ok_or_else(|| "--repo requires a path argument".to_string())?;
            return Ok(PathBuf::from(value));
        }
        if let Some(value) = arg.strip_prefix("--repo=") {
            return Ok(PathBuf::from(value));
        }
    }
    Err("missing required --repo <path>".to_string())
}

/// Resolve the `bd` executable from `--bd <program>` or [`BD_ENV`].
fn program_from_args(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--bd" {
            if let Some(value) = it.next() {
                return Some(value.clone());
            }
        } else if let Some(value) = arg.strip_prefix("--bd=") {
            return Some(value.to_string());
        }
    }
    std::env::var(BD_ENV).ok().filter(|v| !v.is_empty())
}

/// Default socket path beside a run root (used by tests and callers).
pub fn default_socket_for(root: &Path) -> PathBuf {
    root.join("taskqueue.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_the_taskqueue_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
    }

    #[test]
    fn server_announces_the_taskqueue_identity_for_each_method() {
        let server = build_server(Path::new("/run/agalma-repo"));
        assert_eq!(server.info().impl_name, IMPL_NAME);
        agalma_ipc::taskqueue_server::ensure_all_handlers(&server)
            .expect("all handlers registered");
    }

    #[test]
    fn repo_arg_parsing() {
        assert_eq!(
            repo_from_args(&["--repo".into(), "/run/repo".into()]).unwrap(),
            PathBuf::from("/run/repo")
        );
        assert_eq!(
            repo_from_args(&["--repo=/run/repo".into()]).unwrap(),
            PathBuf::from("/run/repo")
        );
        assert!(repo_from_args(&[]).is_err());
    }

    #[test]
    fn bd_program_arg_parsing() {
        assert_eq!(
            program_from_args(&["--bd".into(), "/opt/bd".into()]),
            Some("/opt/bd".to_string())
        );
        assert_eq!(
            program_from_args(&["--bd=/opt/bd".into()]),
            Some("/opt/bd".to_string())
        );
    }
}
