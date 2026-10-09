#!/bin/sh
# S0b probe — rows 1/4/5/6: file-access pattern under confinement.
# Usage: p01-file-access.sh <rundir> [repo]
# Prints "RESULT <cell> <PASS|FAIL> <observed>" lines.
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
RUN_SH="$S0B/probes/run.sh"
REPO="$(cd "${2:-$S0B/../..}" && pwd -P)"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"; ST="$RUN/state"

sandbox() { "$RUN_SH" "$A" "$PROT" "$SOCK" "$@" 2>&1; }
chk() { # name expected_exit actual_exit observed
  if [ "$2" = "$3" ]; then echo "RESULT $1 PASS exit=$3 :: $4"; else echo "RESULT $1 FAIL want=$2 got=$3 :: $4"; fi
}

# --- row1: built-in file tools / attempt-owned dirs --------------------------
echo "== row1 attempt-owned dirs =="
out=$(sandbox /bin/cat "$A/scratch.txt"); rc=$?
echo "READ_ATTEMPT[$rc]: $out"
chk row1.read-attempt 0 "$rc" "$out"

out=$(sandbox /bin/sh -c "echo new > '$A/w1.txt' && echo wrote"); rc=$?
echo "WRITE_ATTEMPT[$rc]: $out"
chk row1.write-attempt 0 "$rc" "created=$([ -f "$A/w1.txt" ] && echo yes)"

out=$(sandbox /bin/sh -c "mkdir -p '$A/sub/deep' && echo n > '$A/sub/deep/f' && echo ok"); rc=$?
echo "WRITE_NESTED[$rc]: $out"
chk row1.mkdir-nested 0 "$rc" "$out"

# --- row1: main checkout denied ---------------------------------------------
echo "== row1 main checkout denied =="
out=$(sandbox /bin/cat "$REPO/docs/architecture.md"); rc=$?
echo "READ_MAIN[$rc]: $out"
chk row1.read-main 1 "$rc" "$out"

out=$(sandbox /bin/sh -c "echo tamper >> '$REPO/docs/architecture.md'"); rc=$?
echo "WRITE_MAIN[$rc]: $out"
chk row1.write-main 1 "$rc" "$out"
if grep -q "tamper" "$REPO/docs/architecture.md"; then echo "RESULT row1.write-main-integrity FAIL main file modified"; else echo "RESULT row1.write-main-integrity PASS main file intact"; fi

# --- row1: ledger/state dir denied ------------------------------------------
echo "== row1 state dir denied =="
out=$(sandbox /bin/cat "$ST/ledger.sqlite"); rc=$?
echo "READ_STATE[$rc]: $out"
chk row1.read-state 1 "$rc" "$out"
out=$(sandbox /bin/sh -c "echo x >> '$ST/ledger.sqlite'"); rc=$?
echo "WRITE_STATE[$rc]: $out"
chk row1.write-state 1 "$rc" "$out"

# --- row5: credentials denied -----------------------------------------------
echo "== row5 credentials denied =="
out=$(sandbox /bin/ls "$HOME/.ssh"); rc=$?
echo "READ_SSH[$rc]: $out"
chk row5.read-ssh 1 "$rc" "$out"
out=$(sandbox /bin/ls "$HOME/.config"); rc=$?
echo "READ_CONFIG[$rc]: $out"
chk row5.read-config 1 "$rc" "$out"
echo '{"token":"DUMMY-SECRET"}' > "$ST/auth.json"
out=$(sandbox /bin/cat "$ST/auth.json"); rc=$?
echo "READ_AUTHJSON[$rc]: $out"
chk row5.read-authjson 1 "$rc" "$out"

# --- row6: protected tests readable, not writable ---------------------------
echo "== row6 protected tests =="
out=$(sandbox /bin/cat "$PROT/guard_test.sh"); rc=$?
echo "READ_PROTECTED[$rc]: $out"
chk row6.read-protected 0 "$rc" "$out"
out=$(sandbox /bin/sh -c "echo tamper >> '$PROT/guard_test.sh'"); rc=$?
echo "WRITE_PROTECTED[$rc]: $out"
chk row6.write-protected 1 "$rc" "$out"
if grep -q "tamper" "$PROT/guard_test.sh"; then echo "RESULT row6.protected-integrity FAIL protected file modified"; else echo "RESULT row6.protected-integrity PASS protected file intact"; fi

# --- row4: shared git metadata denied (no git binary: avoids install prompt) -
echo "== row4 shared git metadata =="
out=$(sandbox /bin/cat "$REPO/.git/HEAD"); rc=$?
echo "READ_GITHEAD[$rc]: $out"
chk row4.read-git-head 1 "$rc" "$out"
out=$(sandbox /bin/cat "$REPO/.git/config"); rc=$?
echo "READ_GITCONFIG[$rc]: $out"
chk row4.read-git-config 1 "$rc" "$out"
out=$(sandbox /bin/ls "$REPO/.git/objects"); rc=$?
echo "READ_GITOBJECTS[$rc]: $out"
chk row4.read-git-objects 1 "$rc" "$out"
out=$(sandbox /bin/sh -c "echo x > '$REPO/.git/s0b_tamper'"); rc=$?
echo "WRITE_GIT[$rc]: $out"
chk row4.write-git 1 "$rc" "$out"
out=$(sandbox /bin/sh -c "cd '$REPO' && ls -a 2>&1"); rc=$?
echo "LS_REPO_AS_CWD[$rc]: $out"
chk row4.cwd-main-denied 1 "$rc" "$out"
