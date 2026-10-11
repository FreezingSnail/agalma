# M0 dependency policy

Task: `agalma-4ui.2` (M0-D2). Status: approved. User approved the pinned
shortlist below; anything outside it requires explicit approval before fetch.

Rule: no downloads beyond this list. If a build requires another crate, stop and
report — do not add it.

## Approved shortlist (pinned)

| Crate | Version | Purpose | Features |
|---|---|---|---|
| `tokio` | 1.53.2 | async runtime, process, net, time, sync | `rt-multi-thread`, `macros`, `process`, `net`, `time`, `sync`, `signal`, `io-util` |
| `serde` | 1.0.229 | serialization derives for contract types | `derive` |
| `serde_json` | 1.0.151 | canonical JSON encoding | default |
| `rusqlite` | 0.40.2 | ledger SQLite access | `bundled` (pinned SQLite, no system variance) |
| `thiserror` | 2.0.21 | typed errors in contracts/impls | default |
| `clap` | 4.6.7 | conductor CLI | `derive` |

Pinning: exact `=` versions in the workspace `Cargo.toml`
(`[workspace.dependencies]`), `Cargo.lock` committed. `cargo update` is a
reviewed change. New transitive dependencies arriving through these crates stay
inside the approval; new direct crates do not.

## Rationale

- `tokio`: the OpenCode adapter is async (HTTP + SSE + process supervision);
  one runtime avoids a sync/async bridge.
- `serde`/`serde_json`: canonical Agalma schemas are JSON on the wire and in
  the ledger; the S0d std-only reader was spike scaffolding only.
- `rusqlite` `bundled`: deterministic SQLite version independent of the system
  library; matches the S0c-pinned 3.51 line at whatever the crate bundles —
  record the actual bundled version in `meta` at first migration and in M0.8
  evidence.
- `thiserror`: contract error taxonomy (`KnownFailure`, `UnsupportedCapability`,
  `Conflict`, `UnknownOutcome`).
- `clap`: `agalma run|resume|status`; hand-rolling CLI parsing buys nothing.

## Later approved additions

Approved by the user on 2026-10-11 for the TUI epic (`agalma-3kr`):

| Crate | Version | Purpose | Features |
|---|---|---|---|
| `ratatui` | 0.30.2 | full-screen TUI widgets/layout (design: docs/tui-design.md) | default |
| `crossterm` | 0.29.0 | terminal raw mode, input events, backend for ratatui | default |

Pinned `=` like the shortlist; `agalma-conductor` only.

Approved by the user on 2026-10-11 for the IPC v2 epic (`agalma-ebg`; design:
`docs/ipc-connect-design.md`):

| Crate | Version | Purpose | Features |
|---|---|---|---|
| `connectrpc` | 0.9.1 | ConnectRPC runtime for the nano-service transport (Connect protocol over HTTP/2 on a Unix socket) | `default-features = false`, `server`, `client` |
| `buffa` | 0.9.2 | generated protobuf messages for the IPC envelope | default (`std`, `fast-utf8`) |
| `connectrpc-build` | 0.9.0 | build.rs codegen for `agalma-ipc/proto/ipc.proto` (build-dependency) | default |

Pinned `=`; `agalma-ipc` only. The Connect stack's transitives (hyper,
hyper-util, tower, tower-http, futures, http, http-body(-util), bytes,
tokio-util, tracing, async-trait, base64, percent-encoding, pin-project,
sync_wrapper, libc) arrive through these approved crates and stay inside the
approval by the rule above. No TLS, compression, or protobuf-JSON features.

## Excluded on purpose

- No HTTP client crate (reqwest/ureq/hyper): the OpenCode adapter's hand-rolled
  `TcpStream` client from S0d is retained; revisit after M0 measurement. The
  IPC v2 stack pulls hyper in transitively, confined to `agalma-ipc` and not
  used by the adapter.
- No git library (git2): `git` CLI via `std::process` stays private to
  `agalma-workspace`.
- No logging/tracing crate yet: `eprintln` + ledger records; revisit at M3.
- No uuid/ulid crate: IDs are derived (`exec:`, `op:`) or counter-based inside
  the state dir.

## Verification

- `cargo build --workspace --offline` after the first fetch, then every bead
  builds offline (`--offline`) to prove no hidden fetch.
- `grep` audit of `Cargo.lock` for new direct deps vs this table in M0.8.
