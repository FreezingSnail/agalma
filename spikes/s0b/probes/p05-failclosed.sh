#!/bin/sh
# S0b probe — row 10: sandbox initialization fails closed.
# A malformed/missing/empty/undefined-param profile MUST make sandbox-exec
# refuse to run the command. The command is a "fails open" canary that would
# create a file OUTSIDE any allowed area if it ever executed unconfined.
# Usage: p05-failclosed.sh <rundir> [repo]
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
REPO="$(cd "${2:-$S0B/../..}" && pwd -P)"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"
SX=/usr/bin/sandbox-exec

chk() { if [ "$2" = "$3" ]; then echo "RESULT $1 PASS exit=$3 :: $4"; else echo "RESULT $1 FAIL want=$2 got=$3 :: $4"; fi; }

# build the fail-open canary profile that WOULD allow the attempt dir, so the
# only reason the canary cannot run is sandbox init failure.
canary="$REPO/s0b_failopen_canary.txt"

run_case() { # name profile
  name="$1"; prof="$2"
  rm -f "$canary"
  out=$("$SX" -f "$prof" -D "ATTEMPT=$A" -D "PROTECTED=$PROT" -D "SOCKDIR=$SOCK" \
        /bin/sh -c "echo FAILOPEN > '$canary'" 2>&1); rc=$?
  echo "CASE $name rc=$rc out=$out canary=$([ -e "$canary" ] && echo PRESENT || echo absent)"
  if [ "$rc" -ne 0 ] && [ ! -e "$canary" ]; then
    echo "RESULT $name PASS exit=$rc :: refused, canary absent"
  else
    echo "RESULT $name FAIL exit=$rc :: canary=$([ -e "$canary" ] && echo PRESENT || echo absent)"
  fi
}

echo "== row10 fail-closed =="
run_case row10.malformed-profile "$S0B/profiles/malformed.sb"
run_case row10.missing-profile "$S0B/profiles/does-not-exist.sb"
: > "$S0B/profiles/empty.sb"    # deliberate: empty profile fixture
run_case row10.empty-profile "$S0B/profiles/empty.sb"
run_case row10.undefined-param "$S0B/profiles/undefined-param.sb"

# malformed profile passed inline via -p
rm -f "$canary"
out=$("$SX" -p '(version 1) (deny default) (bogus-operator invalid)' \
      /bin/sh -c "echo FAILOPEN > '$canary'" 2>&1); rc=$?
echo "CASE row10.bad-inline-profile rc=$rc out=$out canary=$([ -e "$canary" ] && echo PRESENT || echo absent)"
if [ "$rc" -ne 0 ] && [ ! -e "$canary" ]; then
  echo "RESULT row10.bad-inline-profile PASS exit=$rc :: refused, canary absent"
else
  echo "RESULT row10.bad-inline-profile FAIL exit=$rc :: canary=$([ -e "$canary" ] && echo PRESENT || echo absent)"
fi

# a well-formed profile must still run a benign command (sanity: not just always-fail)
rm -f "$canary"
out=$("$SX" -f "$S0B/profiles/worker.sb" -D "ATTEMPT=$A" -D "PROTECTED=$PROT" -D "SOCKDIR=$SOCK" -D "EXTRA_RO=$A" \
      /bin/sh -c "cd '$A' && echo OK > '$A/p05-ok.txt' && echo ran" 2>&1); rc=$?
echo "SANITY_GOOD[$rc]: $out"
chk row10.wellformed-runs 0 "$rc" "ok_file=$([ -f "$A/p05-ok.txt" ] && echo yes)"
rm -f "$canary"
