# turso bugs found with SQLRight

Findings from running the SQLRight rewrite against tursopg (the PostgreSQL
frontend of turso). The turso version is the submodule commit in this
repository. Each entry has a minimal SQL reproducer. Run a reproducer with:

```bash
printf '<sql>' > repro.sql
crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg --replay repro.sql
```

SQLRight does not fix turso bugs. Issues were opened on 2026-11-04 for the
findings that no existing issue covered: #11385 (finding 1), #11386
(finding 4), #11387 (findings 2, 3 and 5 together) and #11388 (finding 6).

## 1. Crash: extension aggregate in a GROUP BY query

`percentile_cont` (an extension aggregate) inside a `GROUP BY` query
panics with a slice index error in `core/ext/function.rs`
(`ExtFunc::step_aggregate`, the `Self::Aggregate` branch):

```sql
CREATE TABLE t(a float8);
INSERT INTO t VALUES (0.5);
SELECT percentile_cont(a, a) FROM t GROUP BY a;
```

```text
thread 'main' panicked at core/ext/function.rs:111:45:
range end index 16 out of range for slice of length 15
```

The index numbers in the message change with the program. The same query
without `GROUP BY` returns `0.5`. `SELECT percentile_cont(0.5, a) FROM t
GROUP BY a` panics too, so the crash needs the extension aggregate and a
`GROUP BY`, not two column arguments. The register range of the aggregate
arguments (`registers[start_reg..start_reg + aggregate.argc]`) goes past
the end of the register file in the grouped path.

Issue: https://github.com/tursodatabase/turso/issues/11385

Found by: the AFL++ dry run on a seed from the PostgreSQL regression test
`aggregates.sql` (the original statement is
`select p, percentile_cont(p,p) from generate_series(1,5) x, (values (0::float8),(0.1),...) v(p) group by p order by p;`).

## 2. WHERE accepts a non-boolean expression

PostgreSQL rejects a WHERE expression that is not boolean
(`argument of WHERE must be type boolean, not type integer`). tursopg
accepts it and treats the value like SQLite does: zero and NULL are false,
everything else is true.

```sql
CREATE TABLE t (c1 INT);
INSERT INTO t VALUES (1), (2), (3), (0), (NULL);
SELECT count(*) FROM t WHERE c1;      -- tursopg: 3      PostgreSQL: error
SELECT count(*) FROM t WHERE 2.5;     -- tursopg: 5      PostgreSQL: error
SELECT count(*) FROM t WHERE 'abc';   -- tursopg: 0      PostgreSQL: error
SELECT NOT 5, CASE WHEN 5 THEN 'yes' ELSE 'no' END;   -- tursopg: 0 yes   PostgreSQL: error
```

Found by: the NoREC oracle. The first campaign reported this more than 800
times because one library statement had the predicate below (bug 3), and
`SUM((pred)::INT)` summed the integer values while `WHERE pred` counted
the rows. The NoREC rewrite now uses `CASE WHEN`, so the oracle no longer
depends on strict boolean typing.

Issue: https://github.com/tursodatabase/turso/issues/11387 (with findings 3 and 5)

## 3. Bit shift with an array operand is accepted

PostgreSQL has no `integer << integer[]` operator. tursopg evaluates it
and returns the left operand, as if the array were 0.

```sql
SELECT 5 << ARRAY[1, 2];                                        -- tursopg: 5   PostgreSQL: error
SELECT 5 << array_cat(ARRAY[[2, 4], [4, 10]], ARRAY[1, 2]);     -- tursopg: 5   PostgreSQL: error
```

Found by: the NoREC oracle (see bug 2).

Issue: https://github.com/tursodatabase/turso/issues/11387 (with findings 2 and 5).
Related: https://github.com/tursodatabase/turso/issues/11368 covers `+`, `-` and `*` on arrays.

## 4. Index lookup and expression disagree on a `date` primary key

An integer inserted into a `date` column is stored as the date of that
Julian day. After that, `WHERE c1 = 0` finds the row through the primary
key index, but the same comparison is false when it is evaluated as an
expression. Without the index the count is 0.

```sql
CREATE TABLE v0 (c1 date PRIMARY KEY);
INSERT INTO v0 VALUES (0);
SELECT c1, c1 = 0 FROM v0;             -- -4713-11-24 0      (the comparison is false)
SELECT count(*) FROM v0 WHERE c1 = 0;  -- 1                  (the index finds the row)
CREATE TABLE v1 (c1 date);
INSERT INTO v1 VALUES (0);
SELECT count(*) FROM v1 WHERE c1 = 0;  -- 0                  (no index: consistent with the expression)
```

