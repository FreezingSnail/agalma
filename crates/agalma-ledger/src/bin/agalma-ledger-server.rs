//! Ledger `LedgerApi` component server binary (B1.6).
//!
//! Usage: `agalma-ledger-server --socket <path> --state-dir <path>`.

fn main() {
    if let Err(err) = agalma_ledger::server::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-ledger-server: {err}");
        std::process::exit(1);
    }
}
