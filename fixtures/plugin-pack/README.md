# `plugin-pack` — HP.8 independent plugin package fixture

A standalone, off-workspace executable package used by the HP.8 proof
(`crates/agalma-conductor/tests/hp_plugin_proof.rs`, `agalma-jmr.8`). It is a
**real** plugin process, not a linked implementation loaded into the hub: it
speaks the frozen common plugin protocol (`docs/self-mutation-contracts.md`)
over the Connect RPC transport and shares only public contracts and the common
transport with the hub.

## Contents

| File | Role |
|---|---|
| `Cargo.toml` | Own workspace boundary (separate from the product workspace); path deps on `crates/agalma-contracts` and `crates/agalma-ipc` only. |
| `Cargo.lock` | Pinned resolution for the standalone workspace (no new downloads). |
| `src/main.rs` | The executable: announcement, `plugin.Describe`/`Health`/`Snapshot`/`Invoke`/`Reconcile`/`Cancel`, `hub.PublishReport` callback, independent effect journal. |
| `manifest.json` | `PluginManifest/v1` template. The proof fills `package_digest` when it seals the runtime package. |
| `config-schema.json` | Declared configuration schema (artifact ref). |
| `default-config.json` | Declared default configuration (artifact ref). |
| `report-schema.json` | Declared report schema for `example.plugin-pack.load/v1`. |
| `README.md` | This file. |

The plugin id `example.plugin-pack` and the report namespace
`example.plugin-pack.load` are unknown to any hub before discovery; the runtime
package the proof assembles adds the built executable at `bin/plugin-pack` and
renames `manifest.json` to `plugin-manifest.json`.

## Build (offline, isolated compiler cache)

From the repository root:

```
cargo build --offline --manifest-path fixtures/plugin-pack/Cargo.toml \
    --target-dir target/plugin-pack-build
```

The product workspace build (`make`/`cargo build --workspace`) deliberately does
not build this package: it is its own workspace and is excluded from the product
workspace.

## Runtime contract

* `--socket <path>` (or `--socket=<path>`) — the child UDS to serve.
* `AGALMA_HUB_SOCKET` — the hub capability RPC socket for spoke callbacks.
* `AGALMA_PLUGIN_REPORT_GRANT` — the per-launch `hub.PublishReport` grant token.
* `AGALMA_PLUGIN_BINDING` — the pinned `BindingPin/v1` JSON for the launch,
  when the hub conveys it. When it is absent, the plugin derives its pinned
  binding from its own hub-owned installed location
  (`<state_dir>/plugin-installed/<plugin>/g<generation>/package/`): the
  generation from the `g<generation>` directory name and the `config_digest`
  from the sealed `plugin-manifest.json` default configuration it was installed
  with. This lets the spoke announce and report its exact pinned identity
  without the hub importing plugin code.
* `AGALMA_EFFECT_JOURNAL` — path the plugin appends one JSON line to per applied
  `plugin.Invoke` (the independent, plugin-side effect record; the hub cannot
  write it).
* `AGALMA_PLUGIN_DELAY_ACTIONS` — optional comma-separated action ids whose
  `plugin.Invoke` applies the effect but returns a non-terminal (`Accepted`)
  status, so the hub parks the operation pinned to its generation and settles it
  via `plugin.Reconcile` (used by the HP.8 delayed-upgrade proof).

The plugin announces `api: "plugin"`, protocol major `1`, `impl:
"example.plugin-pack"` and capabilities `ledger.read`, `report.publish`. It
declares two actions: `flush` (requires `ledger.read`) and `danger` (requires
`ledger.write`, which no launch grants — the denied-action case).
