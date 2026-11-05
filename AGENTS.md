# SQLRight in Rust: guide for agents

This file tells an agent what is in this repository, how the parts work
together, and which rules to obey when it changes them.

## What SQLRight is

SQLRight finds logic bugs in a SQL database: queries that run without an
error but return wrong rows. It mutates SQL at the level of a parse tree
(the IR), fills in table and column names that exist in the test case, and
adds pairs of SELECT statements that must return the same result. A pair
that returns different results is a bug report. The method is from the
paper "Detecting Logical Bugs of DBMS with Coverage-based Guidance"
(USENIX Security 2022). The original code was C++ on a fork of AFL.

This repository is a Rust rewrite. Two things changed on purpose:

- AFL++ is used as it is. SQLRight is a custom mutator library
  (`libsqlright_afl.so`) that `afl-fuzz` loads. AFL++ owns the queue, the
  coverage map, the scheduling and parallel instances. SQLRight owns the
  parser, the mutations, the name instantiation, the oracles and the result
  comparison.
- The database under test is tursopg, the PostgreSQL frontend of turso. It
  runs in the same process as a small harness binary that AFL++ starts.

## Layout

```
grammar/postgres/           Bison grammar with IR actions (PostgreSQL 14 syntax),
                            flex scanner, keyword list. The source of truth for the parser.
grammar/sqlite/             The same for SQLite. Not ported yet.
grammar/tools/gen_parser.py Generates the Rust parser tables, actions and IR enums.
corpus/postgres/inputs/     Seed SQL files (PostgreSQL regression tests, cleaned).
corpus/postgres/initlib/    More SQL files for the mutation library.
crates/sqlright-ir/         The IR: IrTree arena, IrNode, IrType, DataType, DataFlag, util.
crates/sqlright-pg-parser/  Lexer, LALR driver, generated tables and actions. parse_query().
crates/sqlright-mutator/    Mutator (libraries, mutation strategies), ir_wrapper, fix
                            (name instantiation), the Oracle trait, reloption generator.
crates/sqlright-oracle/     NOREC, TLP and INDEX oracles; result extraction and comparison.
crates/sqlright-afl/        Engine (the fuzz loop of afl-fuzz.cpp) and the afl_custom_* C API.
crates/sqlright-cli/        The `sqlright` command.
crates/sqlright-harness-tursopg/  The AFL++ target. Its own Cargo workspace (see below).
turso/                      Git submodule: the turso repository.
docs/                       turso-bugs.md (findings), this file's companions.
```

## How a fuzzing run works

1. `sqlright run` starts `afl-fuzz` with the harness as target and the
   environment variables below. The harness is built with `cargo afl build`,
   so AFL++ sees its coverage.
2. AFL++ loads `libsqlright_afl.so` and calls `afl_custom_init`. The engine
   reads every SQL file of the seed directory and the library directories
   into the mutation library (`Mutator::init_from_file`).
3. For a queue entry, AFL++ calls `afl_custom_fuzz_count`. The engine makes
   all test cases of that entry (`Engine::generate_cases`): parse, remove the
   SELECT statements, mutate every IR node with the delete, insert and
   replace strategies, append oracle SELECT statements, fill in names
   (`Mutator::fix_dependency`), apply the oracle rewrites, print.
4. AFL++ calls `afl_custom_fuzz` once per test case and runs the harness on
   it. The harness runs the SQL on a fresh in-memory database and writes the
   result rows to the file named by `SQLRIGHT_OUTPUT`. Its first line is a
   hash of the input.
5. AFL++ calls `afl_custom_post_run`. The engine reads the output file,
   checks the hash, and compares the result blocks of each oracle pair
   (`Engine::check_output`). A difference becomes `bug_<oracle>_<hash>.txt`
   and `.sql` in `SQLRIGHT_BUG_DIR`.
