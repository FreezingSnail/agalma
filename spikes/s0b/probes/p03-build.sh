#!/bin/sh
# S0b probe — row 3: build scripts (shell running a compile-like command) confined.
# Usage: p03-build.sh <rundir> [repo]
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
RUN_SH="$S0B/probes/run.sh"
REPO="$(cd "${2:-$S0B/../..}" && pwd -P)"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"

sandbox() { "$RUN_SH" "$A" "$PROT" "$SOCK" "$@" 2>&1; }
chk() { if [ "$2" = "$3" ]; then echo "RESULT $1 PASS exit=$3 :: $4"; else echo "RESULT $1 FAIL want=$2 got=$3 :: $4"; fi; }

# --- shell build script: write artifacts into attempt, then try to escape ----
cat > "$A/build_ok.sh" <<EOF
#!/bin/sh
set -e
echo "compiling..."
mkdir -p "$A/build"
echo artifact > "$A/build/out.o"
echo "build ok"
EOF
cat > "$A/build_escape.sh" <<EOF
#!/bin/sh
mkdir -p "$A/build"
echo artifact > "$A/build/out.o"
echo escape > "$REPO/should-not-write.txt"
EOF
cat > "$A/build_steal.sh" <<EOF
#!/bin/sh
cat "$RUN/state/auth.json" > "$A/leak.txt"
EOF
cat > "$A/hello.c" <<'EOF'
#include <stdio.h>
int main(void){ printf("hello build\n"); return 0; }
EOF

echo "== row3 build scripts =="
out=$(sandbox /bin/sh "$A/build_ok.sh"); rc=$?
echo "BUILD_OK[$rc]: $out"
chk row3.build-artifact-in-attempt 0 "$rc" "artifact=$([ -f "$A/build/out.o" ] && echo yes)"

out=$(sandbox /bin/sh "$A/build_escape.sh"); rc=$?
echo "BUILD_ESCAPE[$rc]: $out"
chk row3.build-escape-denied 1 "$rc" "$out"
if [ -e "$REPO/should-not-write.txt" ]; then echo "RESULT row3.build-escape-integrity FAIL repo modified"; else echo "RESULT row3.build-escape-integrity PASS repo intact"; fi

rm -f "$A/leak.txt"
out=$(sandbox /bin/sh "$A/build_steal.sh"); rc=$?
echo "BUILD_STEAL[$rc]: $out"
chk row3.build-steal-credential 1 "$rc" "leak_bytes=$(wc -c < "$A/leak.txt" 2>/dev/null || echo 0)"

# real compile under confinement (clang, toolchain read-only allow)
rm -f "$A/hello"
out=$(sandbox /usr/bin/env TMPDIR="$A" /usr/bin/clang -o "$A/hello" "$A/hello.c"); rc=$?
echo "CLANG_COMPILE[$rc]: $out"
chk row3.clang-compile 0 "$rc" "binary=$([ -x "$A/hello" ] && echo yes)"

out=$(sandbox "$A/hello"); rc=$?
echo "RUN_COMPILED[$rc]: $out"
chk row3.run-compiled 0 "$rc" "$out"

# compiled binary is also confined: cannot read secret
cat > "$A/leaf_secret.sh" <<EOF
#!/bin/sh
cat "$RUN/state/auth.json"
EOF
out=$(sandbox /bin/sh -c "TMPDIR='$A' clang -o '$A/stealer' -x c - <<'CS'
#include <stdio.h>
#include <stdlib.h>
int main(void){ FILE*f=fopen(\"$RUN/state/auth.json\",\"r\"); if(!f){perror(\"open\");return 1;} char b[64]; if(fgets(b,64,f)) printf(\"LEAK:%s\n\",b); return 0; }
CS" 2>&1); rc=$?
echo "CLANG_BUILD_STEALER[$rc]: $out"
if [ -x "$A/stealer" ]; then
  out=$(sandbox "$A/stealer"); rc=$?
  echo "RUN_STEALER[$rc]: $out"
  chk row3.compiled-binary-confined 1 "$rc" "$out"
else
  echo "RESULT row3.compiled-binary-confined BLOCKED :: stealer build failed"
fi
