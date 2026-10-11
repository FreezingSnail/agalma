//! OpenCode `HarnessApi` component server (B1.2; sandbox client B1.3).
//!
//! Hosts the real [`OpenCodeHarness`] behind the framed transport. The server is
//! trusted code: the confined `opencode serve` child is launched through the
//! extracted `SandboxApi` boundary — a [`SandboxClient`] connected to the
//! sandbox component server named by `AGALMA_SANDBOX_SOCKET` — instead of
//! embedding `agalma-sandbox` in-process (B1.3).
//!
//! Configuration arrives from the environment (the conductor's `server_command()`
//! / `ProcessBinding` supplies it): `AGALMA_HARNESS_ROOT` names the attempt root;
//! `AGALMA_HARNESS_PROFILE_PATH`, `AGALMA_HARNESS_PROXY_URL`,
//! `AGALMA_HARNESS_PAID_PROXY`, and `AGALMA_MODEL` mirror the build/verifier
//! config. Sandbox-backed methods need a Tokio reactor, so the binary runs the
//! serial serve loop inside a multithread runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agalma_contracts::harness::HarnessApi;
use agalma_contracts::sandbox::SandboxApi;
use agalma_ipc::{register_harness, server_info_from, CommandSpec, SandboxClient, Server};

use crate::adapter::{OpenCodeHarness, OpenCodeHarnessConfig};
use crate::config::{build_config, OpenCodeConfigInit};

/// Binary name of the OpenCode component server.
pub const SERVER_BIN: &str = "agalma-harness-opencode-server";
/// Environment variable naming the attempt root.
pub const ROOT_ENV: &str = "AGALMA_HARNESS_ROOT";
/// Environment variable naming the rendered sandbox profile.
pub const PROFILE_PATH_ENV: &str = "AGALMA_HARNESS_PROFILE_PATH";
/// Environment variable carrying the parent-owned proxy URL.
pub const PROXY_URL_ENV: &str = "AGALMA_HARNESS_PROXY_URL";
/// Environment variable enabling the paid provider override (`1`/`true`).
pub const PAID_ENV: &str = "AGALMA_HARNESS_PAID_PROXY";
/// Environment variable naming the sandbox component server's Unix socket.
///
/// B1.3: the harness server no longer embeds `agalma-sandbox`; it connects a
/// [`SandboxClient`] to this socket for confined `opencode serve` launches.
pub const SANDBOX_SOCKET_ENV: &str = "AGALMA_SANDBOX_SOCKET";
/// Environment variable naming an extra read/write root (Seatbelt `EXTRA_RW`),
/// e.g. the target repo a planning session edits while its runtime state stays
/// under the attempt root.
pub const EXTRA_RW_ENV: &str = "AGALMA_HARNESS_EXTRA_RW";

/// Launch spec for the OpenCode server.
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Assemble the server's harness config from the process environment.
pub fn config_from_env(root: PathBuf) -> OpenCodeHarnessConfig {
    let profile_path = std::env::var_os(PROFILE_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("worker.sb"));
    let proxy_url = std::env::var(PROXY_URL_ENV)
        .ok()
        .filter(|url| !url.is_empty());
    let paid_proxy = matches!(
        std::env::var(PAID_ENV).ok().as_deref(),
        Some("1") | Some("true")
    );
    let model = std::env::var("AGALMA_MODEL").unwrap_or_default();
    let mut extra_env = BTreeMap::new();
    extra_env.insert(
        "CARGO_HOME".to_string(),
        root.join("cargo-home").to_string_lossy().into_owned(),
    );
    extra_env.insert(
        "CARGO_TARGET_DIR".to_string(),
        root.join("agent-target").to_string_lossy().into_owned(),
    );
    let extra_rw_roots = std::env::var_os(EXTRA_RW_ENV)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    build_config(OpenCodeConfigInit {
        profile_path,
        attempt_dir: root.join("attempt"),
        protected_dir: root.join("protected"),
        sock_dir: root.join("sock"),
        model,
        proxy_url,
        paid_proxy,
        extra_ro_roots: Vec::new(),
        extra_rw_roots,
        extra_env,
    })
}

/// Build the OpenCode server around an injected sandbox backend.
pub fn build_server(sandbox: Box<dyn SandboxApi + Send>, config: OpenCodeHarnessConfig) -> Server {
    let harness = OpenCodeHarness::new(sandbox, config);
    let describe = harness.describe();
    let mut server = Server::new(server_info_from(&describe));
    register_harness(&mut server, harness);
    server
}

/// Resolve the attempt root: [`ROOT_ENV`] when set, else `<socket parent>`.
pub fn root_for(socket: &Path) -> PathBuf {
    std::env::var_os(ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            socket
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("opencode-harness")
        })
}

/// Full argv/env entry point used by the server binary.
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let socket = agalma_ipc::socket_from_args(args)?;
    let root = root_for(&socket);
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("create harness root {}: {e}", root.display()))?;
    let config = config_from_env(root);
    let sandbox_socket = std::env::var_os(SANDBOX_SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            socket
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("sandbox.sock")
        });
    let sandbox = SandboxClient::connect(&sandbox_socket).map_err(|e| {
        format!(
            "connect sandbox server at {}: {e}",
            sandbox_socket.display()
        )
    })?;
    let mut server = build_server(Box::new(sandbox), config);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let display = socket.display().to_string();
    runtime
        .block_on(async move { server.serve(&socket) })
        .map_err(|e| format!("serve {display}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_the_opencode_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
    }
}
