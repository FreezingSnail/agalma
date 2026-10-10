# Paid-route evidence — credential-injecting proxy `agalma-csw.2`

Bead: `agalma-csw.2`. Base HEAD: `db7f081`. Date: 2026-10-10.
Design: `docs/reliability-hardening.md` (H3 routing); prices/routes:
`docs/research/opencode-models.md`; auth/proxy mapping:
`docs/research/cloud-offerings.md` §3.

## 0. Result

A scratch-clone task ran on the **paid** OpenCode Go route through the
parent-owned credential-injecting proxy and **merged on `main` with real cash
cost**:

| Field | Value |
|---|---|
| Task | `paid-kvt` (builder-prompt acceptance/no-test-edits guidance, M1.8 `vc6` wording) |
| Execution | `exec:paid-kvt:1` |
| Model | `opencode-go/deepseek-v4.1-flash` |
| Route | `builder:fix` (origin `override`, `paid_active=true`) |
| Attempts | 1 (0 stalls) |
| Verdict | **green / done** (bd `closed`) |
| Tokens | 13 184 in / 1 451 out |
| `cash_usd` | **0.004763088** |
| `quota_usd` | 0.0028482 |
| Merged result | `main` = `da7208a54fb2b137dd7edcc15e2eafbbdbe6b7ed` = candidate; tag `m1/exec-paid-kvt-1` |
| Files touched | `crates/agalma-conductor/prompts/builder.md` (+7/−2) |
| Wall | 38.0 s |
| Credential audit fingerprint | `65f71dcdab6a9cef` (non-cryptographic; host `opencode.ai`) |

Raw artifacts (scratch, never committed):
`target/test-runs/paid-route/run-2/state/ledger.sqlite`,
`…/runs/exec-paid-kvt-1/artifacts/digest.json`, `…/work.log`.

## 1. Command

```text
AGALMA_ALLOW_PAID=1 AGALMA_PROXY_LOG=1 AGALMA_IDLE_TIMEOUT_SECS=240 \
  cargo run --offline -p agalma-conductor -- work \
    --repo target/test-runs/paid-route/run-2/target \
    --state-dir target/test-runs/paid-route/run-2/state \
    --model opencode-go/deepseek-v4.1-flash \
    --harness opencode --max-tasks 1 --once
```

`--model` pins the paid route because the H3 builder class is deliberately
**free-first** (the paid `deepseek-v4.1-flash → glm-5.3-flash` entries are
fallbacks after the free entries, per `docs/reliability-hardening.md` H3); the
verifier class is the tier whose paid entries lead. The override is the H3
sanctioned way to force a paid builder turn and is recorded as `origin=override`
in the `route.model` decision. `AGALMA_ALLOW_PAID=1` is set so the decision
context carries `paid_active=true`.

## 2. Proxy path evidence

The worker only ever reached loopback; the parent proxy terminated TLS and
injected the credential. Proxy request log (stderr):

```text
provider-proxy: allow CONNECT models.opencode.ai:443
provider-proxy: allow CONNECT opencode.ai:443
provider-proxy: allow POST opencode.ai:443   (×8; tool-loop turns)
```

Gated live proxy smoke (same proxy, direct exercise of the forward path):

```text
$ AGALMA_PAID_LIVE=1 cargo test -p agalma-conductor --offline \
      --test provider_proxy_paid_live -- --ignored --nocapture
paid auth: present=true host=opencode.ai fingerprint=65f71dcdab6a9cef
paid proxy upstream status: HTTP/1.1 200 OK
x-opencode-endpoint-id: orcarouter-dsv4.1flash
x-opencode-upstream-model-id: deepseek/deepseek-v4.1-flash
proxy request: allowed=true POST opencode.ai:443
```

The token is read by the parent from `~/.local/share/opencode/auth.json`
(`opencode-go.key`), never logged, never copied into the sandbox, and never
written to the ledger/digest. The sandbox holds only the dummy key
`agalma-proxy`; the proxy strips any inbound credential header and injects
`Authorization: Bearer <token>` plus `x-opencode-session` (OpenCode Go requires
it) upstream.

## 3. Route decision (recorded in the ledger)

```json
{
  "kind": "RouteModel",
  "context": {
    "allow_paid": true, "paid_available": true, "paid_active": true,
    "fallback_reason": null, "class": "builder", "family": "fix",
    "role": "builder", "origin": "override"
  },
  "options": [{"option_id": "opencode-go/deepseek-v4.1-flash", "attributes": {"rank": 0}}],
  "pin": {"policy": "route.model/v1", "model": "static"}
}
```

`provider_auth` audit event (token-free):

