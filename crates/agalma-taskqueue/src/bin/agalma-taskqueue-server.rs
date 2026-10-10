//! Task-queue `TaskQueueApi` component server binary (B1.5).
//!
//! Usage: `agalma-taskqueue-server --socket <path> --repo <path> [--bd <program>]`.

fn main() {
    if let Err(err) = agalma_taskqueue::server::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-taskqueue-server: {err}");
        std::process::exit(1);
    }
}