6. When AFL++ saves a test case with new coverage, `afl_custom_queue_new_entry`
   adds its structure to the mutation library.

Multi-run oracles (INDEX) put several runs in one test case, separated by
`-- SQLRIGHT RUN n` lines. The harness starts a new database at each marker
and repeats the marker in its output.

### Environment variables of the mutator

| Variable | Meaning | Default |
|---|---|---|
| `SQLRIGHT_ORACLE` | `NOREC`, `TLP` or `INDEX` | `NOREC` |
| `SQLRIGHT_SEED_DIR` | the afl-fuzz input directory, also used for the library | none |
| `SQLRIGHT_INIT_LIB` | more library directories, separated by `:` | none |
| `SQLRIGHT_OUTPUT` | the file the harness writes | `/tmp/sqlright_output.txt` |
| `SQLRIGHT_BUG_DIR` | where bug reports and `stats_<pid>.txt` go | `sqlright_bugs` |
| `SQLRIGHT_SELECT_COUNT` | oracle SELECTs per test case | 10 |
| `SQLRIGHT_MAX_CASES` | most test cases from one queue entry | 2000 |
| `SQLRIGHT_RNG_SEED` | fixed random seed | the seed AFL++ gives |
| `SQLRIGHT_DEBUG` | messages on stderr | off |

`sqlright run` sets all of them. It also sets `AFL_CUSTOM_MUTATOR_ONLY=1`,
`AFL_DISABLE_TRIM=1` and `AFL_FRAMESHIFT_DISABLE=1`, because byte-level
mutations of SQL text only make statements that do not parse.

## Build

```bash
cargo build --release                 # the workspace: libsqlright_afl.so and the sqlright command
cargo install cargo-afl               # once
cargo afl config --build              # once: builds AFL++ for the pinned toolchain
cd crates/sqlright-harness-tursopg && RUSTFLAGS="-C target-cpu=x86-64-v3" cargo afl build --release
```

`cargo afl build` compiles for the CPU of the build machine
(`-C target-cpu=native`). A container can restart on a machine with a
different CPU. Then the harness stops with an illegal instruction
(SIGILL) on every input, afl-fuzz saves inputs as crashes and sees no
coverage. The `RUSTFLAGS` value comes after the cargo-afl flags, so it
replaces `native` with a common x86-64 level. `sqlright run` runs the
harness on `SELECT 1` before it starts afl-fuzz and stops if the harness
fails.

The harness is a separate workspace because it links turso from the
submodule, and because `cargo afl build` instruments every crate it
compiles. Its `Cargo.lock` is a copy of the turso lock file, so the
dependency versions work with the pinned toolchain (`rust-toolchain.toml`,
the same version turso uses). After a submodule update, copy
`turso/Cargo.lock` over it again.

## Run

```bash
H=crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg
# Seeds: statements that tursopg accepts, 30 per file. Both corpora.
target/release/sqlright seeds --from corpus/postgres/inputs --out work/seeds-pg --harness $H
target/release/sqlright seeds --from turso/postgres/conformance/pg-sqltests --out work/seeds-turso --harness $H
# One afl-fuzz instance for one hour, NOREC oracle.
target/release/sqlright run --oracle NOREC --harness $H --in work/seeds-pg --lib work/seeds-turso --out work/norec --time 3600
# Look at a finding.
target/release/sqlright replay work/norec/bugs/bug_NOREC_<hash>.sql --oracle NOREC --harness $H --raw
target/release/sqlright minimize work/norec/bugs/bug_NOREC_<hash>.sql --oracle NOREC --harness $H
```

The harness filter in `seeds` matters: most statements of the PostgreSQL
regression tests use features tursopg rejects. A test case whose schema
statements fail gives the oracle only errors.

## Regenerate the parser

```bash
python3 -I grammar/tools/gen_parser.py --grammar-dir grammar/postgres \
    --parser-out crates/sqlright-pg-parser/src/generated \
    --ir-out crates/sqlright-ir/src/generated
```

