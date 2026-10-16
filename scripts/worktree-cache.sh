#!/usr/bin/env bash
# Worktree build-cache hygiene for Agalma.
#
# Why this exists
#   Implementation runs in a per-bead Git worktree with an isolated Cargo cache
#   (CARGO_TARGET_DIR=target/worktree-caches/<bead>/target). A cold debug cache
#   is 0.5-4 GiB: full dependency graph, native test executables, and previously
#   incremental IR. Removing the worktree after a merge left the cache behind, so
#   merged and abandoned beads accumulated multi-gigabyte orphans that nothing
#   reclaimed.
#
# Invariants
#   * target/test-runs holds retained evidence and is never touched here.
#   * A registered (live) worktree is never removed or stripped.
#   * A dirty or unmerged worktree is preserved; removal requires that bead's
#     reviewed changes to be merged and the worktree clean.
#   * Nothing is deleted while a cargo/rustc process is running.
#
# Commands
#   report                     Sizes of every target/ consumer. No changes.
#   sweep [--apply] [--dedupe] Classify each cache and worktree:
#                                keep        registered / dirty / unmerged
#                                orphaned    bead's worktree is gone
#                                pending     clean + merged, removal via
#                                            `remove <bead>` or --apply
#                                duplicate   <worktree>/target exists beside a
#                                            redirected cache (--dedupe)
#                              Dry-run by default; --apply performs the removals
#                              for "orphaned" and "pending" entries. --dedupe
#                              additionally strips the redundant inner target of
#                              a live worktree, keeping the worktree itself.
#   remove <bead>              Remove one bead's worktree (guarded: clean and
#                              merged) plus its redirected cache.
#
# Usage:
#   bash scripts/worktree-cache.sh report
#   bash scripts/worktree-cache.sh sweep
#   bash scripts/worktree-cache.sh sweep --apply
#   bash scripts/worktree-cache.sh remove agalma-5fy.12

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
root_dir="$(cd -- "$script_dir/.." && pwd)"
cd "$root_dir"

main_branch="${MAIN_BRANCH:-main}"
worktrees_dir="target/worktrees"
caches_dir="target/worktree-caches"

