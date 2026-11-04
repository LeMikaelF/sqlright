#!/bin/bash
# Minimize every bug report of a campaign and print one line per report.
#
# usage: scripts/triage.sh <campaign-dir> <oracle> [harness]
#
# The small test cases go to <campaign-dir>/min/. A report whose small
# case still fails prints FAIL and the size; a report that does not fail
# again prints FLAKY.
set -u
dir="$1"
oracle="$2"
harness="${3:-crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg}"
sqlright="${SQLRIGHT:-target/release/sqlright}"
mkdir -p "$dir/min"
for case in "$dir"/bugs/bug_*.sql; do
  [ -e "$case" ] || continue
  name=$(basename "$case" .sql)
  out="$dir/min/$name.sql"
  if [ -s "$out" ]; then
    continue
  fi
  if "$sqlright" minimize "$case" --oracle "$oracle" --harness "$harness" --out "$out" >"$dir/min/$name.log" 2>&1; then
    printf "FAIL  %4d statements  %s\n" "$(grep -c ';' "$out")" "$out"
  else
    printf "FLAKY %s (%s)\n" "$case" "$(tail -1 "$dir/min/$name.log")"
    rm -f "$out"
  fi
done
