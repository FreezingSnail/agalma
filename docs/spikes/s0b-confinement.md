# S0b — Seatbelt confinement spike results

Task: agalma-7v9. Status: **complete**. Timeboxed discovery spike; kill/fallback
decision at exit.

Architecture refs: `docs/architecture.md` §11 (Safety and governance), §13 S0b.

Environment recorded:

| Field | Value |
|---|---|
| macOS | 26.6.2 (build 25G83) |
| `sandbox-exec` | `/usr/bin/sandbox-exec` (present, universal x86_64/arm64e) |
| shell | `/bin/sh` (bash 3.2) |
| node | `/opt/homebrew/bin/node` |
| opencode | v2.0.15 (`/Users/connorfranc/.local/bin/opencode` → `~/code/opencode/packages/cli/dist/cli-darwin-arm64/bin/opencode`) |
| repo HEAD at run | `776e7e2` |
| defining run dir | `spikes/s0b/runs/20261009T171146Z` (gitignored) |

---

## Purpose

Prove that macOS Seatbelt (`sandbox-exec`) can confine a future harness worker
and **all of its descendants**, failing closed, so that unattended execution is
safe on macOS without a container. Specifically prove the §11 boundary: writes
only to attempt-owned dirs; main checkout, ledger/state, credentials, and Git
metadata inaccessible; protected tests readable but not writable; loopback HTTP
to a parent-owned auth proxy and the nerve Unix socket allowed; arbitrary
network denied; sandbox init fails closed.

## Exit criteria

- [x] Built-in file-tool access pattern confined (attempt dirs R/W; main checkout, ledger/state, protected files denied)
- [x] Shell descendants inherit confinement (child + grandchild + depth-6)
- [x] Build scripts (shell + real `clang` compile) confined
- [x] Shared Git metadata (main repo `.git`) inaccessible
- [x] Credentials (`~/.ssh`, `~/.config`, copied `auth.json`) denied reads
- [x] Protected tests readable, not writable
- [x] Loopback HTTP to parent-owned server allowed (proxy + worker bind)
- [x] Nerve Unix domain socket allowed; off-path socket denied
- [x] Arbitrary external network denied
- [x] Sandbox init fails closed (malformed/missing/empty/undefined-param profiles)
- [x] Provider auth proxy and narrow conductor bridge validated
- [x] Idle memory/CPU and shutdown measured
- [x] Verdict recorded (seatbelt acceptable / container required)

All 59 assertions PASS, 0 FAIL (see `spikes/s0b/runs/20261009T171146Z/logs/`).

## Procedure

Permanent artifacts live only under `spikes/s0b/`:

```
spikes/s0b/
├── profiles/
│   ├── worker.sb            # deny-default confine profile (parametrised)
│   ├── malformed.sb         # fail-closed fixture: syntax error
│   ├── undefined-param.sb   # fail-closed fixture: unbound param
│   └── empty.sb             # fail-closed fixture: empty (created by p05)
└── probes/
    ├── run.sh               # sandbox-exec launcher + param substitution
    ├── all.sh               # fresh run dir + run all probes + summary
    ├── p01-file-access.sh   # rows 1/4/5/6
    ├── p02-descendants.sh   # row 2
    ├── p03-build.sh         # row 3
    ├── p04-network.sh       # rows 7/8/9
    ├── p05-failclosed.sh    # row 10
    ├── p06-resources.sh     # idle resource + shutdown
    ├── p07-authproxy-bridge.sh  # acceptance: auth proxy + nerve bridge
    ├── p08-opencode-seatbelt.sh # acceptance: opencode under seatbelt
    ├── net-http.js          # parent-owned loopback HTTP server (auth proxy)
    └── net-uds.js           # parent-owned Unix socket server (nerve bridge)
```

Reproduce everything (creates a fresh `runs/<UTC>` and prints a summary):

```
spikes/s0b/probes/all.sh
```

The launcher wraps the profile with resolved absolute params (always supplied, so
an unsubstituted param fails closed):

```
spikes/s0b/probes/run.sh <attempt-dir> <protected-dir> <sockdir> <cmd...>
# expands to:
/usr/bin/sandbox-exec -f spikes/s0b/profiles/worker.sb \
  -D ATTEMPT=<abs> -D PROTECTED=<abs> -D SOCKDIR=<abs> -D EXTRA_RO=<abs> <cmd>
```

