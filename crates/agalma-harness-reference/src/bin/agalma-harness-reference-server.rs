//! Reference `HarnessApi` component server binary (B1.2).
//!
//! Usage: `agalma-harness-reference-server --socket <path>`. The deterministic
//! profile comes from `AGALMA_HARNESS_PROFILE` (`success` | `red` | `fail`).

fn main() {
    if let Err(err) = agalma_harness_reference::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-harness-reference-server: {err}");
        std::process::exit(1);
    }
}
