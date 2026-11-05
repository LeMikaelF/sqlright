# SQLRight (Rust, on AFL++)

SQLRight is a fuzzer for logic bugs in SQL databases. It mutates SQL at
the level of a parse tree, fills in names that exist in the test case, and
adds pairs of SELECT statements that must return the same result. When the
results differ, the database has a bug.

This repository is a Rust rewrite of the original C++ SQLRight
(<https://github.com/PSU-Security-Universe/sqlright>, USENIX Security
2022). It runs as a custom mutator of stock AFL++ and, at this time, tests
tursopg: the PostgreSQL frontend of [turso](https://github.com/tursodatabase/turso).

## Build

Requirements: the Rust toolchain named in `rust-toolchain.toml`, `cargo-afl`,
`git` with the `turso` submodule checked out.

```bash
git submodule update --init --depth 1
cargo install cargo-afl
cargo afl config --build
cargo build --release
(cd crates/sqlright-harness-tursopg && cargo afl build --release)
```

## Fuzz tursopg

```bash
H=crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg
target/release/sqlright seeds --from corpus/postgres/inputs --out work/seeds-pg --harness $H
target/release/sqlright seeds --from turso/postgres/conformance/pg-sqltests --out work/seeds-turso --harness $H
target/release/sqlright run --oracle NOREC --harness $H --in work/seeds-pg --lib work/seeds-turso --out work/norec --time 3600 --jobs 2
```

Oracles: `NOREC`, `TLP`, `INDEX` (from SQLRight), `EET`, `DQE`, `LATERAL`
(from SQLancer; see the oracle table in [AGENTS.md](AGENTS.md)). Bug
reports appear in `work/norec/bugs/`
as `bug_<oracle>_<hash>.txt` (the report) and `.sql` (the test case).
`stats_<pid>.txt` in the same directory counts what the mutator did.

## Look at a finding

```bash
target/release/sqlright replay  work/norec/bugs/bug_NOREC_<hash>.sql --oracle NOREC --harness $H --raw
target/release/sqlright minimize work/norec/bugs/bug_NOREC_<hash>.sql --oracle NOREC --harness $H --out small.sql
```

`replay` runs the test case on the harness and prints the oracle verdict.
The `work/norec` and `NOREC` above stand for any of the oracles.
`minimize` removes oracle pairs, statements and parts of statements while
the oracle still fails. `scripts/triage.sh <out-dir> <oracle>` minimizes
every report of a campaign, and `scripts/group_failures.py` groups the
failing pairs of many reports by the shape of their SELECT, which shows
when one cause is behind all of them.

## Commands

| Command | What it does |
|---|---|
| `sqlright print-ir <file>` | parse SQL and print the IR tree |
| `sqlright mutate <seed>` | make test cases from one seed, without AFL++ |
| `sqlright replay <case>` | run a test case on the harness and compare |
| `sqlright minimize <case>` | make a failing test case small |
| `sqlright seeds` | build a seed directory from `.sql` and `.sqltest` files |
| `sqlright run` | start `afl-fuzz` with the SQLRight mutator |

`sqlright <command> --help` lists the options.

## Repository guide

See [AGENTS.md](AGENTS.md) for the layout, the run-time flow, the
environment variables, how to regenerate the parser, and the differences
from the C++ code. Findings about turso are in
[docs/turso-bugs.md](docs/turso-bugs.md).

## License

MIT, like the original SQLRight. The grammar files come from PostgreSQL
(PostgreSQL License) with SQLRight's IR actions.
