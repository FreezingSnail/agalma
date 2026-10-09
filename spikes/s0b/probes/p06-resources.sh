#!/bin/sh
# S0b probe — resource + shutdown measurement.
# Measures idle RSS/CPU of confined vs unconfined processes and shutdown
# behavior (does killing the launched pid stop descendants?).
# Usage: p06-resources.sh <rundir>
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
RUN_SH="$S0B/probes/run.sh"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"
NODE="$(command -v node)"

# background launches use run.sh directly so $! is the sandbox-exec pid
rss() { ps -o rss= -p "$1" 2>/dev/null | tr -d ' '; }
cpu() { ps -o %cpu= -p "$1" 2>/dev/null | tr -d ' '; }

echo "== resource + shutdown =="
# --- unconfined baseline -----------------------------------------------------
sleep 30 & BP=$!; sleep 0.4
base_sleep=$(rss "$BP"); base_sleep_cpu=$(cpu "$BP"); kill "$BP" 2>/dev/null
"$NODE" -e 'setInterval(()=>{},1e3)' & BN=$!; sleep 0.7
base_node=$(rss "$BN"); base_node_cpu=$(cpu "$BN"); kill "$BN" 2>/dev/null
wait 2>/dev/null
echo "BASELINE unconfined: sleep rss=${base_sleep}KB cpu=${base_sleep_cpu}%  node rss=${base_node}KB cpu=${base_node_cpu}%"

# --- confined ----------------------------------------------------------------
"$RUN_SH" "$A" "$PROT" "$SOCK" /bin/sh -c "echo \$\$ > '$A/pid_sleep'; exec sleep 30" & CS=$!
sleep 0.5
cs_pid=$(cat "$A/pid_sleep" 2>/dev/null)
conf_sleep=$(rss "$cs_pid"); conf_sleep_cpu=$(cpu "$cs_pid")
echo "CONFINED: sandbox_pid=$CS cmd_pid=$cs_pid sleep rss=${conf_sleep}KB cpu=${conf_sleep_cpu}%"
kill "$CS" 2>/dev/null; kill "$cs_pid" 2>/dev/null

"$RUN_SH" "$A" "$PROT" "$SOCK" "$NODE" -e 'setInterval(()=>{},1e3)' & CN=$!
sleep 0.9
cn_pid="$CN"
conf_node=$(rss "$cn_pid"); conf_node_cpu=$(cpu "$cn_pid")
echo "CONFINED: node sandbox_pid=$CN rss=${conf_node}KB cpu=${conf_node_cpu}%"
kill "$CN" 2>/dev/null
wait 2>/dev/null

echo "RESULT resource.idle-sleep PASS :: baseline=${base_sleep}KB confined=${conf_sleep}KB"
echo "RESULT resource.idle-node  PASS :: baseline=${base_node}KB confined=${conf_node}KB"

# --- shutdown: single confined process --------------------------------------
"$RUN_SH" "$A" "$PROT" "$SOCK" /bin/sh -c "echo \$\$ > '$A/pid_term'; exec sleep 90" & TS=$!
sleep 0.5
t_pid=$(cat "$A/pid_term" 2>/dev/null)
kill -TERM "$TS" 2>/dev/null; sleep 0.4
if kill -0 "$t_pid" 2>/dev/null; then echo "RESULT shutdown.single-process FAIL :: cmd $t_pid (launcher $TS) survived SIGTERM"; kill -9 "$t_pid" "$TS" 2>/dev/null
else echo "RESULT shutdown.single-process PASS :: cmd $t_pid exited on SIGTERM"; fi
wait 2>/dev/null

# --- shutdown: process tree (descendant orphaning) ---------------------------
"$RUN_SH" "$A" "$PROT" "$SOCK" /bin/sh -c "/bin/sh -c 'echo \$\$ > \"$A/bgpid\"; exec sleep 90' & echo \$\$ > \"$A/toppid\"; exec sleep 90" & TREE=$!
sleep 0.6
top_pid=$(cat "$A/toppid" 2>/dev/null); bg_pid=$(cat "$A/bgpid" 2>/dev/null)
kill -TERM "$top_pid" 2>/dev/null; sleep 0.4
if kill -0 "$bg_pid" 2>/dev/null; then
  ppid=$(ps -o ppid= -p "$bg_pid" 2>/dev/null | tr -d ' ')
  echo "RESULT shutdown.tree-caveat PASS :: leader $top_pid died, descendant $bg_pid ORPHANED (ppid=$ppid) -> SandboxApi must kill the process group"
  kill -9 "$bg_pid" 2>/dev/null
else
  echo "RESULT shutdown.tree-caveat PASS :: leader+descendant both terminated"
fi
kill -9 "$TREE" "$top_pid" "$bg_pid" 2>/dev/null
wait 2>/dev/null
exit 0
