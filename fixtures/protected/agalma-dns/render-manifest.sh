#!/usr/bin/env bash
# Render the operator ProtectedInputs/v1 draft for the agalma-dns trial.
#
# This is a repository tool for the bootstrap policy artifact, not the protected
# judge. It pins the canonical repository root and a base SHA, and records the
# judge file's exact SHA-256, UTF-8 byte length, mode, and text.
#
#   bash fixtures/protected/agalma-dns/render-manifest.sh \
#     [--repository-root <abs-path>] [--base-sha <sha>] \
#     [--evidence-base <abs-path>]
#
# Defaults: repository root = first git worktree (the primary checkout);
# base SHA = that repository's current main; evidence base =
# <repository-root>/target/test-runs/agalma-dns. The emitted command is a
# POSIX-shell literal: the evidence base is single-quote encoded and the
# runtime protected script path is double-quoted. The emitted file is a draft
# until the predecessor merges and the final main base is regenerated.

set -euo pipefail

repo_root="$(git worktree list --porcelain | sed -n 's/^worktree //p' | head -n 1)"
base_sha=""
evidence_base=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --repository-root)
            [ "$#" -ge 2 ] || { echo "render-manifest: --repository-root needs a value" >&2; exit 2; }
            repo_root="$2"; shift 2 ;;
        --base-sha)
            [ "$#" -ge 2 ] || { echo "render-manifest: --base-sha needs a value" >&2; exit 2; }
            base_sha="$2"; shift 2 ;;
        --evidence-base)
            [ "$#" -ge 2 ] || { echo "render-manifest: --evidence-base needs a value" >&2; exit 2; }
            evidence_base="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,14p' "$0"; exit 0 ;;
        *)
            echo "render-manifest: unknown argument: $1" >&2; exit 2 ;;
    esac
done

command -v jq >/dev/null 2>&1 || { echo "render-manifest: jq is required" >&2; exit 1; }
[ -n "$base_sha" ] || base_sha="$(git -C "$repo_root" rev-parse main)"
[ -d "$repo_root/.git" ] || [ -f "$repo_root/.git" ] || {
    echo "render-manifest: not a git checkout: $repo_root" >&2; exit 1;
}
repo_root="$(cd "$repo_root" && pwd -P)"
[ -n "$evidence_base" ] || evidence_base="$repo_root/target/test-runs/agalma-dns"
case "$evidence_base" in
    /*) ;;
    *) echo "render-manifest: --evidence-base must be absolute" >&2; exit 2 ;;
esac

# POSIX-shell single-quote literal, including embedded single quotes.
shell_quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
judge="$here/judge.sh"
[ -f "$judge" ] || { echo "render-manifest: missing judge: $judge" >&2; exit 1; }

bytes="$(wc -c <"$judge" | tr -d ' ')"
if command -v shasum >/dev/null 2>&1; then
    digest="sha256:$(shasum -a 256 "$judge" | awk '{print $1}')"
else
    digest="sha256:$(sha256sum "$judge" | awk '{print $1}')"
fi
if stat -f '%Lp' "$judge" >/dev/null 2>&1; then
    mode="$(stat -f '%Lp' "$judge")"
else
    mode="$(stat -c '%a' "$judge")"
fi
case "$mode" in
    755|0755) mode="100755" ;;
    644|0644) mode="100644" ;;
    *) echo "render-manifest: unsupported judge mode $mode" >&2; exit 1 ;;
esac

# Emit one shell-correct operator command: the evidence literal is single-quote
# encoded (spaces and embedded single quotes are safe), and the runtime
# protected path is expanded inside double quotes.
command="AGALMA_JUDGE_EVIDENCE_BASE=$(shell_quote "$evidence_base") bash \"\$AGALMA_PROTECTED_DIR/agalma-dns/judge.sh\""

jq -n \
    --arg repo "$repo_root" \
    --arg base "$base_sha" \
    --arg command "$command" \
    --arg digest "$digest" \
    --argjson bytes "$bytes" \
    --arg mode "$mode" \
    --rawfile text "$judge" \
    '{
        format: "ProtectedInputs/v1",
        entries: [{
            task_id: "agalma-dns",
            repository_root: $repo,
            base_sha: $base,
            allowed_paths: [
                "scripts/test.sh",
                "scripts/tests/runner-properties.sh",
                "scripts/tests/runner-focused.sh"
            ],
            protected_paths: ["fixtures/protected/agalma-dns"],
            baseline: {
                format: "ProtectedBaseline/v1",
                files: [{
                    path: "agalma-dns/judge.sh",
                    mode: $mode,
                    digest: $digest,
                    bytes: $bytes,
                    text: $text
                }],
                commands: [$command]
            }
        }]
    }'
