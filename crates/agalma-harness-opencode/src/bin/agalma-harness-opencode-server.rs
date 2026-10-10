//! OpenCode `HarnessApi` component server binary (B1.2).
//!
//! Usage: `agalma-harness-opencode-server --socket <path>`. Configuration comes
//! from the environment (`AGALMA_HARNESS_ROOT`, `AGALMA_HARNESS_PROFILE_PATH`,
//! `AGALMA_HARNESS_PROXY_URL`, `AGALMA_HARNESS_PAID_PROXY`, `AGALMA_MODEL`).

fn main() {
    if let Err(err) = agalma_harness_opencode::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-harness-opencode-server: {err}");
        std::process::exit(1);
    }
}
