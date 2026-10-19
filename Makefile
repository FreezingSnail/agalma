# Root Makefile for Agalma.
#
# Single entry point for routine builds, verification scopes, and build-artifact
# hygiene:
#   make / make all / make build   debug build of all workspace targets
#   make release                   release build of the same targets
#   make test-focused SELECTION=.. focused checks (required selection)
#   make test-peers                local process smokes (ledger/workspace/...)
#   make test-full                 workspace tests
#   make check-noop-rebuild        agalma-ipc build-script no-op regression guard
#   make start                     live cockpit (real work --watch runtime)
#   make preview [PORT=<n>]        fixture-backed cockpit preview (not live state)
#   make report                    build-artifact sizes (no changes)
#   make sweep / sweep-apply       classify / remove orphaned worktree caches
#   make worktree-remove BEAD=<id> remove a merged bead's worktree and cache
#   make clean-build               remove target/debug and target/release
#   make clean-caches              remove every worktree cache (rebuildable)
#   make help                      targets, scope, output, and overrides
#
# This builds, runs the selected checks, and manages caches. It does not start
# long-running services or call paid providers. See docs/development-workflow.md
# for the test scope contract and docs/testing-strategy.md for selection rules.

# Overrides (environment or command line):
#   CARGO        Cargo executable; default "cargo". Quoted in recipes so an
#                executable path containing spaces works and is never split
#                into multiple shell words.
#   CARGO_FLAGS  Cargo options; default "--offline --locked". Provided as a
#                whole, so setting it replaces the defaults.
#   CARGO_INCREMENTAL
#                Incremental-compilation switch; default 0. Incremental IR is
#                the largest per-cache artifact and is not shareable between
#                caches. Set CARGO_INCREMENTAL=1 for fast local iteration.
#   CARGO_TARGET_DIR and other Cargo environment variables are left untouched,
#                so the normal target cache is reused by default. Inside an
#                implementation worktree, point this at
#                target/worktree-caches/<bead>/target (workflow rule).
#   BEAD         Bead ID used in the evidence/log path; default "manual".
#   RUN_ID       Run directory name; default a UTC timestamp.
#   SELECTION    Focused test selection passed to Cargo, e.g.
#                '-p agalma-conductor --test sf_scope_guard live_process_smoke'.
#                Required by test-focused. Passed as written; never evaluate
#                untrusted text here.
#   JOBS         Concurrent make jobs, forwarded to Cargo where relevant.
#   PORT         Local TCP port for `make preview`; default 8791.
#   REPO         Live start origin repository and bd backlog; default repo root.
#   PLUGIN_ROOT  Live sealed plugin directory; default target/live/plugins.
#   STATE_DIR    Optional live runtime durable state directory.
#   CONTROL_PLUGIN
#                Optional installed plugin id for factory.control/v1.
#   NO_OPEN      Set to 1 to skip opening the live cockpit URL.
CARGO ?= cargo
CARGO_FLAGS ?= --offline --locked
CARGO_INCREMENTAL ?= 0
export CARGO_INCREMENTAL

BEAD ?= manual
RUN_ID ?= $(shell date -u +%Y%m%dT%H%M%S)
RUN_DIR ?= target/test-runs/$(BEAD)/$(RUN_ID)

# Local port for the fixture-backed cockpit preview (`make preview`).
PORT ?= 8791

# Live cockpit inputs (`make start`): the origin repository and bd backlog, the
# sealed plugin root the dashboard package is installed into, and optional
# pass-through settings for the conductor's own resolution.
REPO ?= $(CURDIR)
PLUGIN_ROOT ?= target/live/plugins
STATE_DIR ?=
CONTROL_PLUGIN ?=
NO_OPEN ?=

# Native Cargo workspace discovery: --workspace selects every member of
# Cargo.toml's [workspace] table, so new members need no Makefile edits.
# --all-targets compiles libraries, binaries, examples, tests, and benches
# without running them.
BUILD_TARGETS = --workspace --all-targets

# Test binaries live in the same target directory as the build. Keep this
# derived from CARGO_TARGET_DIR so redirected worktree caches work unchanged.
TARGET_DIR ?= $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)

# Keep the shell POSIX-ish and non-fatal by default; recipes that need strict
# behavior add `set -eu` or `set -o pipefail` themselves, so a recipe can still
# capture an exit status after a failing pipeline.
SHELL := /bin/bash
.SHELLFLAGS := -c

# Run a command, keep the exact command line, raw output, and exit status under
# $(RUN_DIR), then fail the target when the command fails. $(1) command text
# (interpolated as shell words, never re-evaluated), $(2) log base name.
# Keep every $(1) free of commas: they separate $(call) arguments.
define run_logged
mkdir -p "$(RUN_DIR)"; printf '%s\n' "$(1)" > "$(RUN_DIR)/$(2).command"; \
set -o pipefail; { $(1); } 2>&1 | tee "$(RUN_DIR)/$(2).log"; \
status=$$?; printf 'exit=%s\n' "$$status" > "$(RUN_DIR)/$(2).status"; \
exit $$status
endef

