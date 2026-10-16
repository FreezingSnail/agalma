//! Kernel `ExecutionApi` component server binary (B1.7).
//!
//! Usage: `agalma-kernel-server --socket <path> --state-dir <path>` with
//! `AGALMA_KERNEL_LEDGER_SOCKET=<ledger socket>` (required). The process
//! lifecycle seam is an in-process local launcher; there is no sandbox socket.

fn main() {
    if let Err(err) = agalma_kernel::server::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-kernel-server: {err}");
        std::process::exit(1);
    }
}
