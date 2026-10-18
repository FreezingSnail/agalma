//! `agalma-web-dashboard` executable entry point.
//!
//! Required: `--socket <plugin-rpc-socket>` (the HP.3 launch contract). The
//! installed config path/digest and hub read binding are private launch inputs.
//! An invalid or mismatched pin, foreign bind address, or occupied port exits
//! non-zero before the process announces READY.

use std::process::ExitCode;

use agalma_web_dashboard::config::resolve_installed_config;
use agalma_web_dashboard::server::{run, LaunchInputs};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match launch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agalma-web-dashboard: {error}");
            ExitCode::from(2)
        }
    }
}

fn launch(args: &[String]) -> Result<(), String> {
    let socket = agalma_ipc::socket_from_args(args.iter().cloned())?;
    let launch_inputs = LaunchInputs::from_env();
    launch_inputs
        .validate_installed_config_pin()
        .map_err(|_| "missing or invalid private installed configuration pin".to_string())?;
    let config = resolve_installed_config(
        launch_inputs
            .config_path
            .as_deref()
            .expect("validated config path"),
        &launch_inputs.config_digest,
    )
    .map_err(|error| error.to_string())?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| format!("tokio runtime: {error}"))?;
    runtime
        .block_on(run(socket, config, launch_inputs))
        .map_err(|error| error.to_string())
}
