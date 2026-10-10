//! Shared test support for the B0 kernel recovery pack (B0.1 `agalma-9n4.2`).
//!
//! Included by integration tests with `mod support;`:
//!
//! - [`pack`] — protected fixture-pack integrity (SHA-256 + `MANIFEST.sha256`).
//! - [`harness`] — crash-injection hooks, fake-activity effect journal, and
//!   isolated-ledger helper under `target/test-runs/recovery-*`.
//!
//! The `harness` contract is frozen for B0.2+ recovery tests: add cases as new
//! point constants + test functions, but do not change existing semantics.

#![allow(dead_code)] // not every test binary consumes every helper

pub mod harness;
pub mod pack;