The profile is deny-by-default (`(deny default)`) plus explicit read-only system/
dyld/toolchain paths, read/write only inside `ATTEMPT`, read-only `PROTECTED`,
loopback + one Unix-socket subtree, and `mach-lookup`/`sysctl-read`. It is
deliberately minimal; see Deviations for the two broad allowances.

## Allow / deny matrix

`R=read`, `W=write`, `ALLOW`=command succeeds, `DENY`=“Operation not permitted”.
Absolute paths abbreviated: `<A>`=attempt dir, `<STATE>`=ledger/state dir,
`<REPO>`=main checkout, `<PROT>`=protected dir, `<SOCK>`=socket dir.

### Row 1 — built-in file-tool access pattern

| Cell | Command (inside sandbox) | Expected | Observed | Result |
|---|---|---|---|---|
| attempt R | `/bin/cat <A>/scratch.txt` | allow | `attempt scratch`, exit 0 | PASS |
| attempt W | `sh -c "echo new > <A>/w1.txt"` | allow | file created, exit 0 | PASS |
| attempt nested W | `sh -c "mkdir -p <A>/sub/deep && echo n > <A>/sub/deep/f"` | allow | `ok`, exit 0 | PASS |
| main checkout R | `/bin/cat <REPO>/docs/architecture.md` | deny | `Operation not permitted`, exit 1 | PASS |
| main checkout W | `sh -c "echo tamper >> <REPO>/docs/architecture.md"` | deny | `Operation not permitted`, exit 1; file unchanged | PASS |
| ledger/state R | `/bin/cat <STATE>/ledger.sqlite` | deny | `Operation not permitted`, exit 1 | PASS |
| ledger/state W | `sh -c "echo x >> <STATE>/ledger.sqlite"` | deny | `Operation not permitted`, exit 1 | PASS |

Built-in read/write/edit tools use the same `open`/`write`/`rename` syscalls, so
this proves the built-in-tool boundary. Verified against the real harness too
(row p08): opencode v2.0.15 starts, serves, and is subject to the same profile.

### Row 2 — shell descendants inherit confinement

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| child R main | `sh <A>/depth.sh 1 leaf_read_main.sh` | deny | `Operation not permitted` | PASS |
| grandchild W attempt | `sh <A>/depth.sh 2 leaf_write_attempt.sh` | allow | `grand.txt` created | PASS |
| grandchild R `.git` | `sh <A>/depth.sh 2 leaf_read_git.sh` | deny | `Operation not permitted` | PASS |
| grandchild R `~/.ssh` | `sh <A>/depth.sh 2 leaf_read_ssh.sh` | deny | `Operation not permitted` | PASS |
| depth-6 R `.git` | `sh <A>/depth.sh 5 leaf_read_git.sh` | deny | `Operation not permitted` | PASS |
| depth-4 R secret | `sh <A>/depth.sh 3 leaf_read_secret.sh` | deny | `Operation not permitted` | PASS |
| backgrounded child | `sh -c "sh leaf_write_attempt.sh & wait"` | confined | allowed write only to attempt | PASS |

### Row 3 — build scripts confined

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| build artifact | `sh build_ok.sh` (writes `<A>/build/out.o`) | allow | artifact created | PASS |
| build escape | `sh build_escape.sh` (also writes `<REPO>/should-not-write.txt`) | deny | `Operation not permitted`; repo intact | PASS |
| build steal | `sh build_steal.sh` (reads `<STATE>/auth.json`) | deny | `Operation not permitted`; 0-byte leak | PASS |
| clang compile | `env TMPDIR=<A> clang -o <A>/hello <A>/hello.c` | allow | binary built | PASS |
| run compiled | `<A>/hello` | allow | `hello build` | PASS |
| compiled binary R secret | `clang`-built stealer reads `<STATE>/auth.json` | deny | `open: Operation not permitted` | PASS |

A real compile works under confinement once the Xcode toolchain path is
read-only allowed; the compiled binary inherits the same denials.

### Row 4 — shared Git metadata

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| read `.git/HEAD` | `/bin/cat <REPO>/.git/HEAD` | deny | `Operation not permitted` | PASS |
| read `.git/config` | `/bin/cat <REPO>/.git/config` | deny | `Operation not permitted` | PASS |
| read `.git/objects` | `/bin/ls <REPO>/.git/objects` | deny | `Operation not permitted` | PASS |
| write `.git` | `sh -c "echo x > <REPO>/.git/s0b_tamper"` | deny | `Operation not permitted` | PASS |
| work in main cwd | `sh -c "cd <REPO> && ls -a"` | deny | `ls: .: Operation not permitted` | PASS |

