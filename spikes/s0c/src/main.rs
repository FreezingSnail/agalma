//! S0c spike entry point: `s0c <case1..case6>`.
//!
//! Each case runs in a parent ("recovering") role and, where a crash boundary
//! must be crossed, forks a child of this same binary that aborts at an
//! injection point selected by `SPIKE_CRASH_AT`. The parent then recovers from
//! the same database file. See docs/spikes/s0c-execution.md.

mod cases;
mod executor;
mod ledger;
mod util;

use std::process::Command;

use ledger::{EXECUTION_VERSION, SCHEMA_VERSION};
use util::Report;

fn sqlite_cli_version() -> String {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "<sqlite3 not found>".to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let case = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let is_child = std::env::var("SPIKE_CHILD").is_ok();

    eprintln!(
        "[s0c] sqlite3 CLI: {} | schema_version={} | execution_definition_version={}",
        sqlite_cli_version(),
        SCHEMA_VERSION,
        EXECUTION_VERSION
    );

    let result: Result<Option<Report>, String> = match (case, is_child) {
        ("case1", true) => cases::case1_child().map(|_| None),
        ("case1", false) => cases::case1_parent().map(Some),
        ("case2", _) => cases::case2().map(Some),
        ("case3", true) => cases::case3_child().map(|_| None),
        ("case3", false) => cases::case3_parent().map(Some),
        ("case4", true) => cases::case4_child().map(|_| None),
        ("case4", false) => cases::case4_parent().map(Some),
        ("case5", true) => cases::case5_child().map(|_| None),
        ("case5", false) => cases::case5_parent().map(Some),
        ("case6", _) => cases::case6().map(Some),
        _ => {
            eprintln!("usage: s0c <case1|case2|case3|case4|case5|case6>");
            std::process::exit(2);
        }
    };

    match result {
        Ok(None) => {
            // Child role completed without crashing. Parent treats a normal
            // exit as a failed injection and reports it.
            eprintln!("[s0c] child role finished without reaching its injection point");
        }
        Ok(Some(report)) => {
            let dir = util::case_dir(case);
            if let Err(e) = report.emit(&dir) {
                eprintln!("[s0c] failed to write evidence: {e}");
            }
            if !report.passed() {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("[s0c] case {case} ERROR: {e}");
            std::process::exit(1);
        }
    }
}
