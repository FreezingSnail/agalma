//! Sandbox `SandboxApi` component server binary (B1.3).
//!
//! Usage: `agalma-sandbox-server --socket <path>`.

fn main() {
    if let Err(err) = agalma_sandbox::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-sandbox-server: {err}");
        std::process::exit(1);
    }
}