The `git` binary was not executed inside the sandbox (running it triggers an
xcode-select install prompt on an unconfigured toolchain); direct `.git`
filesystem access is the authoritative test and is denied.

### Row 5 — credentials

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| `~/.ssh` | `/bin/ls ~/.ssh` | deny | `Operation not permitted` | PASS |
| `~/.config` | `/bin/ls ~/.config` | deny | `Operation not permitted` | PASS |
| copied `auth.json` | `/bin/cat <STATE>/auth.json` | deny | `Operation not permitted` | PASS |

### Row 6 — protected tests

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| read | `/bin/cat <PROT>/guard_test.sh` | allow | `PROTECTED TEST FIXTURE` | PASS |
| write | `sh -c "echo tamper >> <PROT>/guard_test.sh"` | deny | `Operation not permitted`; file intact | PASS |

### Row 7 — loopback HTTP (auth proxy)

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| parent baseline | parent `curl http://127.0.0.1:39077/` | reach | `AUTH-PROXY-OK` (exit 0) | PASS |
| worker → parent server | sandboxed `curl http://127.0.0.1:39077/` | allow | `AUTH-PROXY-OK` (exit 0) | PASS |
| worker binds loopback | sandboxed `node` http server on 127.0.0.1:39078; parent curls | allow | `WORKER-OK` | PASS |

### Row 8 — Unix domain socket (nerve bridge)

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| nerve socket | `printf 'ping\n' \| sandboxed nc -U <SOCK>/nerve.sock` | allow | `NERVE-OK:ping` | PASS |
| off-path socket | sandboxed `nc -U <STATE>/forbidden.sock` | deny | exit 1, no response | PASS |

### Row 9 — arbitrary network

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| parent baseline | parent `curl example.com` | reachable | HTTP 200 | PASS |
| DNS name | sandboxed `curl http://example.com/` | deny | curl exit 6 (could not resolve) | PASS |
| raw IP | sandboxed `curl http://1.1.1.1/` | deny | curl exit 7 (could not connect) | PASS |

The parent baseline proves the host has Internet connectivity, so the sandboxed
failures are the boundary, not an offline host.

## Fail-closed proof (row 10)

Canary command `sh -c "echo FAILOPEN > <REPO>/s0b_failopen_canary.txt"` would
create a file outside every allowed area **if** it ever ran unconfined. Under a
broken profile, `sandbox-exec` must refuse and the canary must stay absent.

| Cell | Profile | `sandbox-exec` output | Canary | Result |
|---|---|---|---|---|
| malformed | `profiles/malformed.sb` | `sandbox-exec: syntax error: expecting ')'` (rc 65) | absent | PASS |
| missing | `profiles/does-not-exist.sb` | `No such file or directory` (rc 65) | absent | PASS |
| empty | `profiles/empty.sb` | `no version specified` (rc 65) | absent | PASS |
| undefined param | `profiles/undefined-param.sb` | `invalid data type of path filter` (rc 65) | absent | PASS |
| bad inline | `sandbox-exec -p '(version 1) (deny default) (bogus-operator invalid)'` | `unbound variable: bogus-operator` (rc 65) | absent | PASS |
| sanity (valid profile) | `profiles/worker.sb` | runs, writes only in attempt (rc 0) | n/a | PASS |

`sandbox-exec` never fell back to unconfined execution: every broken profile
exited 65 before `exec`-ing the command. The engine applies the sandbox at
`exec`, so a profile that does not compile means the process is never created.

## Provider auth proxy + narrow bridge

| Cell | Command | Expected | Observed | Result |
|---|---|---|---|---|
| worker reaches proxy | sandboxed `curl http://127.0.0.1:39079/` (parent owns server) | allow | `AUTH-PROXY-OK` | PASS |
| worker cannot read credential | `/bin/cat <STATE>/auth.json` | deny | `Operation not permitted` | PASS |
| worker reaches nerve | `printf '{...}' \| nc -U <SOCK>/nerve.sock` | allow | `NERVE-OK:{"op":"report"}` | PASS |
| worker cannot list state | `/bin/ls <STATE>` | deny | `Operation not permitted` | PASS |

The credential stays with the parent-owned proxy; the worker reaches the proxy
over loopback and the bridge over one Unix-socket subtree, while state and
credential files remain unreadable.

## harness-in-sandbox smoke (opencode)