The same happens with a `UNIQUE` index instead of the primary key. With a
valid date (`INSERT INTO v0 VALUES ('2024-01-02')`) both forms agree.
PostgreSQL rejects the insert (`column "c1" is of type date but expression
is of type integer`), so the first problem is the accepted insert; given
the accepted row, the index and the expression must still agree.

Related: with the index, `WHERE c1 = 'abc'` fails with `invalid date
value`; without the index the same query returns 0 rows.

Found by: the NoREC oracle, round 3 (the only report of that round that
was not a transaction-marker false positive).

Issue: https://github.com/tursodatabase/turso/issues/11386.
Related: https://github.com/tursodatabase/turso/issues/11011 (the `c1 = 'abc'` case) and
https://github.com/tursodatabase/turso/issues/11314 (tracking issue for comparisons and index lookups).

## 5. A column that is neither grouped nor aggregated is accepted with GROUP BY

PostgreSQL rejects a select target that is not in the GROUP BY clause and
not inside an aggregate (`column "v4.c6" must appear in the GROUP BY
clause or be used in an aggregate function`). tursopg accepts it and
returns the value of one row of the group, like SQLite does. Which row
depends on the plan: the same query with a different WHERE clause gives
another value for the same group.

```sql
CREATE TABLE v4 (c5 INT, c6 INT);
INSERT INTO v4 VALUES (1, 10), (2, 20), (2, 30), (2, 40), (5, 50);
SELECT c6 FROM v4 GROUP BY c5;                        -- tursopg: 10 20 50        PostgreSQL: error
SELECT c6 FROM v4 WHERE NOT (c6 = 30) GROUP BY c5;    -- tursopg: 10 20 50
SELECT c6 FROM v4 WHERE c6 = 30 GROUP BY c5;          -- tursopg: 30
```

Found by: the TLP oracle, round 4 (two reports). The TLP oracle now takes
a grouped SELECT only when its target is one of the GROUP BY expressions.

Issue: https://github.com/tursodatabase/turso/issues/11387 (with findings 2 and 3).
Related: https://github.com/tursodatabase/turso/issues/5709 asks for an option that enforces this.

## 6. avg() over integers returns double precision

PostgreSQL computes `avg` over `integer` or `bigint` as `numeric`, so the
result is exact. tursopg returns a double, which loses digits above 2^53.

```sql
CREATE TABLE t (c bigint);
INSERT INTO t VALUES (-2773227799603160576), (-2773227799603160576);
SELECT avg(c) FROM t;             -- tursopg: -2.77322779960316e+18    PostgreSQL: -2773227799603160576.00000000000000000000
SELECT sum(c) / count(c) FROM t;  -- tursopg: -2773227799603160576
```

Found by: the TLP oracle, round 5 (90 reports with the same two-way
print of one value). The TLP aggregate comparison now compares numbers
with a relative tolerance.

Issue: https://github.com/tursodatabase/turso/issues/11388.
Related: https://github.com/tursodatabase/turso/issues/9605 covers `sum()` and `avg()` of `numeric(p,s)` columns.

## 7. Crash: extension aggregate in a WHERE clause

An extension aggregate (`percentile_cont`, `median`, `stddev`) in a
WHERE clause panics. A built-in aggregate in the same place gives an
error. The planner does not reject the extension aggregate, so the
program calls it as a scalar function.

```sql
CREATE TABLE t (a INTEGER);
INSERT INTO t VALUES (1);
SELECT a FROM t WHERE median(a) = 1;
```

```text
thread 'main' panicked at core/ext/function.rs:91:18:
internal error: entered unreachable code: aggregate called in scalar context
```

The panic is in `ExtFunc::call_scalar`, called from `op_function`.
`SELECT a FROM t WHERE sum(a) = 1` gives `misuse of aggregate function sum()`.
`SELECT 1 WHERE percentile_cont(0.5, 1) = 1` panics too, so a table is
not necessary.

Expected PostgreSQL behavior: the error `aggregate functions are not
allowed in WHERE`.

This is not finding 1. Finding 1 is a slice index panic in
`ExtFunc::step_aggregate` with GROUP BY.

Found by: the NoREC oracle, 12-hour campaign. AFL++ saved it as a crash
after 88 seconds (`SELECT count(*) FROM v0 WHERE c1 @> percentile_cont(c2, c1)`).

## 8. ON CONFLICT DO UPDATE SET is not checked on a table without an index

An unknown column in the SET list of `ON CONFLICT DO UPDATE` gives an
error only when the table has an index. On a table without an index,
the statement inserts the row. Thus a `CREATE INDEX` changes whether
an insert succeeds.

