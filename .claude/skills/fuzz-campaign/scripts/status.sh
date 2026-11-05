#!/bin/bash
# One status line per instance of a campaign, plus the free memory.
#
# usage: status.sh <name>     (the name given to launch.sh)
set -u
name="${1:-camp}"
root="$(git rev-parse --show-toplevel)"
cd "$root/work" || exit 1
for d in "$name"-*/; do
  d="${d%/}"
  [ -d "$d/default" ] || continue
  st=$(cat "$d"/bugs/stats_*.txt 2>/dev/null | tr '\n' ' ')
  get() { echo "$st" | grep -o "$1: [0-9]*" | head -1 | awk '{print $NF}'; }
  fs="$d/default/fuzzer_stats"
  printf "%-16s run=%ss execs=%s checked=%s pass=%s fail=%s error=%s reports=%s crashes=%s hangs=%s\n" "$d" \
    "$(grep -h run_time "$fs" 2>/dev/null | awk '{print $3}')" \
    "$(grep -h execs_done "$fs" 2>/dev/null | awk '{print $3}')" \
    "$(get 'cases checked')" "$(get 'oracle pass')" "$(get 'oracle fail')" "$(get 'oracle error')" \
    "$(ls "$d"/bugs/bug_*.sql 2>/dev/null | wc -l)" \
    "$(ls "$d/default/crashes" 2>/dev/null | grep -c '^id:')" \
    "$(ls "$d/default/hangs" 2>/dev/null | grep -c '^id:')"
done
echo "afl-fuzz processes: $(pgrep -c -f 'afl-fuzz .*-o ' || true)   free memory: $(free -g | awk '/Mem:/{print $7}') GB"
