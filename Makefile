# Root Makefile for Agalma.
#
# Reusable build entry point for the product Cargo workspace:
#   make / make all / make build   debug build of all workspace targets
#   make release                   release build of the same targets
#   make help                      targets, scope, output, and overrides
#
# This builds only. It does not run tests, start services, or call paid
# providers. See docs/development-workflow.md for the test entry point.

# Overrides (environment or command line):
#   CARGO        Cargo executable; default "cargo". Quoted in recipes so an
#                executable path containing spaces works and is never split
#                into multiple shell words.
#   CARGO_FLAGS  Cargo options; default "--offline --locked". Provided as a
#                whole, so setting it replaces the defaults.
# CARGO_TARGET_DIR and other Cargo environment variables are left untouched,
# so the normal target cache is reused by default.
CARGO ?= cargo
CARGO_FLAGS ?= --offline --locked

# Native Cargo workspace discovery: --workspace selects every member of
# Cargo.toml's [workspace] table, so new members need no Makefile edits.
# --all-targets compiles libraries, binaries, examples, tests, and benches
# without running them.
BUILD_TARGETS = --workspace --all-targets

# Explicit default goal: `make` with no arguments builds the workspace.
.DEFAULT_GOAL := all

.PHONY: all build release help

all: build

build:
	"$(CARGO)" build $(CARGO_FLAGS) $(BUILD_TARGETS)

release:
	"$(CARGO)" build $(CARGO_FLAGS) $(BUILD_TARGETS) --release

help:
	@echo "Agalma root build"
	@echo ""
	@echo "Targets:"
	@echo "  make, make all, make build   Debug build of all product-workspace targets"
	@echo "                               (libraries, binaries, examples, tests, benches)."
	@echo "  make release                 Same targets in the release profile."
	@echo "  make help                    Show this help."
	@echo ""
	@echo "Scope: every member of the Cargo workspace in Cargo.toml, discovered"
	@echo "natively with --workspace, so new registered members need no Makefile"
	@echo "edits. The standalone spikes under spikes/ and the fixture packages under"
	@echo "fixtures/ are excluded from the product workspace and are not built here."
	@echo ""
	@echo "Behavior: compiles only. Does not run tests or benches, start services,"
	@echo "or invoke paid providers. Cargo runs --offline --locked by default, so it"
	@echo "downloads no crates by default; overriding CARGO_FLAGS does not authorize"
	@echo "dependency downloads or lockfile changes -- those still require the"
	@echo "repository's dependency authorization."
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
