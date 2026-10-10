//! Kernel `ExecutionApi` component server binary (B1.7).
//!
//! Usage: `agalma-kernel-server --socket <path> --state-dir <path>` with
//! `AGALMA_KERNEL_LEDGER_SOCKET=<ledger socket>` (required) and
//! `AGALMA_KERNEL_SANDBOX_SOCKET=<sandbox socket>` (optional).

fn main() {
    if let Err(err) = agalma_kernel::server::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-kernel-server: {err}");
        std::process::exit(1);
    }
}