usage() {
    sed -n '2,60p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

registered_worktrees() {
    git worktree list --porcelain | awk '/^worktree /{print $2}'
}

is_registered() {
    local path="$1"
    registered_worktrees | while IFS= read -r wt; do
        if [ "$(cd -- "$wt" 2>/dev/null && pwd -P)" = "$path" ]; then
            echo yes
            return 0
        fi
    done | grep -q yes
}

bead_branches() {
    local bead="$1"
    git for-each-ref --format='%(refname:short)' refs/heads/ \
        | grep -E "^task/${bead}$|^task/${bead}-" || true
}

# merged | unmerged | missing
branch_state() {
    local bead="$1" branches any=0
    branches="$(bead_branches "$bead")"
    [ -n "$branches" ] || { echo missing; return 0; }
    any=1
    while IFS= read -r branch; do
        [ -n "$branch" ] || continue
        if ! git merge-base --is-ancestor "$branch" "$main_branch" 2>/dev/null; then
            echo unmerged
            return 0
        fi
    done <<<"$branches"
    [ "$any" = 1 ] && echo merged
}

worktree_dirty() {
    local path="$1"
    [ -d "$path" ] || { echo no; return 0; }
    if [ -n "$(git -C "$path" status --porcelain 2>/dev/null)" ]; then
        echo yes
    else
        echo no
    fi
}

cargo_running() {
    pgrep -qx cargo 2>/dev/null || pgrep -qx rustc 2>/dev/null
}

size_of() {
    [ -e "$1" ] || { echo "-"; return 0; }
    du -sh "$1" 2>/dev/null | awk '{print $1}'
}

cmd_report() {
    echo "Agalma build-artifact report ($root_dir)"
    echo
    printf '  %-34s %s\n' "target/debug" "$(size_of target/debug)"
    printf '  %-34s %s\n' "target/release" "$(size_of target/release)"
    printf '  %-34s %s\n' "target/test-runs (evidence)" "$(size_of target/test-runs)"
    printf '  %-34s %s\n' "target/tmp" "$(size_of target/tmp)"
    echo
    if compgen -G "$caches_dir/*" >/dev/null; then
        echo "  isolated worktree caches:"
        for dir in "$caches_dir"/*/; do
            [ -d "$dir" ] || continue
            printf '    %-32s %s\n' "$(basename "$dir")" "$(size_of "$dir")"
        done
    fi
    if compgen -G "$worktrees_dir/*" >/dev/null; then
        echo "  worktrees (inner target/ is a duplicate of the redirected cache):"
        for dir in "$worktrees_dir"/*/; do
            [ -d "$dir" ] || continue
            printf '    %-32s %s\n' "$(basename "$dir")" "$(size_of "$dir")"
            if [ -d "$dir/target" ]; then
                printf '      %-30s %s\n' "inner target/" "$(size_of "$dir/target")"
            fi
        done
    fi
    echo
    printf '  %-34s %s\n' "target total" "$(size_of target)"
}

cmd_sweep() {
    local apply="no" dedupe="no"
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --apply) apply="yes" ;;
            --dedupe) dedupe="yes" ;;
            -h|--help) usage; return 0 ;;
            *) echo "unknown option: $1" >&2; return 2 ;;
        esac
        shift
    done

    if cargo_running; then
        echo "cargo/rustc is running; reporting only, no changes." >&2
        apply="no"
    fi

    local empty=yes
    for dir in "$caches_dir"/*/ "$worktrees_dir"/*/; do
        if [ -d "$dir" ]; then
            empty=no
            break
        fi
    done
    if [ "$empty" = yes ]; then
        echo "no worktree caches or worktrees found under $caches_dir / $worktrees_dir"
        return 0
    fi

    local keep=0 orphan=0 pending=0 duplicate=0
    for dir in "$caches_dir"/*/; do
        [ -d "$dir" ] || continue
        local bead wt state dirty label action
        bead="$(basename "$dir")"
        wt="$worktrees_dir/$bead"
        state="$(branch_state "$bead")"
        dirty="$(worktree_dirty "$wt")"

        if [ -d "$wt" ] && is_registered "$(cd "$wt" && pwd -P)"; then
            if [ "$dirty" = yes ]; then
                label="keep (live, dirty)"; keep=$((keep + 1))
            elif [ "$state" = unmerged ]; then
                label="keep (live, unmerged)"; keep=$((keep + 1))
            else
                label="pending removal (live, clean, merged)"; pending=$((pending + 1))
            fi
        elif [ -d "$wt" ]; then
            if [ "$dirty" = yes ]; then
                label="keep (unregistered, dirty)"; keep=$((keep + 1))
            elif [ "$state" = unmerged ]; then
                label="keep (unregistered, unmerged)"; keep=$((keep + 1))
            else
                label="pending removal (unregistered worktree, clean, merged)"; pending=$((pending + 1))
            fi
        else
            label="orphaned (worktree gone, branch ${state})"; orphan=$((orphan + 1))
        fi

        action=""
        case "$label" in
            orphaned*)
                if [ "$apply" = yes ]; then action="remove"; fi
                ;;
            "pending removal"*)
                if [ "$apply" = yes ] && [ -d "$wt" ]; then
                    if git worktree remove -- "$wt" >/dev/null 2>&1; then
                        action="remove"
                    fi
                fi
                ;;
        esac

        printf '  %-34s %-10s %s %s\n' "$bead" "$(size_of "$dir")" "$label" "$action"
        if [ "$action" = remove ]; then
            rm -rf -- "$dir"
            echo "      removed cache $dir"
        fi
    done

    # Duplicate inner target of a live worktree.
    for dir in "$worktrees_dir"/*/; do
        [ -d "$dir" ] || continue
        [ -d "$dir/target" ] || continue
        local bead
        bead="$(basename "$dir")"
        [ -d "$caches_dir/$bead" ] || continue
        printf '  %-34s %-10s duplicate inner target/ (redirected cache exists)\n' \
            "$bead" "$(size_of "$dir/target")"
        duplicate=$((duplicate + 1))
        if [ "$dedupe" = yes ] && [ "$apply" = yes ]; then
            rm -rf -- "$dir/target"
            echo "      removed duplicate $dir/target"
        fi
    done

    echo
    echo "classified: keep=$keep orphaned=$orphan pending=$pending duplicate=$duplicate"
    if [ "$apply" = no ]; then
        echo "dry run. Re-run with: make sweep-apply    (add --dedupe to also strip duplicates)"
    fi
}

cmd_remove() {
    local bead="${1:-}"
    [ -n "$bead" ] || { echo "remove requires a bead ID" >&2; usage; return 2; }

    local wt="$worktrees_dir/$bead" cache="$caches_dir/$bead"
    [ -d "$wt" ] || [ -d "$cache" ] || { echo "no worktree or cache for $bead" >&2; return 1; }

    local state dirty
    state="$(branch_state "$bead")"
    dirty="$(worktree_dirty "$wt")"

    if [ -d "$wt" ]; then
        if [ "$dirty" = yes ]; then
            echo "refusing: $wt is dirty. Preserve dirty worktrees (AGENTS.md); commit or report the residual work." >&2
            return 3
        fi
        if [ "$state" != merged ]; then
            echo "refusing: branch state for $bead is '$state', not merged into $main_branch." >&2
            return 3
        fi
        if cargo_running; then
            echo "refusing: cargo/rustc is running." >&2
            return 3
        fi
        git worktree remove -- "$wt"
        echo "removed worktree $wt"
    fi

    if [ -d "$cache" ]; then
        local freed
        freed="$(size_of "$cache")"
        rm -rf -- "$cache"
        echo "removed cache $cache ($freed)"
    fi
}

case "${1:-}" in
    report) shift; cmd_report "$@" ;;
    sweep) shift; cmd_sweep "$@" ;;
    remove) shift; cmd_remove "$@" ;;
    ""|-h|--help) usage ;;
    *) echo "unknown command: $1" >&2; usage; return 2 ;;
esac
