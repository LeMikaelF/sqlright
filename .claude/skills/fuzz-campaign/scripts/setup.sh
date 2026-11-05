#!/bin/bash
# Set up a fresh sandbox: AFL++, the sqlright build, the harness and the seeds.
# Run from the repository root. Takes about 20 minutes.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
git submodule update --init --depth 1
command -v cargo-afl >/dev/null 2>&1 || cargo install cargo-afl
cargo afl config --build 2>&1 | tail -2 || true
cargo build --release
(cd crates/sqlright-harness-tursopg && RUSTFLAGS="-C target-cpu=x86-64-v3" cargo afl build --release)
H=$PWD/crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg
printf 'SELECT 1;\n' > /tmp/sqlright-one.sql
$H --replay /tmp/sqlright-one.sql | grep -qx '1 *' || { echo "the harness does not run SELECT 1" >&2; exit 1; }
rm -rf work/seeds-pg work/seeds-turso work/seeds
target/release/sqlright seeds --from corpus/postgres/inputs --out work/seeds-pg --harness "$H"
target/release/sqlright seeds --from turso/postgres/conformance/pg-sqltests --out work/seeds-turso --harness "$H"
mkdir -p work/seeds
for f in work/seeds-pg/*; do cp "$f" "work/seeds/pg_$(basename "$f")"; done
for f in work/seeds-turso/*; do cp "$f" "work/seeds/turso_$(basename "$f")"; done
echo "seeds: $(ls work/seeds | wc -l) files in work/seeds"
echo "setup done"