# Unified runner (scripts/test.sh) is pending implementation. When it exists,
# the scope targets forward to it; otherwise they run the documented
# transitional Cargo command. Exactly one implementation runs either way.
TEST_RUNNER := $(shell if [ -x scripts/test.sh ]; then echo scripts/test.sh; fi)

# Explicit default goal: `make` with no arguments builds the workspace.
.DEFAULT_GOAL := all

.PHONY: all build release help test test-focused test-peers test-full \
        check-noop-rebuild start preview report sweep sweep-apply worktree-remove docs-html clean-build clean-caches

all: build

build:
	"$(CARGO)" build $(CARGO_FLAGS) $(BUILD_TARGETS) $(if $(JOBS),-j $(JOBS),)

release:
	"$(CARGO)" build $(CARGO_FLAGS) $(BUILD_TARGETS) --release $(if $(JOBS),-j $(JOBS),)

# ---------------------------------------------------------------------------
# Verification. Scope contract: docs/development-workflow.md. Selection rules:
# docs/testing-strategy.md. Until scripts/test.sh (the unified runner) exists,
# these targets run the documented transitional command for each scope and keep
# raw logs plus exit status under $(RUN_DIR). Once scripts/test.sh lands, the
# targets forward to it so there is one implementation, not two.
# ---------------------------------------------------------------------------

test:
	@echo "Select a scope:" >&2
	@echo "  make test-focused SELECTION='-p <crate> --test <target> [filter]'" >&2
	@echo "  make test-peers" >&2
	@echo "  make test-full" >&2
	@echo "" >&2
	@echo "Optional: BEAD=<id> RUN_ID=<name> to label evidence under target/test-runs/." >&2
	@exit 2

test-focused:
	@if [ -z "$(strip $(SELECTION))" ]; then \
		echo "SELECTION is required, e.g. make test-focused SELECTION='-p agalma-conductor --test sf_scope_guard'" >&2; \
		exit 2; \
	fi
ifeq ($(TEST_RUNNER),)
	@$(call run_logged,"$(CARGO)" test $(CARGO_FLAGS) $(SELECTION),focused)
else
	@$(call run_logged,$(TEST_RUNNER) focused $(SELECTION),focused)
endif

test-peers:
ifeq ($(TEST_RUNNER),)
	@$(call run_logged,bash scripts/verify-peer-transports.sh,peers)
else
	@$(call run_logged,$(TEST_RUNNER) peers,peers)
endif

test-full:
ifeq ($(TEST_RUNNER),)
	@$(call run_logged,"$(CARGO)" build $(CARGO_FLAGS) --workspace --bins,full-build)
	@$(call run_logged,"$(CARGO)" test $(CARGO_FLAGS) --workspace,full)
else
	@$(call run_logged,$(TEST_RUNNER) full,full)
endif

# Regression guard for the agalma-ipc build-script no-op property: a second
# identical invocation must not recompile agalma-ipc or its dependents, while
# editing the real inputs (ipc.proto, build.rs) still triggers a rebuild. The
# script writes its own evidence under target/test-runs/<bead>/<run-id>/.
check-noop-rebuild:
	@bash scripts/check-noop-rebuild.sh

# ---------------------------------------------------------------------------
# Documentation. Renders the markdown linked from docs/progress-roadmap.html
# into rich, self-contained HTML (no network/CDN/JS needed to read). Each page
# carries a component-ingestion map and a wrapping prev/next ring. Node stdlib
# only; it downloads nothing.
# ---------------------------------------------------------------------------

docs-html:
	@node scripts/md-to-html.mjs

# ---------------------------------------------------------------------------
# Cockpit. `make start` is live: it builds the conductor, the real dashboard
# binary, and the workspace component-server binaries the runtime spawns (the
# `--workspace --bins` line; the runtime resolves those siblings next to
# `agalma`). It then seals the dashboard package plus that binary into
# $(PLUGIN_ROOT) and runs the conductor's continuous runtime
# (`agalma work --watch`), which supervises the installed dashboard and prints
# its loopback URL. This is the real factory runtime: it dispatches eligible
# work from REPO's bd backlog.
# scripts/cockpit-live.sh streams the runtime's stderr, announces
# AGALMA_COCKPIT_URL, opens the page (unless NO_OPEN=1), forwards INT/TERM and
# exits with the runtime's status.
#
# `make preview` remains the offline fixture preview: a fixture-backed serving
# root is regenerated under target/start/ on every run, so no repository file
# and no other target/ content changes. scripts/cockpit-preview.py (stdlib
# only) serves the exact-file routes plus GET /api/v1/components/<plugin_id>
# and GET /api/v1/status. This is a preview of the UI with recorded fixture
# data, not live state. Ctrl-C stops the server; a command-line PORT overrides
# the default 8791.
# ---------------------------------------------------------------------------