| Cell | Command | Observed | Result |
|---|---|---|---|
| version | sandboxed `opencode --version` | `opencode v2.0.15` | PASS |
| serve alive | sandboxed `opencode serve --hostname 127.0.0.1 --port 39002` (isolated XDG, `TMPDIR`→attempt) | `server listening on http://127.0.0.1:39002` | PASS |
| loopback reachable | parent `curl /api/info` | HTTP 401 (TCP accepted) | PASS |
| shutdown | SIGTERM the sandboxed serve | no orphan process | PASS |

Real-harness API validation (auth handshake, sessions, SSE) is S0a scope; here it
merely confirms the harness binary and its server run inside the boundary.
opencode required `TMPDIR` pointed at an attempt-owned dir — pointing it at the
global temp dir is exactly the leak the profile forbids (see Deviations).

## Idle resource and shutdown

| Metric | Unconfined | Confined | Note |
|---|---|---|---|
| idle `sleep` RSS | 1168 KB | 1168 KB | same pid: `sandbox-exec` `exec`s the target |
| idle `node` RSS | 46992 KB | 47104 KB | +~112 KB noise; boundary is kernel-enforced |
| idle CPU | ~0–1.4 % | ~1.0–2.0 % | startup sample; settles at idle |

Seatbelt adds **no supervisor process**: `sandbox-exec` applies the sandbox and
`exec`s the target in place, so per-worker memory overhead is negligible and the
boundary is inherited by every descendant.

Shutdown caveat (important for `SandboxApi`): signalling the launched pid does
**not** reap descendants. `kill -TERM <leader>` left a background descendant
reparented to `init` (ppid 1). The architecture already requires terminating
process trees (§2, §3.6); the spike confirms `SandboxApi` must kill the whole
process group / tree, not just the top pid.

## Verdict

**Seatbelt is acceptable for the macOS MVP — no container-in-VM required before
unattended execution**, subject to the conditions below. All §11 boundary
properties hold: fail-closed initialization, write confinement to attempt-owned
dirs, credential/Git/state/main-checkout denial, protected-read-only,
loopback + single Unix-socket reachability, arbitrary-network denial, and
effectively zero idle overhead. `sandbox-exec` is deprecated but functional on
macOS 26.6.2 and refuses to run on any profile error.

Conditions carried into `SandboxApi`:

1. **Fail-closed gate stays in the conductor.** `sandbox-exec` fails closed at
   exec time; `SandboxApi` must additionally refuse to launch when the rendered
   profile is missing/invalid (never fall back to an unconfined run).
2. **Set `TMPDIR`/`XDG_*` into attempt-owned dirs.** The profile denies the
   global temp dir; tools (e.g. opencode/bun) need an attempt-scoped temp.
3. **Kill process groups, not pids.** Descendants orphan on leader death.
4. **Render toolchain read paths per attempt.** The profile allows the Xcode
   toolchain read-only; additional SDK/toolchain roots must be rendered.
5. **Narrow `mach-lookup` in the production profile** (see Deviations).

## Deviations and open items

- **`(allow mach-lookup)` is unfiltered.** Required for a functioning process
  (dyld/Foundation/system services). It does not re-enable file or network access
  under Seatbelt, and every denial above held with it enabled, but the production
  profile should restrict it to an observed required-service allowlist. Track as
  a hardening item for `SandboxApi`; a follow-on spike can enumerate the services
  opencode actually needs.
- **`EXTRA_RO` is a single read-only root** for the harness install tree; a real
  renderer needs a small list (install tree, additional SDKs, caches that must be
  read-only).
- **`file-read-metadata` is global (stat-only).** Needed for path traversal
  (`mkdir -p`, `cd`). Verified it does **not** grant directory listing or file
  content (`ls <REPO>/docs` and `ls ~/.ssh` are still denied). Acceptable, but
  documented as an intentional allowance.
- **opencode HTTP API returned 401** to unauthenticated `/api/info`; full auth
  handshake and endpoint validation belong to S0a. Not a confinement result.
- **`sandbox-exec` deprecation.** No replacement API was exercised (SBPL/seatbelt
  via `sandbox-exec` is the only supported local mechanism); compatibility was
  proved empirically on the pinned OS. Re-verify on macOS upgrades.
- **`~/.config` denial**: verified for the direct path only; the profile denies
  all of `/Users/<user>` except rendered roots, so alternate credential paths
  (keychains via mach, `~/.netrc`, provider-specific dirs) rely on the mach-lookup
  hardening above.
- **Not tested here**: multi-user hosts, SIP-disabled systems, network namespace
  by hostname vs raw IP edge cases (raw IP + DNS both denied as expected),
  `opencode` tool-execution under a live model (needs provider auth, S0a).
