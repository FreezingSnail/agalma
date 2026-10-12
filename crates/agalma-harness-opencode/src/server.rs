//! OpenCode `HarnessApi` component server (B1.2; sandbox client B1.3).
//!
//! Hosts the real [`OpenCodeHarness`] behind the framed transport. The server is
//! trusted code: the confined `opencode serve` child is launched through the
//! extracted `SandboxApi` boundary — a [`SandboxClient`] connected to the
//! sandbox component server named by `AGALMA_SANDBOX_SOCKET` — instead of
//! embedding `agalma-sandbox` in-process (B1.3).
//!
//! Configuration arrives from the environment (the conductor's `server_command()`
//! / `ProcessBinding` supplies it). The process transport serializes the full
//! explicit launch configuration into [`LAUNCH_CONFIG_ENV`] (see
//! [`LaunchConfig`]); [`config_from_env`] reconstructs the harness config from it
//! verbatim, so the server's sandbox `LaunchSpec` matches the in-process one.
//! The legacy per-variable form (`AGALMA_HARNESS_ROOT`,
//! `AGALMA_HARNESS_PROFILE_PATH`, `AGALMA_HARNESS_PROXY_URL`,
//! `AGALMA_HARNESS_PAID_PROXY`, `AGALMA_MODEL`) is kept for direct invocation.
//! Sandbox-backed methods need a Tokio reactor, so the binary runs the serial
//! serve loop inside a multithread runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agalma_contracts::harness::HarnessApi;
use agalma_contracts::sandbox::SandboxApi;
use agalma_ipc::{register_harness, server_info_from, CommandSpec, SandboxClient, Server, Wire};
use serde_json::{json, Value};

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
///
/// Legacy per-variable form: honoured only when [`LAUNCH_CONFIG_ENV`] is absent.
pub const EXTRA_RW_ENV: &str = "AGALMA_HARNESS_EXTRA_RW";
/// Environment variable carrying the explicit worker launch configuration (JSON)
/// across the trusted conductor→server boundary (`agalma-tqf`).
///
/// The process transport serializes the caller-supplied launch fields into this
/// variable; the server reconstructs an identical [`OpenCodeHarnessConfig`] from
/// it, so the confined sandbox `LaunchSpec` matches the in-process transport.
/// Only this explicit, conductor-built configuration crosses the boundary — the
/// parent environment and real provider credentials are never forwarded.
pub const LAUNCH_CONFIG_ENV: &str = "AGALMA_HARNESS_LAUNCH_CONFIG";

/// The explicit launch fields transferred across the process boundary.
///
/// Mirrors exactly the values the conductor hands [`build_config`]: the profile
/// path, the four sandbox roots, the extra read-only/read-write roots, and the
/// child environment. The server reconstructs its config from these verbatim, so
/// a custom `bd` PATH addition, toolchain homes, `CARGO_HOME`, and read roots
/// survive the transport unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchConfig {
    /// Rendered, fail-closed sandbox profile.
    pub profile_path: PathBuf,
    /// Attempt-owned read/write root (Seatbelt `ATTEMPT`).
    pub attempt_dir: PathBuf,
    /// Protected-tests root (Seatbelt `PROTECTED`).
    pub protected_dir: PathBuf,
    /// Parent-owned Unix-socket directory (Seatbelt `SOCKDIR`).
    pub sock_dir: PathBuf,
    /// Resolved model for the session.
    pub model: String,
    /// Parent-owned forward-proxy URL (provider egress).
    pub proxy_url: Option<String>,
    /// Whether the paid provider path is active.
    pub paid_proxy: bool,
    /// Explicit opencode binary; `None` resolves `OPENCODE_BIN` then `PATH`.
    pub program: Option<PathBuf>,
    /// Explicit opencode install root (Seatbelt `EXTRA_RO`); normally `None`.
    pub extra_ro: Option<PathBuf>,
    /// Extra read-only roots for the confined worker.
    pub extra_ro_roots: Vec<PathBuf>,
    /// Extra read/write roots (at most one honoured), e.g. the target repo.
    pub extra_rw_roots: Vec<PathBuf>,
    /// Additional child environment (toolchain homes, PATH additions).
    pub extra_env: BTreeMap<String, String>,
}

