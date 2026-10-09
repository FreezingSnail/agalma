//! `agalma` binary entry point.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    agalma_conductor::run().await
}