```json
{"fingerprint":"65f71dcdab6a9cef","host":"opencode.ai","present":true,"reason":null,"recorded_at_unix_ms":1791638332527}
```

Usage event (builder turn):

```text
phase=build role=builder model=opencode-go/deepseek-v4.1-flash
tokens_in=13184 tokens_out=1451 cost_usd=0.004763088
cash_usd=0.004763088 quota_usd=0.0028482
```

`cash_usd` is the provider-reported per-turn dollar cost (OpenCode session
`data.cost`); `quota_usd` is the notional Go-allowance drain from the routing
table. Both are recorded separately, so paid turns no longer read `$0`.

## 4. Acceptance on `main`

The conductor committed the candidate, cloned a pristine verification checkout
at the candidate SHA, ran each acceptance command there (all exit 0), then
CAS-merged the candidate onto `main` and tagged it:

```text
sh -c 'grep -q "acceptance commands" .../builder.md'        exit 0
sh -c 'grep -qi "do not edit the tests" .../builder.md'      exit 0
sh -c '! git diff --name-only HEAD~1 HEAD | grep -qE "Cargo\.toml$|/src/.*\.rs$"' exit 0
```

Post-merge the committed `main` tree contains both markers and the tag
`m1/exec-paid-kvt-1` points at `da7208a` (= candidate = `main`). (The scratch
origin *worktree* copy lags because integration advances `main` by CAS
`update-ref` — the M1.8 origin-worktree caveat — the committed tree is
authoritative and passes acceptance.)

## 5. OpenCode provider `baseURL` override (verified shape)

Verified against the OpenCode v2 provider config (docs
`https://opencode.ai/docs/providers/` "Base URL") and the v2 source
(`packages/core/src/config/discovery.ts`, `packages/util/src/global.ts`):

- The provider is keyed by id under `provider`; the endpoint is
  `provider.<id>.options.baseURL`.
- The global config directory is `OPENCODE_CONFIG_DIR` when set, else
  `$XDG_CONFIG_HOME/opencode`; `opencode.json` there is the global config.
  The confined adapter sets `OPENCODE_CONFIG_DIR` to the checkout (the config
  discovery short-circuit the confine profile needs), so the override is a
  written-then-locally-excluded `opencode.json` in the checkout.
- `opencode-go` uses env `OPENCODE_API_KEY`; the sandbox gets the dummy and the
  proxy injects the real bearer token.

Rendered shape (`render_provider_config`):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": { "opencode-go": { "options": { "baseURL": "http://127.0.0.1:<port>" } } }
}
```

The worker's `baseURL` is the loopback proxy root; the proxy maps
`/<path>` onto `https://opencode.ai/zen/go/v1/<path>` (the configured upstream
base, `AGALMA_PROVIDER_BASE`), so OpenCode's `POST /chat/completions` becomes
`https://opencode.ai/zen/go/v1/chat/completions` with the credential injected.

## 6. Deviations / notes

1. **First scratch run (`run-1`) failed with a config error, not an auth/quota
   error.** Before the provider gate was added, the override was applied to
   *every* model, so the free `opencode/big-pickle` route was mapped onto the
   paid Go base and the provider returned
   `AI.Error.InvalidRequest: Upstream request failed: Model is unavailable.`
   The fix: point only `opencode-go` at the injecting proxy; free `opencode`
   routes keep the transparent CONNECT tunnel to the Zen endpoint. `run-1`
   cost $0.00 (free model) and is retained as evidence.
2. **`--model` override.** As above, the live proof pins the paid model because
   H3 builder routing is free-first; static paid *eligibility* (with/without
   credential) is covered by the deterministic tests.
3. No `git commit`/`push` was performed in the live repo; all bd, merge, and
   tag activity happened in the scratch clone. `bd` in the live repo was only
   read.

## 7. Test coverage for this bead

- `provider_proxy` unit tests: header injection, inbound-credential stripping,
  allowlist denial (`403`), path mapping, chunked/SSE passthrough, token
  redaction in `Debug`, `x-opencode-session` injection/preservation.
- `provider_proxy_paid_live` (gated): real 200 through the proxy (above).
- `provider_auth` unit tests: token read precedence, fingerprint stability,
  audit event recorded without the token, host parsing.
- `harness-opencode` unit tests: `baseURL` config shape, dummy-key isolation,
  local exclude so the override never enters the candidate commit.
- `routing` unit/integration tests: paid eligibility requires both opt-in and
  a credential; absent credential → free fallback with a recorded reason;
  `cash_quota_for_usage` surfaces the reported paid cash.
- `phases` unit tests: paid turns record `cash_usd > 0`; free turns stay `$0`.
