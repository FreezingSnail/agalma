#!/bin/sh
# S0b probe — row 2: shell descendants inherit confinement.
# Usage: p02-descendants.sh <rundir> [repo]
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
RUN_SH="$S0B/probes/run.sh"
REPO="$(cd "${2:-$S0B/../..}" && pwd -P)"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"

sandbox() { "$RUN_SH" "$A" "$PROT" "$SOCK" "$@" 2>&1; }
chk() { if [ "$2" = "$3" ]; then echo "RESULT $1 PASS exit=$3 :: $4"; else echo "RESULT $1 FAIL want=$2 got=$3 :: $4"; fi; }

# parent-written helper scripts (unconfined), executed by confined descendants
cat > "$A/leaf_read_main.sh" <<EOF
#!/bin/sh
cat "$REPO/docs/architecture.md"
EOF
cat > "$A/leaf_read_git.sh" <<EOF
#!/bin/sh
cat "$REPO/.git/HEAD"
EOF
cat > "$A/leaf_read_ssh.sh" <<EOF
#!/bin/sh
ls "$HOME/.ssh"
EOF
cat > "$A/leaf_write_attempt.sh" <<EOF
#!/bin/sh
echo g > "$A/grand.txt"
EOF
cat > "$A/leaf_read_secret.sh" <<EOF
#!/bin/sh
cat "$RUN/state/auth.json"
EOF
cat > "$A/depth.sh" <<EOF
#!/bin/sh
# depth.sh N leaf -> spawn N nested shells, then exec leaf
n=\$1
if [ "\$n" -gt 0 ]; then
  exec sh "$A/depth.sh" \$((n-1)) "\$2"
fi
exec sh "\$2"
EOF

echo "== row2 descendants =="
out=$(sandbox /bin/sh "$A/depth.sh" 1 "$A/leaf_read_main.sh"); rc=$?
echo "CHILD_READ_MAIN[$rc]: $out"
chk row2.child-read-main 1 "$rc" "$out"

out=$(sandbox /bin/sh "$A/depth.sh" 2 "$A/leaf_write_attempt.sh"); rc=$?
echo "GRANDCHILD_WRITE_ATTEMPT[$rc]: $out"
chk row2.grandchild-write-attempt 0 "$rc" "created=$([ -f "$A/grand.txt" ] && echo yes)"

out=$(sandbox /bin/sh "$A/depth.sh" 2 "$A/leaf_read_git.sh"); rc=$?
echo "GRANDCHILD_READ_GIT[$rc]: $out"
chk row2.grandchild-read-git 1 "$rc" "$out"

out=$(sandbox /bin/sh "$A/depth.sh" 2 "$A/leaf_read_ssh.sh"); rc=$?
echo "GRANDCHILD_READ_SSH[$rc]: $out"
chk row2.grandchild-read-ssh 1 "$rc" "$out"

out=$(sandbox /bin/sh "$A/depth.sh" 5 "$A/leaf_read_git.sh"); rc=$?
echo "DEPTH6_READ_GIT[$rc]: $out"
chk row2.depth6-read-git 1 "$rc" "$out"

out=$(sandbox /bin/sh "$A/depth.sh" 3 "$A/leaf_read_secret.sh"); rc=$?
echo "DEPTH4_READ_SECRET[$rc]: $out"
chk row2.depth4-read-secret 1 "$rc" "$out"

# backgrounded descendant inherits confinement
out=$(sandbox /bin/sh -c "sh '$A/leaf_write_attempt.sh' & wait; echo done"); rc=$?
echo "BACKGROUND_CHILD[$rc]: $out"
chk row2.background-child 0 "$rc" "$out"
