//! Workspace `WorkspaceApi` component server binary (B1.4).
//!
//! Usage: `agalma-workspace-server --socket <path> --base-dir <path>`.

fn main() {
    if let Err(err) = agalma_workspace::server::run_from_args(std::env::args().skip(1)) {
        eprintln!("agalma-workspace-server: {err}");
        std::process::exit(1);
    }
}