start:
	@set -eu; \
	"$(CARGO)" build $(CARGO_FLAGS) -p agalma-conductor --bin agalma $(if $(JOBS),-j $(JOBS),); \
	"$(CARGO)" build $(CARGO_FLAGS) -p agalma-web-dashboard --bin agalma-web-dashboard $(if $(JOBS),-j $(JOBS),); \
	"$(CARGO)" build $(CARGO_FLAGS) --workspace --bins $(if $(JOBS),-j $(JOBS),); \
	"$(TARGET_DIR)/debug/agalma" seal-package \
		--package crates/agalma-web-dashboard/package \
		--binary "$(TARGET_DIR)/debug/agalma-web-dashboard" \
		--root "$(PLUGIN_ROOT)"; \
	exec env AGALMA_BIN="$(TARGET_DIR)/debug/agalma" REPO="$(REPO)" PLUGIN_ROOT="$(PLUGIN_ROOT)" \
		$(if $(strip $(STATE_DIR)),STATE_DIR="$(STATE_DIR)",) \
		$(if $(strip $(CONTROL_PLUGIN)),CONTROL_PLUGIN="$(CONTROL_PLUGIN)",) \
		$(if $(strip $(NO_OPEN)),NO_OPEN="$(NO_OPEN)",) \
		sh scripts/cockpit-live.sh

preview:
	@set -eu; \
	if ! command -v python3 >/dev/null 2>&1; then \
		echo "error: 'make preview' needs python3 on PATH; none found" >&2; \
		exit 2; \
	fi; \
	rm -rf target/start; \
	mkdir -p target/start/api/v1; \
	cp -R crates/agalma-web-dashboard/assets/. target/start/; \
	cp fixtures/cockpit/overview.json target/start/api/v1/overview; \
	cp fixtures/cockpit/components-page.json target/start/api/v1/components; \
	cp fixtures/cockpit/factory-runtime.json target/start/api/v1/factory; \
	cp fixtures/cockpit/component-detail.json target/start/api/v1/component-detail.json; \
	cp fixtures/cockpit/status.json target/start/api/v1/status.json; \
	url="http://127.0.0.1:$(PORT)/"; \
	echo "Agalma cockpit PREVIEW: fixture data from fixtures/cockpit (not live hub state)"; \
	echo "AGALMA_COCKPIT_URL $$url"; \
	if command -v open >/dev/null 2>&1; then open "$$url"; fi; \
	exec python3 scripts/cockpit-preview.py "$(PORT)" target/start

# ---------------------------------------------------------------------------
# Build-artifact hygiene. target/test-runs holds retained evidence and is never
# swept; a registered, dirty, or unmerged worktree and its cache are preserved.
# ---------------------------------------------------------------------------

report:
	@bash scripts/worktree-cache.sh report

sweep:
	@bash scripts/worktree-cache.sh sweep

sweep-apply:
	@bash scripts/worktree-cache.sh sweep --apply

worktree-remove:
	@if [ -z "$(strip $(BEAD))" ] || [ "$(BEAD)" = "manual" ]; then \
		echo "BEAD is required, e.g. make worktree-remove BEAD=agalma-5fy.12" >&2; \
		exit 2; \
	fi
	@bash scripts/worktree-cache.sh remove "$(BEAD)"

clean-build:
	@echo "removing $(TARGET_DIR)/debug, $(TARGET_DIR)/release and $(PLUGIN_ROOT) (test-runs and worktree caches preserved)"
	rm -rf "$(TARGET_DIR)/debug" "$(TARGET_DIR)/release" "$(PLUGIN_ROOT)"

clean-caches:
	@echo "removing target/worktree-caches (rebuildable; evidence under target/test-runs untouched)"
	rm -rf target/worktree-caches