```sql
CREATE TABLE t (a INTEGER, b INTEGER);
INSERT INTO t VALUES (6, 7) ON CONFLICT DO UPDATE SET nosuch = t.a;   -- tursopg: inserts the row
SELECT count(*) FROM t;                                               -- tursopg: 1
CREATE TABLE u (a INTEGER, b INTEGER);
CREATE INDEX ub ON u (b);
INSERT INTO u VALUES (6, 7) ON CONFLICT DO UPDATE SET nosuch = u.a;   -- tursopg: ERROR:  no such column: nosuch
SELECT count(*) FROM u;                                               -- tursopg: 0
```

A table with a primary key gives the error too. tursopg also accepts
`ON CONFLICT DO UPDATE` without a conflict target.

Expected PostgreSQL behavior: both inserts fail. Without a conflict
target, the error is `ON CONFLICT DO UPDATE requires inference
specification or constraint name`. With a target, an unknown column
gives `column "nosuch" of relation "t" does not exist`.

Found by: the INDEX oracle, 12-hour campaign (the row counts of the run
with indexes and the run without indexes were different).

## 9. A partial index with an invalid literal makes every insert fail

tursopg accepts a partial index whose predicate compares a `numeric`
column with a string that is not a number. Then every insert into the
table fails. Without the index, the same insert succeeds.

```sql
CREATE TABLE t (c NUMERIC(10, 1));
CREATE INDEX i ON t (c) WHERE c > 'x';     -- tursopg: accepted
INSERT INTO t VALUES (1.2);                -- tursopg: ERROR:  invalid input syntax for type numeric: "x"
SELECT count(*) FROM t;                    -- tursopg: 0 (1 without the index)
```

The same index on an `integer` column does not block the insert.

Expected PostgreSQL behavior: `CREATE INDEX` fails with
`invalid input syntax for type numeric: "x"`, because PostgreSQL converts
the literal when it makes the index.

Found by: the INDEX oracle, 12-hour campaign (one of three reports with
the index rows `1.2` and `1.25`).

## 10. A very large numeric scale makes an insert run for minutes

tursopg accepts any precision and scale in `NUMERIC(p, s)`. An insert
into such a column takes time that increases with the square of the scale.

```sql
CREATE TABLE t (c NUMERIC(852292321, 852292321));   -- tursopg: accepted
INSERT INTO t VALUES (0.5);                          -- tursopg: does not stop in 60 seconds
```

Measured times for `INSERT INTO t VALUES (0.5)`: 2.0 seconds for a
scale of 1,000,000, 7.4 seconds for 2,000,000 and 30 seconds for
4,000,000. The harness has no time limit of its own, so AFL++ saves the
test case as a hang.

Expected PostgreSQL behavior: `CREATE TABLE` fails with
`NUMERIC precision 852292321 must be between 1 and 1000`.

Found by: the INDEX instance of the 12-hour campaign, as an AFL++ hang
(`default/hangs/`).

## Campaign log

tursopg at submodule commit ff97c22, 4 cores, one `afl-fuzz` instance per
oracle. Seeds: the SQLRight PostgreSQL corpus and turso's `pg-sqltests`,
filtered to statements tursopg accepts (`sqlright seeds --harness`). The
mutation library is the same set.

| Round | Oracle | Runs | Reports | What they were |
|---|---|---|---|---|
| 1 | NoREC | 10k | 813 | all `WHERE c1 << array_cat(...)`: finding 3 plus finding 2, through the `(pred)::INT` rewrite |
| 1 | TLP | 16k | 244 | all window functions taken for aggregates (SQLRight bug) |
| 2 | NoREC | 13k | 126 | all `count(*) FILTER (...)` (SQLRight bug) |
| 2 | TLP | 11k | 0 | |
| 3 | NoREC | 51k | 6 | 5 lost result markers after a failed statement in a transaction block (SQLRight bug), 1 real: finding 4 |
| 3 | TLP | 13k | 0 | |
| 4 | TLP | 14k | 2 | both a grouped SELECT with an ungrouped target: finding 5 |
| 5 | TLP | 5k | 108 | all `avg(bigint)` printed as a double against an integer: finding 6 |
| 4 | NoREC | 161k | 0 | 108,809 test cases, 96,545 comparable pairs, no failure (60 min, seeds without transaction statements, harness prints the markers) |
| 6 | TLP | 65k | 0 | 25,130 test cases, 20,365 comparable pairs, no failure (50 min) |
| 1 | INDEX | 52k | 0 | 23,199 multi-run test cases, 19,976 comparable blocks, no failure, no crash, no hang (120 min) |

The INDEX oracle ran once for two hours with the first harness build. The
NoREC and TLP rounds after the first used the harness that prints the
result markers itself and the seeds without transaction statements.
