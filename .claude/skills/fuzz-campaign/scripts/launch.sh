#!/bin/bash
# Start one detached afl-fuzz instance per oracle.
#
# usage: launch.sh <name> <seconds> <ORACLE>...
#   name     prefix of the output directories: work/<name>-<oracle>
#   seconds  run time of each instance (43200 is 12 hours)
#   ORACLE   NOREC, TLP, INDEX, EET, DQE or LATERAL (one instance each)
#
# The instances survive the end of the shell that started them.
set -euo pipefail
name="$1"; seconds="$2"; shift 2
root="$(git rev-parse --show-toplevel)"
cd "$root"
sqlright="$root/target/release/sqlright"
harness="$root/crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg"
seeds="${SQLRIGHT_SEEDS:-$root/work/seeds}"
[ -x "$sqlright" ] || { echo "build first: cargo build --release" >&2; exit 1; }
[ -x "$harness" ] || { echo "build the harness first (see SKILL.md)" >&2; exit 1; }
[ -d "$seeds" ] || { echo "no seed directory $seeds (set SQLRIGHT_SEEDS or run setup.sh)" >&2; exit 1; }
mem=""
if "$sqlright" run --help 2>/dev/null | grep -q -- "--mem-mb"; then mem="--mem-mb 4096"; fi
for oracle in "$@"; do
  o=$(echo "$oracle" | tr 'A-Z' 'a-z')
  out="$root/work/$name-$o"
  rm -rf "$out"
  # shellcheck disable=SC2086
  setsid nohup "$sqlright" run --oracle "$oracle" --harness "$harness" --in "$seeds" --out "$out" --time "$seconds" --jobs 1 $mem \
    > "$root/work/$name-$o.log" 2>&1 < /dev/null &
  echo "started $oracle -> $out (log: work/$name-$o.log)"
done
sleep 15
echo "afl-fuzz processes: $(pgrep -c -f 'afl-fuzz .*-o ' || true)"
