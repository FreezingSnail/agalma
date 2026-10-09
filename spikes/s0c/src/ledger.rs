//! Ledger: physical SQLite access, isolated behind this module.
//!
//! The spike deliberately uses the installed `sqlite3` CLI (3.51.0) through
//! `std::process::Command` instead of a Rust driver. No crate is used; the
//! project is std-only and builds offline.
//!
//! Durability: WAL journalling with `synchronous=FULL`; every logical
//! transition commits state + event + operation receipt + dispatch intent in
//! one `BEGIN IMMEDIATE ... COMMIT` script. No external I/O ever runs while a
//! transaction is open.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Persisted physical schema version. Bump only with a migration.
pub const SCHEMA_VERSION: i64 = 1;
/// Persisted execution-definition version. Recovery must understand this or park.
pub const EXECUTION_VERSION: i64 = 1;

/// Quote a text literal for SQLite.
pub fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quote an integer literal.
pub fn qi(n: i64) -> String {
    n.to_string()
}

/// Wrap statements in one immediate transaction with durability pragmas.
pub fn atomic(stmts: &[String]) -> String {
    let mut s = String::from(
        "PRAGMA busy_timeout=5000;\nPRAGMA synchronous=FULL;\nBEGIN IMMEDIATE;\n",
    );
    for st in stmts {
        s.push_str(st);
        if !st.trim_end().ends_with(';') {
            s.push(';');
        }
        s.push('\n');
    }
    s.push_str("COMMIT;\n");
    s
}

pub struct Db {
    pub path: PathBuf,
}

impl Db {
    pub fn new(path: PathBuf) -> Self {
        Db { path }
    }

    /// Execute a multi-statement script delivered on stdin.
    pub fn script(&self, sql: &str) -> Result<String, String> {
        let mut child = Command::new("sqlite3")
            .arg("-bail")
            .arg(&self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn sqlite3: {e}"))?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(sql.as_bytes())
            .map_err(|e| format!("write sqlite3 stdin: {e}"))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("wait sqlite3: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "sqlite3 exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Run one query and return trimmed stdout (empty when no rows).
    pub fn query(&self, sql: &str) -> Result<String, String> {
        let out = Command::new("sqlite3")
            .arg("-bail")
            .arg("-noheader")
            .arg(&self.path)
            .arg(sql)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("spawn sqlite3: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "sqlite3 exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Return the first row's raw `|`-separated line, or None.
    pub fn row(&self, sql: &str) -> Result<Option<String>, String> {
        let out = self.query(sql)?;
        let line = out.lines().next().unwrap_or("").trim().to_string();
        if line.is_empty() {
            Ok(None)
        } else {
            Ok(Some(line))
        }
    }

    /// Return the first column as a scalar string.
    pub fn scalar(&self, sql: &str) -> Result<Option<String>, String> {
        Ok(self.row(sql)?.map(|r| r.split('|').next().unwrap_or("").to_string()))
    }
}
