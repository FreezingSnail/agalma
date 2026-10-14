# IPCX — ConnectRPC transport for nano services (design)

Bead: `agalma-ebg.1` (IPCX-D1). Epic: `agalma-ebg`. Status: design;
orchestrator-authored; gates all IPC v2 beads. Refs:
`docs/component-contracts.md` (§Boundaries and local deployment, §Shared
contract rules, §Runtime replacement), `docs/b1-extraction.md` (recipe,
evidence, D1 superseded), `docs/m0-dependencies.md`, `docs/tui-design.md`
(live transport), `agalma-www` (head-of-line), `agalma-5de` (execution gap),
`agalma-rvj`/`agalma-ud1`/`agalma-fez` (default flips).

## Goal

Replace the byte transport under the already-extracted Tier-1 seams with
ConnectRPC ([connectrpc](https://github.com/connectrpc/connect-rust)) over Unix
domain sockets, envelope-first: the Agalma contract schemas (`agalma-contracts`,
serde JSON) and the ledger's persisted JSON shapes stay unchanged. In-process
bindings stay unchanged. Each boundary migrates one at a time with the B1
recipe — dual-binding parity, staged rebind, rollback, evidence.

Rationale (the review that selected this): connection multiplexing
(`agalma-www`), standard deadlines/cancellation plumbing, typed RPC
interceptors for binding pins and caller identity, server streaming for future
event push, health/reflection tooling, and a conformance-tested protocol instead
of hand-rolled framing. Proto-first schema migration was rejected for now:
contracts are frozen and the wire JSON is also the ledger's at-rest format.

## Decisions

### D1 — Transport: ConnectRPC envelope over a Unix socket

- Wire: HTTP/2 over UDS. Client: `Http2ConnectionBuilder::connect_unix(path,
  authority)` (`connectrpc::client`, `client` feature). Server: custom accept
  loop — accept on a `tokio::net::UnixListener` and hand each connection to
  `Server::serve_connection` (`connectrpc::server`; documented path for "listen
  on a Unix socket"). No TCP listener, no network server per component.
- Protocol: Connect (not gRPC) on the client (`Protocol::Connect`); the server
  router answers all three ConnectRPC protocols by default.
- Envelope (`crates/agalma-ipc/proto/ipc.proto`), payloads stay serde JSON:

```proto
syntax = "proto3";
package agalma.ipc.v1;

message IpcError { string code = 1; string message = 2; }

message IpcRequest {
  uint32 v = 1;            // protocol version (1)
  uint64 id = 2;           // client-assigned; replay key
  string method = 3;       // snake_case method name
  bytes params_json = 4;   // serde_json encoding of params
}

message IpcResponse {
  uint32 v = 1;
  uint64 id = 2;           // echo
  optional bytes result_json = 3; // present even for JSON null
  optional IpcError error = 4;
}

service Ipc {
  rpc Call(IpcRequest) returns (IpcResponse);
  // Reserved for later event push (server streaming); no Tier-1 seam needs it
  // for parity:
  // rpc Stream(IpcRequest) returns (stream IpcResponse);
}
```

- `result_json` presence preserves the current result-vs-null distinction (JSON
  `null` is a present field containing `null`; an error response omits it).
- Envelope semantics preserved verbatim: duplicate `id` returns the recorded
  receipt/result; unknown-outcome surfaces as `UnknownOutcome` through
  `IpcError.code`; missing socket maps to `Unavailable`. `IpcError.code`
  carries the frozen `ContractError` variant names; the Connect error path is
  reserved for protocol/transport faults (see D5).
- Concurrency: HTTP/2 multiplexing removes the one-connection-one-call
  constraint (`agalma-www`). Server dispatch keeps the current ordering
  guarantee — one mutating call at a time per boundary (a per-server guard);
  read-side concurrency is a per-seam decision recorded in the boundary's
  evidence. A fresh `id` never runs concurrent with its own replay window.
- Lifetime: sockets stay directly under the run state dir (`<state_dir>/<name>.sock`)
  to respect the Unix path-length limit noted in the B1 evidence.

### D2 — Codegen and dependency pins

- `ipc.proto` lives in `agalma-ipc`; code is generated in `build.rs` with
  `connectrpc-build` (no extra binaries, regenerates on change, offline after
  the first fetch). `pub mod proto { connectrpc::include_generated!(); }`.
  Fallback only if a blocker appears: checked-in `buf generate` output with the
  pinned plugins (`protoc-gen-buffa`, `protoc-gen-connect-rust`,
  `protoc-gen-buffa-packaging`) and a documented `scripts/` regen — plugin
  binaries are downloaded code and require explicit user approval.
- Pins (exact `=`): `connectrpc = 0.9.1`, `buffa = 0.9.2`,
  `connectrpc-build = 0.9.0` (build-dependency, `agalma-ipc` only).
  Features: `connectrpc` `default-features = false`, features `server`,
  `client` — no TLS, no `gzip`/`zstd`, and no protobuf-JSON codec (`json`
  refers to proto3-JSON; Agalma's JSON rides in `bytes`). `buffa-types` only
  if well-known types ever appear. MSRV 1.88 ≤ workspace toolchain (1.97.1);
  edition 2024 alongside the workspace's 2021 is fine.
- Transitives arriving with the approved direct crates (hyper, hyper-util,
  tower, tower-http, futures, http, bytes, tracing, …) stay inside the policy's
  approval; rows added to `docs/m0-dependencies.md`. `agalma-ipc` is the only
  product crate depending on the Connect stack (architecture test addition).
- Builds stay offline: codegen reads a local `.proto`; `cargo build --offline`
  after the one-time fetch, as in M0.

### D3 — Process model unchanged

Each implementation crate keeps its server binary, `CommandSpec`, stdout
`READY {json}` line, and supervised launch (`SandboxChild`). The bind gate
(`plan_from_describe` semantics) is unchanged. Clients in `agalma-ipc`
implement the same contract traits over the transport. Readiness does not wait
for anything new.

### D4 — Shadow parity methodology (unchanged from B1 D4)

Per boundary: run the scenario against identical scratch state under in-process
and Connect bindings; normalized transcript (event kinds/payloads, receipts,
git digest, ledger snapshot, `Describe`); structural-difference checker;
artifacts under `target/test-runs/ipcx/<boundary>/<run>/`; evidence doc
`docs/ipcx-<boundary>-evidence.md` structured like the B1 docs.

### D5 — Error mapping and receipts

- In-band authoritative: `IpcError.code` = `ContractError` variant name
  (`KnownFailure`, `UnsupportedCapability`, `Conflict`, `UnknownOutcome`) or
  `Unavailable`/`Protocol` for transport faults, exactly as `agalma-ipc` does
  today. No lossy translation to the 16 Connect codes on the primary path.
- Protocol-level mapping, documented for tooling only: transport failure →
  `Unavailable`; deadline → `DeadlineExceeded`; decode failure → `Internal`.
- Duplicate delivery, receipt replay, changed-input rejection, and
  `UnknownOutcome` reconciliation remain as implemented in `agalma-ipc`
  (`envelope.rs`, `server.rs`); the envelope gains no new semantics.

### D6 — Consumer invariance and architecture tests

- Allowed diffs per boundary bead: `ipc.proto`/generated module, Connect
  client/server plumbing in `agalma-ipc`, server bin wiring in the impl crate,
  composition/construction sites, new tests, evidence docs. `git diff
  --name-only` is part of the evidence; conductor phase/work logic unchanged.
- Architecture test: keep the impl→impl Cargo edge check; add: only
  `agalma-ipc` lists `connectrpc`/`buffa` as a normal dependency; extracted
  servers still do not import conductor internals.

### D7 — Migration order (mirror B1)

| # | Boundary | Why this position |
|---|---|---|
| 1 | `HarnessApi` | first in B1; smallest full-vertical slice to prove Connect/UDS end to end |
| 2 | `SandboxApi` | serves every confined launch; harness server switches here when needed |
| 3 | `WorkspaceApi` | small surface, single consumer |
| 4 | `TaskQueueApi` | small surface, single consumer; `agalma-www` already closed by foundation |
| 5 | `LedgerApi` | durable state: quiesced export/import via files preserved |
| 6 | `ExecutionApi` | largest boundary, last; folds in the `agalma-5de` handler-registry resolution |

Extraction stays strictly one boundary per bead, chained by `blocks` deps.
Current defaults after `agalma-ud1`/`agalma-fez`: harness, ledger, workspace and
task queue use process transport; sandbox always uses a process. Execution keeps
its in-process default pending production integration (`agalma-pob.1`). The
`AGALMA_*_TRANSPORT=in-process` override retains direct embedding, while
`AGALMA_IPC_WIRE=framed` selects framed JSON for process-wire parity. Process
bindings otherwise use Connect. Evidence: `docs/b1-f3-evidence.md` §§5–6.

## Bead plan

| Bead | Scope |
|---|---|
| `agalma-ebg.1` | IPCX-D1 design (this doc) — gates all IPC v2 beads |
| `agalma-ebg.2` | IPCX.1 foundation: `ipc.proto`, Connect stack + pins, UDS accept loop, client core, Connect-level conformance harness, architecture-test additions; closes `agalma-www` |
| `agalma-ebg.3` | IPCX.2 Harness boundary |
| `agalma-ebg.4` | IPCX.3 Sandbox boundary |
| `agalma-ebg.5` | IPCX.4 Workspace boundary |
| `agalma-ebg.6` | IPCX.5 TaskQueue boundary |
| `agalma-ebg.7` | IPCX.6 Ledger boundary |
| `agalma-ebg.8` | IPCX.7 Execution/kernel boundary |
| `agalma-ebg.9` | IPCX.8 exit evidence: component map on Connect, epic close |

## Risks / open items

- **Pre-1.0 churn.** `connectrpc` 0.9.x may break in 0.x; exact pins and a
  reviewed `cargo update` policy (M0 rule) hold the line.
- **Codegen offline policy.** `connectrpc-build` compiles `.proto` at build
  time with no external binaries; the buf fallback would add downloaded plugin
  binaries and needs explicit approval.
- **Concurrency semantics.** Multiplexing changes failure interleavings; the
  D1 guard preserves ordering, and parity checkers normalize timing. Per-seam
  read-concurrency decisions go in the boundary evidence.
- **Transitive dependency surface.** First non-hand-rolled HTTP stack in the
  workspace; the D2 architecture check keeps it confined to `agalma-ipc`, and
  the M0.8 `Cargo.lock` audit applies.
- **`ExecutionApi` adapter gap** (`agalma-5de`) is inherited, not solved, until
  IPCX.7.
- **TUI.** `docs/tui-design.md` v1 stays a polling refresh loop; Connect
  server streaming is the enabling step if push is later wanted.

## Review outcome (docs touched)

- `docs/component-contracts.md` — transport sentence now points at this doc.
- `docs/m0-dependencies.md` — approved additions: connectrpc/buffa/
  connectrpc-build.
- `docs/tui-design.md` — live-transport paragraph notes multiplexing and the
  reserved streaming option; v1 polling unchanged.
- `docs/b1-extraction.md` remains historical; its D1 is superseded for
  transport only, its recipe and evidence method stay in force.