help:
	@echo "Agalma root entry point"
	@echo ""
	@echo "Build targets:"
	@echo "  make, make all, make build   Debug build of all product-workspace targets"
	@echo "                               (libraries, binaries, examples, tests, benches)."
	@echo "  make release                 Same targets in the release profile."
	@echo "  make help                    Show this help."
	@echo ""
	@echo "Verification targets (scope contract: docs/development-workflow.md):"
	@echo "  make test-focused SELECTION=..  Focused crate/target/module checks."
	@echo "  make test-peers                 Five local process smokes on Connect."
	@echo "  make test-full                  Workspace tests (excludes live/paid checks)."
	@echo "  make check-noop-rebuild         Regression guard: a second identical"
	@echo "                                  invocation of the agalma-ipc build must not"
	@echo "                                  recompile; real input edits still rebuild."
	@echo "  Raw logs and exit status land in target/test-runs/<BEAD>/<RUN_ID>/."
	@echo "  Set BEAD=<id> to label evidence for a task; default BEAD=manual."
	@echo ""
	@echo "Live cockpit target (real factory runtime):"
	@echo "  make start [REPO=<dir>] [STATE_DIR=<dir>] [PLUGIN_ROOT=<dir>]"
	@echo "                         Build and seal the real dashboard package, then"
	@echo "                         run agalma work --watch, print"
	@echo "                         AGALMA_COCKPIT_URL, and open the live cockpit."
	@echo "                         Ctrl-C stops the runtime."
	@echo "                         WARNING: the live runtime dispatches eligible work"
	@echo "                         from REPO's bd backlog. Use a scratch REPO/STATE_DIR"
	@echo "                         when you do not intend to run real tasks."
	@echo ""
	@echo "Preview target (fixture data, not live hub state):"
	@echo "  make preview [PORT=<n>]"
	@echo "                         Serve a fixture-backed cockpit preview from"
	@echo "                         target/start/ on 127.0.0.1 (default port 8791),"
	@echo "                         print AGALMA_COCKPIT_URL, open the page in the"
	@echo "                         default browser, and stop on Ctrl-C."
	@echo ""
	@echo "Artifact hygiene targets:"
	@echo "  make report          Sizes of target/ consumers (no changes)."
	@echo "  make sweep           Classify worktree caches: keep / orphaned / merged."
	@echo "  make sweep-apply     Remove caches for beads whose worktree is gone or"
	@echo "                       whose task branch is merged into main."
	@echo "  make worktree-remove BEAD=<id>"
	@echo "                       Remove a clean, merged bead's worktree and cache."
	@echo "  make clean-build     Remove target/debug and target/release only."
	@echo "  make clean-caches    Remove target/worktree-caches (rebuildable)."
	@echo "  target/test-runs is retained evidence and is never swept."
	@echo ""
	@echo "Scope: every member of the Cargo workspace in Cargo.toml, discovered"
	@echo "natively with --workspace, so new registered members need no Makefile"
	@echo "edits. The standalone spikes under spikes/ and the fixture packages under"
	@echo "fixtures/ are excluded from the product workspace and are not built here."
	@echo ""
	@echo "Behavior: compiles and runs the selected checks; make start runs the live"
	@echo "factory runtime until Ctrl-C, while make preview serves a local fixture"
	@echo "preview. Neither invokes paid providers. Cargo runs --offline"
	@echo "--locked by default, so it downloads no crates by default; overriding"
	@echo "CARGO_FLAGS does not authorize dependency downloads or lockfile changes --"
	@echo "those still require the repository's dependency authorization."
	@echo ""
	@echo "Output: the Cargo target directory, by default target/debug (build) and"
	@echo "target/release (release), or CARGO_TARGET_DIR when that variable is set."
	@echo "The existing Cargo cache is reused by default."
	@echo ""
	@echo "Overrides:"
	@echo "  CARGO=<path>        Cargo executable (default: cargo). Treated as a single"
	@echo "                      executable path, so paths containing spaces work."
	@echo "  CARGO_FLAGS=<opts>  Cargo options (default: --offline --locked). Set as a"
	@echo "                      whole, so it replaces the defaults."
	@echo "                      e.g. make CARGO_FLAGS='--offline --locked -v'"
	@echo "  CARGO_INCREMENTAL=<0|1>"
	@echo "                      Incremental compilation (default: 0). Set 1 for fast"
	@echo "                      local iteration; incremental IR is the largest"
	@echo "                      per-cache artifact and is not shared across caches."
	@echo "  CARGO_TARGET_DIR=<dir>"
	@echo "                      Alternate cache. Inside a worktree use"
	@echo "                      target/worktree-caches/<bead>/target."
	@echo "  BEAD=<id> RUN_ID=<name>"
	@echo "                      Evidence path label under target/test-runs/."
	@echo "  PORT=<n>            Local port for make preview (default: 8791)."
	@echo "  REPO=<dir>          Live start origin repository and bd backlog (default:"
	@echo "                      this repository root)."
	@echo "  PLUGIN_ROOT=<dir>   Live sealed plugin root (default: target/live/plugins)."
	@echo "  STATE_DIR=<dir>     Optional live runtime durable state directory."
	@echo "  CONTROL_PLUGIN=<id> Optional plugin id approved for factory.control/v1."
	@echo "  NO_OPEN=1           Do not open the live cockpit URL in a browser."
	@echo "  SELECTION=<cargo args>, JOBS=<n> as described above."