Needs `bison` 3.8. The generator strips the C++ actions from
`bison_parser.y`, runs Bison for the LALR(1) tables, and translates each
action into Rust (`actions.rs`). The lexer is a hand port of
`flex_lexer.l`; change `crates/sqlright-pg-parser/src/lexer.rs` when the
scanner rules change. `IrType` and `DataType` in
`crates/sqlright-ir/src/generated/ir_type.rs` come from `define.h`.

## Tests

```bash
cargo test --release                      # unit tests of every crate
```

The parser was checked against the C++ parser on all 94,840 statements of
the seed corpora: the IR dumps are identical. The C++ `ir_dump` tool is
not in this repository; the `pg-ir-dump` binary prints the same format if
you need to compare again.

## Differences from the C++ code

The port is literal where the C++ was correct. These behaviors differ on
purpose:

- The replace strategy takes nodes from the library. The C++ PostgreSQL
  variant replaced nodes by an empty string literal (an always-empty
  library).
- The statement list keeps the last statement after `append_stmt_at_end`.
  The C++ lost it. `append_stmt_at_idx` inserts at the index; the C++
  inserted after it.
- New queue entries go into the mutation library (the C++ PostgreSQL
  variant had this commented out; the SQLite variant had it).
- When the instantiation step reuses a seen string literal for an int
  literal, the node becomes a string literal (the C++ kept the int type, so
  the string lost its quotes and the statement did not parse).
- `post_fix_transform` keeps the statement types aligned for multi-run
  oracles (the C++ indexed the type list with the wrong index).
- The NoREC rewrite counts with `CASE WHEN (pred) THEN 1 ELSE 0 END`
  instead of `(pred)::INT`. On a database that accepts a non-boolean WHERE
  expression (tursopg does), the cast sums the expression values while
  `WHERE pred` counts rows, which gave hundreds of false failures.
- The TLP oracle does not take a SELECT with a window clause: the C++ took
  `avg(c1) OVER (...)` for an aggregate and compared one row per input row
  with one aggregated value.
- The TLP comparison of a plain SELECT checks the rows as a sorted multiset
  after the row count. The C++ stopped at the count, which misses a wrong
  value in a row.
- The NoREC oracle does not take `count(*) FILTER (WHERE ...)`: the
  rewrite keeps only the WHERE clause, so the filtered count never matched.
- The TLP aggregate comparison parses both results as numbers and allows a
  relative difference of 1e-9: tursopg prints `avg(bigint)` as a double and
  `sum(s) / sum(c)` as an integer. The C++ parsed both as `int` and
  reported an error on overflow.
- The TLP oracle takes a grouped SELECT only when its target is one of the
  GROUP BY expressions. tursopg accepts a target that is neither grouped
  nor aggregated (PostgreSQL rejects it) and returns the value of some row
  of the group, which depends on the plan.
- The TLP oracle checks the GROUP BY target again after the names are
  filled in. The template `SELECT x ... GROUP BY x` passes the first check
  and can become `SELECT c2 ... GROUP BY c1`.
- The TLP oracle does not take a plain, DISTINCT or HAVING SELECT that
  calls an aggregate inside an expression (`CASE WHEN count(*) = 1 ...`).
  Such a query returns one row, and the union of the partitions returns
  one row per partition.
- The TLP rewrite of `avg` is `sum(s)::float8 / sum(c)`. Without the cast,
  the integer division drops the fraction (PostgreSQL does the same).
- The NoREC rewrite finds the FROM, WHERE and ORDER BY nodes of its
  template before it copies the original clauses in. When the WHERE
  clause held a subquery, a later search found the FROM of that
  subquery, and the two queries of a pair read different tables.