impl LaunchConfig {
    /// Serialize to the compact JSON carried in [`LAUNCH_CONFIG_ENV`].
    pub fn to_json(&self) -> String {
        let roots = |roots: &[PathBuf]| {
            Value::Array(
                roots
                    .iter()
                    .map(|root| Value::String(root.to_string_lossy().into_owned()))
                    .collect(),
            )
        };
        json!({
            "profile_path": self.profile_path.to_string_lossy(),
            "attempt_dir": self.attempt_dir.to_string_lossy(),
            "protected_dir": self.protected_dir.to_string_lossy(),
            "sock_dir": self.sock_dir.to_string_lossy(),
            "model": &self.model,
            "proxy_url": &self.proxy_url,
            "paid_proxy": self.paid_proxy,
            "program": self
                .program
                .as_ref()
                .map(|program| program.to_string_lossy().into_owned()),
            "extra_ro": self
                .extra_ro
                .as_ref()
                .map(|root| root.to_string_lossy().into_owned()),
            "extra_ro_roots": roots(&self.extra_ro_roots),
            "extra_rw_roots": roots(&self.extra_rw_roots),
            "extra_env": &self.extra_env,
        })
        .to_string()
    }

    /// Parse the JSON carried in [`LAUNCH_CONFIG_ENV`]; `None` when malformed.
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let string = |key: &str| value.get(key)?.as_str().map(str::to_string);
        let roots = |key: &str| -> Vec<PathBuf> {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(LaunchConfig {
            profile_path: PathBuf::from(string("profile_path")?),
            attempt_dir: PathBuf::from(string("attempt_dir")?),
            protected_dir: PathBuf::from(string("protected_dir")?),
            sock_dir: PathBuf::from(string("sock_dir")?),
            model: string("model")?,
            proxy_url: value
                .get("proxy_url")
                .and_then(Value::as_str)
                .map(str::to_string),
            paid_proxy: value
                .get("paid_proxy")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            program: value
                .get("program")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            extra_ro: value
                .get("extra_ro")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            extra_ro_roots: roots("extra_ro_roots"),
            extra_rw_roots: roots("extra_rw_roots"),
            extra_env: value
                .get("extra_env")
                .and_then(Value::as_object)
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Reconstruct a harness config, applying the shared paid-provider rule.
    ///
    /// `build_config` (not a raw field copy) is used so the dummy-key rule stays
    /// single-sourced; the conductor already ran the same rule, so this is an
    /// identity for a conductor-built config.
    pub fn to_config(&self) -> OpenCodeHarnessConfig {
        let mut config = build_config(OpenCodeConfigInit {
            profile_path: self.profile_path.clone(),
            attempt_dir: self.attempt_dir.clone(),
            protected_dir: self.protected_dir.clone(),
            sock_dir: self.sock_dir.clone(),
            model: self.model.clone(),
            proxy_url: self.proxy_url.clone(),
            paid_proxy: self.paid_proxy,
            extra_ro_roots: self.extra_ro_roots.clone(),
            extra_rw_roots: self.extra_rw_roots.clone(),
            extra_env: self.extra_env.clone(),
        });
        config.extra_ro = self.extra_ro.clone();
        config.program = self.program.clone();
        config
    }
}

/// Launch spec for the OpenCode server.
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Assemble the server's harness config from the process environment.
pub fn config_from_env(root: PathBuf) -> OpenCodeHarnessConfig {
    config_from_lookup(root, |key| std::env::var(key).ok())
}

/// Assemble the server's harness config, reading each variable through `lookup`.
///
/// Split from [`config_from_env`] so reconstruction is testable without mutating
/// the process environment. When the conductor transferred a [`LaunchConfig`] in
/// [`LAUNCH_CONFIG_ENV`], it is used verbatim (the same fields the in-process
/// transport would use); otherwise the legacy per-variable reconstruction derives
/// a root-scoped config (a run-local `CARGO_HOME`/`CARGO_TARGET_DIR` and a single
/// [`EXTRA_RW_ENV`] root).
pub fn config_from_lookup(
    root: PathBuf,
    lookup: impl Fn(&str) -> Option<String>,
) -> OpenCodeHarnessConfig {
    if let Some(launch) = lookup(LAUNCH_CONFIG_ENV).and_then(|json| LaunchConfig::from_json(&json))
    {
        return launch.to_config();
    }
    let profile_path = lookup(PROFILE_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("worker.sb"));
    let proxy_url = lookup(PROXY_URL_ENV).filter(|url| !url.is_empty());
    let paid_proxy = matches!(lookup(PAID_ENV).as_deref(), Some("1") | Some("true"));
    let model = lookup("AGALMA_MODEL").unwrap_or_default();
    let mut extra_env = BTreeMap::new();
    extra_env.insert(
        "CARGO_HOME".to_string(),
        root.join("cargo-home").to_string_lossy().into_owned(),
    );
    extra_env.insert(
        "CARGO_TARGET_DIR".to_string(),
        root.join("agent-target").to_string_lossy().into_owned(),
    );
    let extra_rw_roots = lookup(EXTRA_RW_ENV)
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
    let server = build_server(Box::new(sandbox), config);
    let wire = Wire::from_env(Wire::Framed);
    let display = socket.display().to_string();
    agalma_ipc::serve_harness(server, &socket, wire).map_err(|e| format!("serve {display}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::DUMMY_PROVIDER_KEY;

    #[test]
    fn command_names_the_opencode_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
    }

    /// The transferred launch config must survive JSON round-tripping, including
    /// paths containing spaces (toolchain/bd roots on real hosts often do).
    #[test]
    fn launch_config_json_round_trips_paths_with_spaces() {
        let launch = LaunchConfig {
            profile_path: PathBuf::from("/state/run dir/worker.sb"),
            attempt_dir: PathBuf::from("/state/run dir/attempt"),
            protected_dir: PathBuf::from("/state/run dir/protected"),
            sock_dir: PathBuf::from("/state/run dir/sock"),
            model: "opencode-go/deepseek".to_string(),
            proxy_url: Some("http://127.0.0.1:41234".to_string()),
            paid_proxy: true,
            program: None,
            extra_ro: None,
            extra_ro_roots: vec![
                PathBuf::from("/Users/a/.local/bin"),
                PathBuf::from("/read only/tool chain"),
            ],
            extra_rw_roots: vec![PathBuf::from("/repo with spaces")],
            extra_env: BTreeMap::from([
                (
                    "PATH".to_string(),
                    "/Users/a/.local/bin:/tool chain/bin:/usr/bin:/bin".to_string(),
                ),
                ("CARGO_HOME".to_string(), "/state/cargo home".to_string()),
            ]),
        };
        let json = launch.to_json();
        let decoded = LaunchConfig::from_json(&json).expect("valid launch config json");
        assert_eq!(decoded, launch);
        // Malformed input is rejected, not silently defaulted.
        assert!(LaunchConfig::from_json("not json").is_none());
        assert!(LaunchConfig::from_json("{}").is_none());
    }

    /// A transferred launch config is reconstructed verbatim: custom bd PATH,
    /// read roots, and `CARGO_HOME` survive, while unrelated parent environment
    /// keys are never consulted.
    #[test]
    fn forwarded_launch_config_is_used_verbatim_and_drops_unrelated_env() {
        let launch = LaunchConfig {
            profile_path: PathBuf::from("/state/run/worker.sb"),
            attempt_dir: PathBuf::from("/state/run/attempt"),
            protected_dir: PathBuf::from("/state/run/protected"),
            sock_dir: PathBuf::from("/state/run/sock"),
            model: "opencode-go/deepseek".to_string(),
            proxy_url: Some("http://127.0.0.1:41234".to_string()),
            paid_proxy: true,
            program: None,
            extra_ro: None,
            extra_ro_roots: vec![PathBuf::from("/Users/a/.local/bin")],
            extra_rw_roots: vec![PathBuf::from("/repo")],
            extra_env: BTreeMap::from([
                (
                    "PATH".to_string(),
                    "/Users/a/.local/bin:/cargo home/bin:/usr/bin:/bin".to_string(),
                ),
                (
                    "CARGO_HOME".to_string(),
                    "/state/run/cargo-home".to_string(),
                ),
                ("RUSTUP_HOME".to_string(), "/Users/a/.rustup".to_string()),
            ]),
        };
        // The lookup carries a parent secret that must never reach the config.
        let lookup = |key: &str| -> Option<String> {
            match key {
                LAUNCH_CONFIG_ENV => Some(launch.to_json()),
                "AWS_SECRET_ACCESS_KEY" => Some("super-secret".to_string()),
                "OPENCODE_API_KEY" => Some("real-provider-token".to_string()),
                _ => None,
            }
        };
        let config = config_from_lookup(PathBuf::from("/state/run"), lookup);

        assert_eq!(config.profile_path, launch.profile_path);
        assert_eq!(config.attempt_dir, launch.attempt_dir);
        assert_eq!(config.protected_dir, launch.protected_dir);
        assert_eq!(config.sock_dir, launch.sock_dir);
        assert_eq!(config.extra_ro_roots, launch.extra_ro_roots);
        assert_eq!(config.extra_rw_roots, launch.extra_rw_roots);
        assert_eq!(config.extra_env, launch.extra_env);
        assert_eq!(
            config.extra_env.get("PATH").map(String::as_str),
            Some("/Users/a/.local/bin:/cargo home/bin:/usr/bin:/bin")
        );
        assert_eq!(
            config.extra_env.get("CARGO_HOME").map(String::as_str),
            Some("/state/run/cargo-home")
        );
        // The legacy root-scoped fallbacks must not leak into a transferred config.
        assert!(!config.extra_env.contains_key("CARGO_TARGET_DIR"));
        // Parent environment is never merged in.
        assert!(!config.extra_env.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!config.extra_env.contains_key("OPENCODE_API_KEY"));
        // The paid-provider dummy-key rule still applies on reconstruction.
        assert_eq!(
            config.provider_base_url.as_deref(),
            Some("http://127.0.0.1:41234")
        );
        assert_eq!(config.provider_api_key.as_deref(), Some(DUMMY_PROVIDER_KEY));
    }

    /// Without a transferred launch config the server keeps the legacy
    /// root-scoped reconstruction (direct invocation / older conductors).
    #[test]
    fn absent_launch_config_falls_back_to_root_scoped_reconstruction() {
        let lookup = |key: &str| -> Option<String> {
            match key {
                EXTRA_RW_ENV => Some("/repo".to_string()),
                "AGALMA_MODEL" => Some("opencode/mimo".to_string()),
                _ => None,
            }
        };
        let config = config_from_lookup(PathBuf::from("/state/run"), lookup);
        assert_eq!(
            config.extra_env.get("CARGO_HOME").map(String::as_str),
            Some("/state/run/cargo-home")
        );
        assert_eq!(
            config.extra_env.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/state/run/agent-target")
        );
        assert_eq!(config.extra_rw_roots, vec![PathBuf::from("/repo")]);
        assert!(config.extra_ro_roots.is_empty());
        assert_eq!(config.attempt_dir, PathBuf::from("/state/run/attempt"));
    }
}