- The NoREC oracle removes every non-empty window clause from the
  original form. The C++ removed the first over clause, which is often
  the empty one of another function call (`round(c1)` inside WITHIN
  GROUP). Then `count(*) OVER (ORDER BY sum(c1))` stayed: the aggregate
  makes one group, and the window count is 1 even with no matching row.
- The INDEX oracle keeps an index on an expression or with a WHERE
  clause in every run. The expression runs on each insert and can fail
  (`1 / c1` with `c1 = 0`), so the index rejects rows by design, as in
  PostgreSQL.
- The INDEX oracle compares the first value of a SELECT only when every
  target calls an aggregate and there is no GROUP BY. The C++ looked at
  one target only, so `SELECT c1, count(*)` compared the value of `c1`
  from a row that the plan chooses.
- The mutation library does not take a statement that holds the result
  marker text (`BEGIN VERI n`, `END VERI n`), and a seed with such a
  statement gives no test cases. Saved test cases hold the marker
  statements, the library kept their string literals, and a mutated
  SELECT returned `'END VERI 1'` as a value, which ended its result
  block too early.
- The output of the harness has a first line with the hash of the input.
  The oracle only compares output that belongs to the test case it sent.
- The harness prints the result markers (`SELECT 'BEGIN VERI n'`) itself
  instead of running them. Inside an aborted transaction block every
  statement fails, the marker SELECT too, and the result blocks of two
  pairs ran into each other. For the same reason `sqlright seeds` leaves
  out BEGIN, COMMIT, ROLLBACK and SAVEPOINT statements by default.
- The harness prints `ERROR:  <message>` for a failed statement, like the
  C++ PostgreSQL client did, so the result extraction is unchanged.

## When a campaign reports hundreds of failures

One cause usually makes them all. Group the failing pairs by the shape of
the oracle SELECT before you read reports one by one:

```bash
# For every report: run the harness, find the pairs whose results differ,
# print the first SELECT of each pair with literals and numbers replaced.
python3 -I scripts/group_failures.py "work/norec/bugs/*.sql" 100
```

(`scripts/group_failures.py` is that script: it
splits the `.sql` file and the harness output at the `BEGIN VERI n` and
`END VERI n` markers and groups the mismatching pairs.) The three floods
of the first tursopg campaigns were: a non-boolean WHERE expression that
tursopg accepts (NoREC), window functions taken for aggregates (TLP), and
`count(*) FILTER (...)` (NoREC), lost result markers after a failed
statement in a transaction block (NoREC), a grouped SELECT whose target
is not a grouping expression (TLP), and one value printed as a double and
as an integer (TLP). Each was a SQLRight problem and
was fixed at its source, not by filtering reports. A flood that is a real database
bug gets one entry in `docs/turso-bugs.md` with a minimal reproducer.

## Rules for changes

- Keep the grammar files and the generator the source of truth. Do not edit
  `src/generated/*.rs` by hand.
- Keep the oracle rewrites as they are described in the paper and in the
  oracle files. A change in a rewrite changes what a failure means.
- A test case that AFL++ saves must parse again as a whole. The engine
  only keeps mutations that re-parse.
- Use plain words in names and comments, one idea per sentence. Put a
  function after the functions that call it.
- Do not fix turso bugs here. Write them in `docs/turso-bugs.md` with a
  minimal reproducer.

## Known limits

- The grammar is PostgreSQL 14 syntax (the version SQLRight shipped). The
  `translate` tools in `grammar/postgres/translate/` were used to add IR
  actions to a newer `gram.y`; they have not been run again.
- The SQLite grammar in `grammar/sqlite/` is not ported. The generator is
  written for the PostgreSQL files; the SQLite grammar needs its own lexer
  port.
- tursopg has no `random()`-style non-determinism filter beyond the
  function name list in `crates/sqlright-afl/src/engine.rs`.
- AFL++ reports some instability for the harness (the coverage of a run
  changes with hash map iteration order inside turso). AFL++ handles it; it
  only lowers the value of the coverage feedback.
