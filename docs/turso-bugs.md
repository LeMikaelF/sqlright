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

The same happens with a `UNIQUE` index instead of the primary key. The
same happens with a `time` column: `INSERT INTO v0 VALUES (0)` stores
`12:00:00`, `c1 = 0` is false as an expression, and the index finds the
row (12-hour campaign, round 12h-b, NoREC). A `timestamptz` column
with an index does the same: `INSERT INTO t VALUES (2)` stores
`-4713-11-26 12:00:00`, `c = '2'` is false as an expression, and
`WHERE c = '2'` finds the row only through the index (round 12h-b, INDEX).
Text in scientific notation is read as a Julian day too: after
`INSERT INTO t VALUES ('-10e-400')`, `WHERE c = '10e-400'` finds the row
only through the index (campaign of 2026-11-05, INDEX,
`bug_INDEX_5ffdc5174ae637df`).
A `timestamp` column with a plain index does the same: after
`INSERT INTO t VALUES (2)`, `c = 2` is false as an expression, and
`WHERE c = 2` finds the row through the index (campaign of 2026-11-05,
NoREC, `bug_NOREC_3f1369eafa254aac` and `bug_NOREC_4187b2e5c1a1ccee`).
A `timestamp UNIQUE` column does the same (`bug_NOREC_148a396fab798a8d`),
and so does a `timestamptz PRIMARY KEY` column with `WHERE c1 = 1`
(`bug_NOREC_8c2f904b5bddc8c2`, 11:04 UTC).
A `timetz` column with an index does the same with a text literal: after
`INSERT INTO t VALUES ('-10e-400')` (stored as `12:00:00`),
`WHERE c = '-10e-400'` counts 0 rows in a scan and 1 row through the
index. INDEX reports of 08:26 UTC: `bug_INDEX_0d42200cc3259d9d`,
`bug_INDEX_334499c60caf18ae`, `bug_INDEX_40c58e23b26c5f5a`,
`bug_INDEX_4655a20ea20dae1d`, `bug_INDEX_5c719c62f46c1324`,
`bug_INDEX_711f6c866179fa35` (`DELETE ... WHERE c1 = 50`),
`bug_INDEX_71ce57b1e6b8ac75`, `bug_INDEX_81d067b83994601a`,
`bug_INDEX_8376e2dbe70b906b`, `bug_INDEX_b7fd6c80df4af30b`,
`bug_INDEX_c0c3c8c18a2e0445`, `bug_INDEX_c289973e3e905df7`,
`bug_INDEX_dfe7cafaa2d6b3ea`, `bug_INDEX_e4364d3ba6037b77` and
`bug_INDEX_e87f495583b987d2`.
A `date` column accepts `'0.0'` and stores `-4713-11-24`. An index on
`(c1 + 1)` then rejects the same insert with
`invalid date value: -4713-11-24`, because date arithmetic does not
accept that date (`bug_INDEX_8ddd762e68dd7a54`, batch of 09:28 UTC).
PostgreSQL rejects the insert with `invalid input syntax for type date`.
A `jsonb` primary key does the same: after `INSERT INTO t VALUES (1), (2), (6)`,
`WHERE c = 6` counts 1 row and `WHERE NOT (c = 6)` counts 3 rows, so
`sum(c)` over the TLP partitions is 15 instead of 9 (round 12h-b, TLP). The
TLP instance of the campaign of 2026-11-05 found it again: after
`INSERT INTO v0 VALUES (4400)` into a `jsonb PRIMARY KEY` column, the
partitions return the row twice (`bug_TLP_0602e96674489819`,
`bug_TLP_06469310a0e61eab`, `bug_TLP_5bc844d37a3e31e9`,
`bug_TLP_86307e4368968465`, `bug_TLP_afe0d9b35941f0e3`,
`bug_TLP_b1a1d6494b83d06d`). At 10:33 UTC the TLP instance wrote 12
more reports with a `jsonb PRIMARY KEY` column, from one queue entry:
`bug_TLP_06a94864063785c6`, `bug_TLP_076a9784944e3c5f`,
`bug_TLP_208fb8d6d440036a`, `bug_TLP_316f6d9d6cec5d37`,
`bug_TLP_3ac525acee178c19`, `bug_TLP_49e07a562b5ffa9f`,
`bug_TLP_4d8562dfeb691a1d`, `bug_TLP_67e07469fc144154`,
`bug_TLP_6b57657cbb4580e0`, `bug_TLP_c3870b4bc200715c`,
`bug_TLP_d0caf18ec455a14f` and `bug_TLP_d1e63494cc33019c`. With a
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

Issue: https://github.com/tursodatabase/turso/issues/11395. The same
panic occurs in a `VALUES` list and in `UPDATE ... SET`:
`INSERT INTO t VALUES (percentile_cont(1, 0))` and
`UPDATE t SET a = median(a)` (campaign of 2026-11-05, NoREC crash
`id:000008`). PostgreSQL gives `aggregate functions are not allowed in
VALUES` and `... in UPDATE`. A comment on the issue already covers
these forms:
https://github.com/tursodatabase/turso/issues/11395#issuecomment-6648263299.
Crashes `id:000010`, `id:000012` and `id:000015` are the same `VALUES` form.
Count at 07:23 UTC: 4 of the 80 NoREC crashes have this panic. Count at
08:26 UTC: 5 of the 221 NoREC crashes. Count at 09:28 UTC: 5 of the 273
NoREC crashes. Count at 10:32 UTC: 5 of the 347 NoREC crashes.

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

Issues: https://github.com/tursodatabase/turso/issues/11407 (DO UPDATE
is compiled only with an index) and
https://github.com/tursodatabase/turso/issues/9664 (unknown name in
DO UPDATE). Found again on 2026-11-05 (`bug_INDEX_7bdd7c2ec31c66e2`,
`bug_INDEX_1988eb367ad99269`, `bug_INDEX_707f3d23a099f49f`,
`bug_INDEX_ee0a950fc16e90b4`). At 06:20 UTC, 152 reports with
`SET fruit` came from one queue entry. At 08:26 UTC, the count of
`SET fruit` reports is still 152. Later forms: `SET level1_no`
(`bug_INDEX_4cb878e594f11cd4`, `bug_INDEX_cd71494ab578e117`),
`SET value` (`bug_INDEX_81b8b3f09cc3f7e9`) and `SET a`
(`bug_INDEX_d2523676375ffab9`). Batch of 07:23 UTC:
`bug_INDEX_7906bb8760a5989e` (`SET b`), `bug_INDEX_86a5ffea1600cff3`
(`SET a`) and `bug_INDEX_920195ff53002088` (`SET level1_no`). Batch of
09:28 UTC: `bug_INDEX_1b178eed5095c9a5`, `bug_INDEX_1d08af128a4509aa`,
`bug_INDEX_6be6a020c30b4897` and `bug_INDEX_91f13d274e059e28`. Batch of
10:32 UTC: `bug_INDEX_0ca621c3060c9971` (its full test case also hits
the harness timeout; the fixed build gives ERROR),
`bug_INDEX_3667624ba05fd815`, `bug_INDEX_39da718980ce4a7b` and
`bug_INDEX_f9f9bd32753072b6`. Batch of 11:08 UTC: `bug_INDEX_27033558ab1b9507`.

A flood: from 05:20 to 05:22 UTC the INDEX instance wrote 150 reports
in 2 minutes. All 150 hold `ON CONFLICT DO UPDATE SET fruit = ...`. They
are mutations of one queue entry. In every group of the replays, run 0
alone has `no such column: fruit`, so they have the cause of this finding.

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

Issue: https://github.com/tursodatabase/turso/issues/11396. An
expression index does the same: `CREATE INDEX i ON t ((c = 'S'))` on a
`numeric(10, 2)` column makes every insert fail (campaign of
2026-11-05, `bug_INDEX_2821e5961239e774`). Comment:
https://github.com/tursodatabase/turso/issues/11396#issuecomment-6649576708.
Found again as a partial index (`WHERE c1 = 'dump_nulls'` on a
`numeric(7, 1)` column, `bug_INDEX_1f2a49ac2c86aa84`). Batch of 09:28 UTC,
with other literals that are converted only at insert time:
`bug_INDEX_3c014510d5e3e79f` (`WHERE v0.c1 / 'plot'` on a `serial`
column), `bug_INDEX_c4b2d91033a5c23f` (`v0.c1 / '{"a":true,...}'`) and
`bug_INDEX_59bb27fce00a48ac` (`'08:00:2b:ff:fe:01:02:03'::jsonb ->> c2`,
`malformed JSON` at insert). PostgreSQL converts these literals when it
makes the index, so `CREATE INDEX` fails there. Batch of 11:08 UTC:
`bug_INDEX_0f51459077f1ecc4` (`'dump_nulls'`) and
`bug_INDEX_6bceb1168055528b` (`'{"a":[1,2],"c":"b"}'`).

## 10. A very large numeric scale makes an insert run for minutes

tursopg accepts any precision and scale in `NUMERIC(p, s)`. An insert
into such a column takes time that increases with the square of the scale.

```sql
CREATE TABLE t (c NUMERIC(852292321, 852292321));   -- tursopg: accepted
INSERT INTO t VALUES (0.5);                          -- tursopg: does not stop in 60 seconds
```

Measured times for `INSERT INTO t VALUES (0.5)`: 2.0 seconds for a
scale of 1,000,000, 7.4 seconds for 2,000,000 and 30 seconds for
4,000,000. The harness has a limit of 2 seconds for each statement
(`STATEMENT_TIMEOUT`), but the limit does not stop this insert. With a
scale of 1,000,000 the insert runs about 4.5 seconds and gives no
error. Thus AFL++ saves the test case as a hang.

Expected PostgreSQL behavior: `CREATE TABLE` fails with
`NUMERIC precision 852292321 must be between 1 and 1000`.

Found by: the INDEX instance of the 12-hour campaign, as an AFL++ hang
(`default/hangs/`). Found again in the campaign of 2026-11-05 as the
hang `id:000000` of the INDEX instance (`NUMERIC(1849400536, 1849400536)`,
then `INSERT INTO t VALUES ('-10e-400')`, no stop in 20 seconds). Hang
`id:000002` of the INDEX instance is the same problem. So is hang
`id:000000` of the NoREC instance (`NUMERIC(298295755, 298295755)`).

Issue: https://github.com/tursodatabase/turso/issues/11397. Comment on
the 2-second limit:
https://github.com/tursodatabase/turso/issues/11397#issuecomment-6650527672.

## 11. A bare column in a HAVING query without GROUP BY is NULL after a table scan

A query with an aggregate in HAVING and no GROUP BY treats the whole
table as one group. A column that is not inside an aggregate is NULL
in that query when the plan scans the table. With a rowid lookup, the
same column has the value of the row. Thus a WHERE clause that is true
for every row changes the result.

```sql
CREATE TABLE t (a INT);
INSERT INTO t VALUES (5);
SELECT a, a IS NULL FROM t HAVING sum(a) > 0;                    -- tursopg: NULL 1
SELECT a, a IS NULL FROM t WHERE oid = 1 HAVING sum(a) > 0;      -- tursopg: 5 0
SELECT count(*) FROM t HAVING sum(a) > 0 AND a > 0;              -- tursopg: no row
SELECT count(*) FROM t WHERE oid = 1 HAVING sum(a) > 0 AND a > 0; -- tursopg: 1
```

`SELECT a, sum(a) FROM t` without HAVING returns `5 5`. With
`GROUP BY 'x'` the HAVING query returns `5` too. Thus only the path
without GROUP BY loses the column value. A possible cause (not examined
in the turso code): the scan cursor is past the last row when the
program reads the column.

Expected PostgreSQL behavior: all four queries fail with
`column "t.a" must appear in the GROUP BY clause or be used in an
aggregate function`. Finding 5 is the same missing check for GROUP BY.
Given the accepted query, the two plans must still agree. SQLite 3.53.1
rejects the first query with `HAVING clause on a non-aggregate query`.

Found by: the TLP oracle, 12-hour campaign, round 12h-b
(`SELECT * FROM v0 HAVING sum(c2) > v0.c2 * 'c'` against its partition on `oid = 1`).

The INDEX oracle found it again on 2026-11-05 (`bug_INDEX_5b5dffcf76239dca`).
A lookup on the primary key keeps the bare column. A seek in a covering
index on `(a, b)` gives NULL, as a table scan does:

```sql
CREATE TABLE t (a serial PRIMARY KEY, b serial);
INSERT INTO t (a) VALUES (0);
SELECT b IS NULL FROM t WHERE a = 0 HAVING count(*) >= 1;   -- tursopg: 0
CREATE INDEX ti ON t (a, b);
SELECT b IS NULL FROM t WHERE a = 0 HAVING count(*) >= 1;   -- tursopg: 1
```

Related open issue: https://github.com/tursodatabase/turso/issues/7977
(HAVING without GROUP BY that references a bare column). Comment on the
index variant:
https://github.com/tursodatabase/turso/issues/7977#issuecomment-6651332885.

## 12. Crash: make_date with a very large day number

`make_date` and `make_timestamp` panic when the day number moves the
date past the range of the `chrono` date type.

```sql
SELECT make_date(1, 4, 1453556443);
```

```text
thread 'main' panicked at chrono-0.4.42/src/naive/datetime/mod.rs:1635:38:
`NaiveDateTime + TimeDelta` overflowed
```

The panic comes from `dt += d` in `Time::time_date`
(`core/time/internal.rs:249`), called from `time_date_internal`
(`core/time/mod.rs:194`). The code makes sure that the day count fits
in a `chrono::Duration` (`try_days`), but it does not use a checked
addition. A day number of 3,000,000 does not panic.
`SELECT make_timestamp(2024, 1, 1453556443, 0, 0, 0)` panics the same way.

Expected PostgreSQL behavior: the error
`date field value out of range: 1-04-1453556443`.

Found by: the INDEX instance of the 12-hour campaign, round 12h-b, as
an AFL++ crash (`SELECT make_date(TRUE, 4, 1453556443) FROM (VALUES ...)`).

Issue: https://github.com/tursodatabase/turso/issues/11398. The INDEX
instance of the campaign of 2026-11-05 found it again as crash
`id:000000` (`make_date(2013, 2, 1534231376)`, the same panic at
`chrono-0.4.42/src/naive/datetime/mod.rs:1635:38`). Count at 09:28 UTC:
1 of the 273 NoREC crashes and 1 of the 2 INDEX crashes. Count at
10:32 UTC: 1 of the 347 NoREC crashes and 1 of the 2 INDEX crashes.

## 13. make_date returns a blob

In PostgreSQL, `make_date(2024, 1, 1)` returns the date `2024-01-01`.
In tursopg, the name resolves to the function of the turso time
extension, which returns its own binary time value.

```sql
SELECT make_date(2024, 1, 1);           -- tursopg: 13 bytes of binary data   PostgreSQL: 2024-01-01
SELECT typeof(make_date(2024, 1, 1));   -- tursopg: blob
```

Expected PostgreSQL behavior: a value of type `date`.

Found by: the reduction of finding 12.

## 14. Crash: stack overflow on a nested CTE with the same name

A CTE that defines an inner CTE with the same name, and reads it
through a subquery, makes the planner recurse until the stack
overflows.

```sql
WITH t AS (WITH t AS (SELECT 7) SELECT * FROM (SELECT * FROM t))
SELECT * FROM (SELECT * FROM t);
```

```text
thread 'main' has overflowed its stack
fatal runtime error: stack overflow, aborting
```

The stack repeats `prepare_select_plan` (`core/translate/select.rs:174`),
`parse_from` (`core/translate/planner.rs:2489`) and `parse_table`
(`core/translate/planner.rs:2086`, the CTE branch). Thus the `t` in the
inner subquery refers to the outer CTE again, not to the inner one.
Without one of the two subqueries the query returns `7`:
`WITH t AS (WITH t AS (SELECT 7) SELECT * FROM t) SELECT * FROM (SELECT * FROM t)`.

Expected behavior: the result `7`. PostgreSQL gives it, and SQLite
3.53.1 gives it too. The planner is in `core/`, so the SQLite frontend
can have the same crash (not examined).

Found by: the NoREC instance of the 12-hour campaign, round 12h-b, as
an AFL++ crash (an `INSERT INTO v0 WITH x AS (... WITH x AS (...) ...)`
statement with four levels of CTEs named `x`).

## 15. Crash: ALTER TABLE DROP on a table that has only a table constraint

tursopg rejects `CREATE TABLE t ()`, but it accepts a table that has
only a table constraint and no columns. Then `ALTER TABLE ... DROP`
fails an assertion.

```sql
CREATE TABLE t (CONSTRAINT x CHECK (1 > 0));   -- tursopg: accepted, a table with no columns
ALTER TABLE t DROP c;
```

```text
thread 'main' panicked at core/translate/alter.rs:893:13:
assertion `left != right` failed: btree.columns().len() != 0
```

The assertion is in the `DropColumn` branch of the ALTER TABLE
translation ("Tables always have at least one column"). The same table
with `FOREIGN KEY (x) REFERENCES t` is accepted too, although the
column `x` does not exist. `SELECT count(*) FROM t` returns `0`.

A related error message is wrong: for `CREATE TABLE t (a int)`,
`ALTER TABLE t DROP c` gives `cannot drop column "c": no other columns exist`.

Expected PostgreSQL behavior: PostgreSQL permits a table with no
columns, so `CREATE TABLE` succeeds. Then `ALTER TABLE t DROP c` fails
with `column "c" of relation "t" does not exist`. For the foreign key
form, `CREATE TABLE` fails with
`column "x" referenced in foreign key constraint does not exist`.

Found by: the NoREC instance of the 12-hour campaign, round 12h-b, as
an AFL++ crash (`CREATE TABLE v0 (CONSTRAINT x FOREIGN KEY (x, x) REFERENCES v0 ...)`
and `ALTER TABLE v0 DROP c`).

Issue: https://github.com/tursodatabase/turso/issues/11402 (open). Found
again in the campaign of 2026-11-05: 2 of the 6 TLP crashes at 08:26 UTC
(the same assertion at `core/translate/alter.rs:893:13`). Count at
09:28 UTC: 2 of the 8 TLP crashes.

## 16. Casts that PostgreSQL does not have are accepted

tursopg accepts a cast from a scalar to an array type when it prepares
the statement. The cast fails when it runs. In an expression index, the
cast runs for each new row, so every insert into the table fails.

```sql
CREATE TABLE t (a int);
CREATE INDEX ti ON t ((a::text[]));   -- tursopg: accepted
INSERT INTO t VALUES (1);             -- tursopg: ERROR:  cannot store non-array value in TEXT column .
SELECT count(*) FROM t;               -- tursopg: 0 (1 without the index)
SELECT 5::date, 5::uuid;              -- tursopg: -4713-11-29 | 5
```

`true::int[]` fails the same way. `5::timestamp` and `5::interval`
return a value. `translate_cast_expr` (`core/translate/expr/translator.rs`)
examines only the element type of an array target, not the source type.

Expected PostgreSQL behavior: each cast fails when PostgreSQL parses
it, for example `cannot cast type integer to text[]` and
`cannot cast type integer to date` (PostgreSQL 16.15).

A related gap: `CREATE INDEX ui ON u ((b::text[]))` on a `text` column
is accepted. PostgreSQL rejects it with
`functions in index expression must be marked IMMUTABLE`.

Another related problem: a type with two or more declared dimensions
(`text[][]`, `int[2][3]`) rejects every text literal, also `'{a,b}'`.
PostgreSQL ignores the declared dimensions. This is a variant of
https://github.com/tursodatabase/turso/issues/11369.

Found by: the INDEX oracle, campaign of 2026-11-05 (reports
`bug_INDEX_84d7e3aca4b9df8c` and `bug_INDEX_0ef60883160771c9`).

Issue: https://github.com/tursodatabase/turso/issues/11408. The note about
declared dimensions is a comment on
https://github.com/tursodatabase/turso/issues/11369#issuecomment-6648530085.

`bug_INDEX_e0a27a8b058da024` is the same `c1::text[]` index on a
`serial` column. Two more INDEX reports have the same cause
(`bug_INDEX_fdf166b716500aac`, `bug_INDEX_53be71538a8228f9`):
`c::text[]` on a `money` column, and `a::serial[]`. tursopg also
accepts `5::serial`. PostgreSQL gives `type "serial" does not exist`.
Comment on #11408:
https://github.com/tursodatabase/turso/issues/11408#issuecomment-6648571421.
`bug_INDEX_aafe8fc0ea9d36e4` is the `text` to `text[]` index of the
related gap (`c2::text[]` on a `text` column, PostgreSQL: not IMMUTABLE).

Batch of 04:13 UTC: 15 more INDEX reports have an expression index with
a cast. Fourteen of them use `text[]`, `int8[]` or `serial[]` on a `text`
column, or `text[]` on an `int4` or `serial` column. One uses
`c2::timestamptz` on a `text` column. PostgreSQL rejects that index
too (not IMMUTABLE). Comment on #11408:
https://github.com/tursodatabase/turso/issues/11408#issuecomment-6649576030.
Later reports with the same cause: `bug_INDEX_bbc9601b7afe661c`,
`bug_INDEX_ca4e42c00132b585`, `bug_INDEX_42f9435f12d03b3a` and
`bug_INDEX_bf15be9e0abbc82e`. Batch of 05:19 UTC: `bug_INDEX_4427601d464563f1`,
`bug_INDEX_453af712dcc74623` (`timestamptz`), `bug_INDEX_69d2910c20088ac5`,
`bug_INDEX_6d3ee45d508cab11`, `bug_INDEX_b9b3ee67c76aab29` (`timestamp[]`),
`bug_INDEX_bd796b14562b09d3`, `bug_INDEX_d0dc3b970ef3a7b5` (`tsvector[]`),
`bug_INDEX_e01e6b4b1d4cbab2` and `bug_INDEX_eb5535a58eb9f671`.
Batch of 06:20 UTC (also `regtype[]`, `money[]` and `time[]`):
`bug_INDEX_097c57d8ea9b2a30`, `bug_INDEX_1676d9f9c4d2342d`,
`bug_INDEX_1787adeff7623e45`, `bug_INDEX_36dcd46666b0997d`,
`bug_INDEX_371047019e85a14c`, `bug_INDEX_397490e880af715c`,
`bug_INDEX_3c0a35529a243bba`, `bug_INDEX_42ffcff08ed1c71d`,
`bug_INDEX_45330038d03d0e2d`, `bug_INDEX_69b536c6242891ea`,
`bug_INDEX_79a2360bf2c14f5d`, `bug_INDEX_95946abf665467d0`,
`bug_INDEX_a525254bf75efc69`, `bug_INDEX_aae019dab4c487f6`,
`bug_INDEX_afc10e330c47ad3a`, `bug_INDEX_b045992133f28f1c`,
`bug_INDEX_b2740e0160220d7d`, `bug_INDEX_b658605ada52f231`,
`bug_INDEX_bafb5dab90011dea`, `bug_INDEX_c0de3a35a5f08930`,
`bug_INDEX_c36ec4c08adc2d04`, `bug_INDEX_ce0bf51be4bd907c`,
`bug_INDEX_ce1f47eea94e7841`, `bug_INDEX_db3b5a00aa1ca909`,
`bug_INDEX_dd8598a8ff806d69`, `bug_INDEX_eb50c76f8c5d2992`,
`bug_INDEX_ee95f7a9f92094b2`, `bug_INDEX_f24076df1227cdb6` (`timestamptz`)
and `bug_INDEX_f5a863c31d917ecc`. Batch of 07:23 UTC:
`bug_INDEX_1520a498965b5732`, `bug_INDEX_18a8204dfe2e925a`,
`bug_INDEX_3baf49e8ce5bb04f`, `bug_INDEX_5b820570910445d8`,
`bug_INDEX_84d1e9ae9f203339`, `bug_INDEX_a8b4bc05efe12c8d`,
`bug_INDEX_af0e2c2ea71d0b7b`, `bug_INDEX_d3811504e87a81d9`,
`bug_INDEX_d7f2464fad3d75bf` and `bug_INDEX_ffc19a13c686a9fb`. From
07:24 to 07:26 UTC, one queue entry gave 90 more reports, and 2 more of
the same style came later (92 at 10:32 UTC). In each, only
run 0 prints `invalid timestamp value` (an index on `c::timestamptz`
over a `text` column). Reports of 08:26 UTC: `bug_INDEX_1a217873e45c6a45`,
`bug_INDEX_211e839985f3f2a4`, `bug_INDEX_224f556ad2098f7b`,
`bug_INDEX_2f1b0fb578608f1a`, `bug_INDEX_339ec0c5eb82f9f2`,
`bug_INDEX_56bf0cd60e19a576`, `bug_INDEX_5e38d5a4c0c7911e`,
`bug_INDEX_855925882214131e`, `bug_INDEX_890d3800c2ead4e1`,
`bug_INDEX_96eea9408a85248f`, `bug_INDEX_9f066be804e3ff0b`,
`bug_INDEX_ad49400f3969e8ea`, `bug_INDEX_b0edbda1114b3d20`,
`bug_INDEX_b90eecb2eae801ce` (`c2::timestamp`), `bug_INDEX_cfef8533ee4c5aa5`
and `bug_INDEX_e939a8e6cc18eea1`. Batch of 09:28 UTC:
`bug_INDEX_1a423d5bb687ff09`, `bug_INDEX_233a663fe366f189`,
`bug_INDEX_77f2099a21f45a28` (`money` to `serial[]`),
`bug_INDEX_e935842f2fa12a98` and `bug_INDEX_fba159fca61bc472`. Batch of
10:32 UTC: `bug_INDEX_14b73f701b3e89bc`, `bug_INDEX_1512696f24409843`,
`bug_INDEX_322d1d511a8ca39c`, `bug_INDEX_340a2af1bad5a0ff`,
`bug_INDEX_454ff5026133fea9`, `bug_INDEX_62e9c18d4a12350a`,
`bug_INDEX_65ed21361e4edff0`, `bug_INDEX_6bb42074f5a7eb48`,
`bug_INDEX_6ffc24dcaaafed98`, `bug_INDEX_93f3ad7122e06ba0`,
`bug_INDEX_d62395f6fe0fc7d1` and `bug_INDEX_dd531838fa002dad`. Batch of
11:08 UTC (13 reports, also `real[]` and `c2::time`, which PostgreSQL
rejects as not IMMUTABLE): `bug_INDEX_23b876800dbb1b7a`,
`bug_INDEX_61ed4cffb6480fe3`, `bug_INDEX_789190e79e3449a0`,
`bug_INDEX_a50a0242df0ca3e4`, `bug_INDEX_ab90ac6fcc63782f`,
`bug_INDEX_b404f646e965727f`, `bug_INDEX_b6a4172b7cba01ae`,
`bug_INDEX_d961ea556edfcf0f`, `bug_INDEX_dece4ac06f367ad5`,
`bug_INDEX_e0a840ab74017cdf` (`time`), `bug_INDEX_e62eccc5582950e4`,
`bug_INDEX_f44d477ec6e2e065` and `bug_INDEX_fc89e6bef329db38`.

## 17. Crash: a multi-row VALUES list with rows of different lengths

tursopg compares only the first row of a multi-row `VALUES` list with
the column count. A longer row later in the list writes past the
register file and panics.

```sql
CREATE TABLE t (a int);
INSERT INTO t VALUES (1), (1, 2, 3, 4, 5, 6, 7);
```

```text
thread 'main' panicked at core/vdbe/execute.rs:5910:5:
index out of bounds: the len is 7 but the index is 7
```

With a text value at the end of the row, the panic is in `op_string8`
(`core/vdbe/execute.rs:6257`). A shorter row takes the missing values
from the row before it. The SQLite parser rejects such a list
(`sqlite/parser/src/parser.rs`). The PostgreSQL frontend does not, and
the `Values` branch of `prepare_select_plan` (`core/translate/select.rs`)
uses `values[0].len()` without a check.

Expected PostgreSQL behavior: `VALUES lists must all be the same length`.

Found by: the NoREC instance, campaign of 2026-11-05, as two AFL++
crashes (signal 6, `id:000000` and `id:000001`). The second crash panics
in `op_string8` on `INSERT INTO v0 VALUES (1, 'p'), (2, 'q'), ('sun',
'mon', 'tue', 'wed', 'thu', NULL, 'fri')` into a table with 2 columns.

Issue: https://github.com/tursodatabase/turso/issues/11403 (already open).

Two more crashes (`id:000002`, `id:000003`) have the same cause. One
panics in `op_real` (`core/vdbe/execute.rs:6227`). The other is a bare
`VALUES` statement without `INSERT`: `VALUES (1), (1, 2, 3, 4, 5, 6, 'x')`
panics too. Comment:
https://github.com/tursodatabase/turso/issues/11403#issuecomment-6649575149.
Crashes `id:000005` (`op_real`), `id:000006` (`op_integer`) and
`id:000007` (`op_string8`) have the same cause. So do `id:000009`
(`op_real`) and `id:000011` (`op_string8`). Count at 07:23 UTC: 12 of
the 80 NoREC crashes (`execute.rs` lines 5910, 6227 and 6257) and both
TLP crashes have this cause. Count at 08:26 UTC: 14 of the 221 NoREC
crashes and 4 of the 6 TLP crashes. Count at 09:28 UTC: 18 of the 273
NoREC crashes (15 at the three known lines and 3 more `VALUES` forms),
5 of the 8 TLP crashes and 1 of the 2 INDEX crashes (`execute.rs:5910`).
Count at 10:32 UTC: 18 of the 347 NoREC crashes (15 plus 3 more `VALUES`
forms), 5 of the 8 TLP crashes and 1 of the 2 INDEX crashes.

## 18. integer / text column is accepted

tursopg accepts `1 / b` on a `text` column. It reads the text as a
number. In an expression index, the row `'0.0'` then fails with
`division by zero`.

```sql
CREATE TABLE t (b text);
CREATE INDEX i ON t ((1 / b));   -- tursopg: accepted
INSERT INTO t VALUES ('0.0');    -- tursopg: ERROR:  division by zero
```

Expected PostgreSQL behavior: `operator does not exist: integer / text`.
On an `integer` column, PostgreSQL gives the same insert error with the
same index. Thus, the different insert result is correct in itself.

Found by: the INDEX oracle, campaign of 2026-11-05 (3 reports:
`bug_INDEX_1653c8fb60014856`, `bug_INDEX_589d514e28fe54ca`,
`bug_INDEX_5e7279dacd3879bb`; later `bug_INDEX_d4fbfa9b03fa97be`). A fourth report (`bug_INDEX_8fb1f3fb9837add0`)
has `1 / c1` on a `serial` column and `INSERT ... VALUES ('0.0')`.
tursopg stores `'0.0'` as `0` in an integer column. PostgreSQL rejects
it (`invalid input syntax for type integer: "0.0"`), close to
https://github.com/tursodatabase/turso/issues/10263. Crash `id:000004`
is finding 17 again. Batch of 06:20 UTC: `bug_INDEX_01ab37bf0fd3927f`
and `bug_INDEX_be4170a2959fde02` (`1 / c`), and
`bug_INDEX_21eb0c518ab6193f`. The last one has a partial index
`WHERE v0.c2 / '1 year'` on a `text` column. Every insert then fails
with `invalid input syntax for type numeric: "1 year"`. Batch of
07:23 UTC: `bug_INDEX_4141aa86b51de78b` (a partial index
`WHERE v0.c2 / '::2:ffff:ffff:ffff:ffff/128'`) and
`bug_INDEX_964bc9968458d23f` (an index on `c1 % '192.168.1.255/6'::text`).
Reports of 08:26 UTC: `bug_INDEX_191daab77c2e66c3` (a partial index
`WHERE v0.c2 / '{2,180,...}'`), `bug_INDEX_91a23ac6a5e82aa9` and
`bug_INDEX_b622ad2cf7311d1c` (`1 / c1`).

Batch of 11:08 UTC: `bug_INDEX_1549b3e94da32994` (`division by zero`).
A burst of 28 reports from 10:52 to 10:57 UTC came from one queue entry.
Each has a partial index `WHERE v0.c2 = 'dump_nulls' AND lcm(c1, - c2)`.
tursopg accepts the unary `-` on a `text` column (`- 'abc'::text` gives
`0`), and `lcm` then overflows at insert (`integer overflow`).
PostgreSQL gives `operator does not exist: - text`. Examples:
`bug_INDEX_0082843f0bdad20d`, `bug_INDEX_13a7ae2747b423df`,
`bug_INDEX_9d7c8a9ec94c4ac2`, `bug_INDEX_ff5a7cf69c4a9a2c`. Comment:
https://github.com/tursodatabase/turso/issues/11387#issuecomment-6656461559.

Issue: a variant of https://github.com/tursodatabase/turso/issues/11387.
Comment:
https://github.com/tursodatabase/turso/issues/11387#issuecomment-6649577460.

## 19. The serial counter keeps an explicit value after a ROLLBACK

An explicit value in a `serial` column moves the counter of the column.
The counter keeps that value after a failed insert, a `DELETE` and a
`ROLLBACK`.

```sql
CREATE TABLE w (a serial PRIMARY KEY, b text);
BEGIN;
INSERT INTO w VALUES (90, 'b');
ROLLBACK;
INSERT INTO w (b) VALUES ('z');
SELECT a, b FROM w;               -- tursopg: 91 z   PostgreSQL: 1 z
```

Expected PostgreSQL behavior: an explicit value does not change the
sequence, so the new row gets `1`.

Found by: the INDEX oracle, campaign of 2026-11-05
(`bug_INDEX_1779ab36aa0d3ed1`). The run with the index hit the 2-second
statement timeout of the harness on an `INSERT ... SELECT` of 100,000
rows. The rows were rolled back, but the counter was not. That report
is also a SQLRight false positive (a timeout in one run only).

Issue: a variant of https://github.com/tursodatabase/turso/issues/9615.
Comment:
https://github.com/tursodatabase/turso/issues/9615#issuecomment-6649784267.

## 20. A partial index on `oid` makes every insert fail

```sql
CREATE TABLE t (a serial, b text);
CREATE INDEX ti ON t (b) WHERE oid = 1;   -- tursopg: accepted
INSERT INTO t (b) VALUES ('x');           -- tursopg: ERROR:  no such column: oid
```

Expected PostgreSQL behavior: `CREATE INDEX` fails with
`column "oid" does not exist`.

Found by: the INDEX oracle, campaign of 2026-11-05
(`bug_INDEX_b7f64e48f26695ac`). Later: `bug_INDEX_1704168220bbe1f5`
(`oid = 70`), and `WHERE oid LIKE '"%"'` in `bug_INDEX_97d76fc4b07760f5`,
`bug_INDEX_9bf29920b7cb4c9f`, `bug_INDEX_9dae627b38f5a1f3` and
`bug_INDEX_c3b093f37a41c17f`. Later: `bug_INDEX_942f1a02d8543551`
(`WHERE oid = 'cr_owner'`), `bug_INDEX_0bee8ad86590ab36` and
`bug_INDEX_29c3ca95a502cf8e`. Batch of 10:32 UTC:
`bug_INDEX_6485a2dad42b35fb` (`oid = '17.35177750493897715514'`). Batch of
11:08 UTC (10 reports): `bug_INDEX_022197452292a1ee`,
`bug_INDEX_1001f725b9f856ff`, `bug_INDEX_1ee85b99d583b41c`,
`bug_INDEX_3e42493a5f64bd22`, `bug_INDEX_4d082225c01e7dc5`,
`bug_INDEX_4e554e0eb8a6579c`, `bug_INDEX_52083cf69d0e8f91`,
`bug_INDEX_68d37717614561a2`, `bug_INDEX_6d462f2a669cfb58` and
`bug_INDEX_ba74ac6e5889dfdc`.

Issue: a variant of https://github.com/tursodatabase/turso/issues/9663
(the same problem with `rowid`). Comment:
https://github.com/tursodatabase/turso/issues/9663#issuecomment-6649784812.

## 21. DELETE with a table alias fails when an index uses a qualified column

tursopg accepts an expression index with a qualified column. After
that, a `DELETE` with a table alias fails. An `UPDATE` with a table
alias fails when it changes a column of the index expression.

```sql
CREATE TABLE v0 (a int, b int);
INSERT INTO v0 VALUES (1, 1);
CREATE INDEX vi ON v0 ((v0.b + 1));
DELETE FROM v0 t;            -- tursopg: ERROR:  no such table: v0
UPDATE v0 AS t SET b = 5;    -- tursopg: ERROR:  no such table: v0
```

Without the alias, or without the index, both statements succeed. The
stored index expression keeps the `v0.` qualifier, and the DML program
looks for it under the alias `t`. The SQLite frontend rejects the index
(#6699), but the PostgreSQL frontend does not.

Expected PostgreSQL behavior: the index is accepted, and `DELETE FROM
v0 t` deletes 1 row (PostgreSQL 16.15).

Found by: the INDEX oracle, campaign of 2026-11-05
(`bug_INDEX_9643460f6e12096a`).

Issue: https://github.com/tursodatabase/turso/issues/11420.

## 22. Crash: an array type with 8 dimensions

A column type or a cast type with more than 7 array dimensions fails an
assertion in `set_array_dimensions` (`core/schema.rs`).

```sql
CREATE TABLE t (c int[][][][][][][][]);
SELECT '{1}'::int[][][][][][][][];
```

```text
thread 'main' panicked at core/schema.rs:10746:13:
array dimensions must be <= 7
```

`ALTER TABLE t ADD COLUMN d int[][][][][][][][]` panics too.

Expected PostgreSQL behavior: PostgreSQL ignores the declared number of
dimensions. PostgreSQL 16.15 accepts the `CREATE TABLE` and returns
`{1}` for the cast.

Found by: the NoREC instance, campaign of 2026-11-05, as AFL++ crashes
`id:000013` and `id:000014`.

Issue: https://github.com/tursodatabase/turso/issues/11416 (`CREATE TABLE`).
A comment already covers the cast and `ALTER TABLE`:
https://github.com/tursodatabase/turso/issues/11416#issuecomment-6650371427.
The issue was closed at 06:32 UTC on 2026-11-05 by pull request
https://github.com/tursodatabase/turso/pull/11423 (merged), which rejects
types with more than 7 dimensions. The fix is newer than the turso commit
under test (ff97c22), so the crash is fixed upstream.

Count at 07:23 UTC: 64 of the 80 NoREC crashes have this panic. They
came from one queue entry. AFL++ ran that entry again and again, and the
NoREC instance lost about 30 minutes on it. Count at 08:26 UTC: 202 of
the 221 NoREC crashes. Count at 09:28 UTC: 249 of the 273 NoREC crashes
and 1 of the 8 TLP crashes. Count at 10:32 UTC: 323 of the 347 NoREC
crashes and 1 of the 8 TLP crashes.

## 23. LIKE reads the whole string when the first characters differ

The statement below runs for longer than the 2-second statement limit
of the harness. AFL++ saved it as a hang in the INDEX instance
(`id:000001`, campaign of 2026-11-05).

```sql
INSERT INTO v0 (c1) VALUES (repeat('*', 537704593) LIKE repeat('?', 20000));
```

In tursopg, `repeat('*', 537704593)` takes 1.75 seconds.
`repeat('*', 50000000) LIKE repeat('?', 20000)` takes about 1 second,
and the time grows linearly with the length of the string. The pattern
has no wildcard (`?` is not a wildcard in LIKE), and the first character
differs. Thus, the result is false after one comparison.

PostgreSQL 16.15 gives `f`. Most of its time goes to `repeat`:
`length(repeat('*', 537704593))` takes 8.3 seconds, and the LIKE query
takes 7.6 seconds in all. For 50,000,000 characters, `repeat` takes
0.35 seconds and the LIKE query 0.30 seconds. Thus, the LIKE itself
takes almost no time in PostgreSQL.

The statement does not stop at the 2-second limit of the harness. This is
a performance difference, not a wrong result. Issue: none.

Other hangs of the same kind (a very long string; the harness limit of
2 seconds cannot stop the statement):

| Hang | Statement | tursopg | PostgreSQL 16.15 |
|---|---|---|---|
| TLP `id:000000` | `SELECT quote_literal(lpad('sample', '563964475')) FROM v0` | no end in 40 s | `ERROR:  requested length too large` at once |
| — | `quote_literal(lpad('sample', 56396447))` | 3.6 s | 0.79 s (with `length()`) |
| NoREC `id:000002` | `INSERT ... VALUES (1, repeat('1234567890', 29329162)), ...` (293 MB) | over 2 s | `length(repeat(...))` 1.1 s |
| NoREC `id:000001` | a SELECT with `repeat('1234567890', 82999375)` (830 MB) | over 2 s | `length(repeat(...))` 8.4 s |

The tursopg times are the coordinator's measurements, and the
PostgreSQL times are mine. PostgreSQL is slow for the `repeat`
statements too. The `lpad` case differs: PostgreSQL rejects a result
longer than 1 GB at once, because it reserves 4 bytes for each
character. tursopg starts to build the string. NoREC hang `id:000000` is
finding 10.

Two more NoREC hangs build a long string with `lpad` in a multi-row
`INSERT ... RETURNING`:

- `id:000003`: `quote_literal(lpad('hi', 529537728, '*'))` (530 MB).
- `id:000004`: `INSERT INTO v7 VALUES (1, 'p'), (quote_literal(lpad('hi', 671437373, '30')), 'q'), (3, 'r') RETURNING v7.*`
  (671 MB). Prefix bisection with a limit of 8 seconds: statement 6 is
  the first slow statement.

In PostgreSQL 16.15, `lpad('hi', 529537728, '*')` fails at once with
`requested length too large`. A length of 268,435,455 fails the same way.

## 24. An IN list with two spellings of one value returns the row twice through an index

```sql
CREATE TABLE a (c time);
INSERT INTO a VALUES ('12:00:00');
CREATE INDEX ai ON a (c);
SELECT count(*) FROM a WHERE c IN ('12:00:00', '12:00');   -- tursopg: 2 (1 without the index)
```

The same happens with `timetz`, `timestamp` and `uuid` (lowercase and
uppercase forms of one UUID). `integer` and `numeric` columns give 1.
`open_in_seek_source_cursor` (`core/translate/main_loop/in_seek.rs`)
puts the literals into a unique ephemeral index before they are
encoded. Then each literal is encoded and gives one seek. Two literals
with the same encoded value find the row twice.

Expected PostgreSQL behavior: `1` (PostgreSQL 16.15).

Found by: the INDEX oracle, campaign of 2026-11-05
(`bug_INDEX_cdc44c1a94399e68`: a `timetz` column and
`WHERE c1 IN ('-10e-400', '0.0', 9.096544)`).

Issue: https://github.com/tursodatabase/turso/issues/11453.

## 25. A cast from float8 to int8 has no range check

```sql
SELECT '9223372036854775807'::float8::int8;   -- tursopg: 9223372036854775807
SELECT 3e9::float8::int4;                     -- tursopg: 3000000000
```

Expected PostgreSQL behavior: `bigint out of range` and
`integer out of range` (PostgreSQL 16.15).

Found by: the INDEX oracle, campaign of 2026-11-05
(`bug_INDEX_f2846adf2d7c68d5`). The test case inserts the cast value
into a `serial PRIMARY KEY` column, so the row gets the largest rowid.
The next `serial` value is then a random rowid, as in SQLite. Thus, the
two runs differ without an index effect. That part is a SQLRight
false positive (a non-deterministic rowid).

Issue: a variant of https://github.com/tursodatabase/turso/issues/8337
(the cast side of the int4 range check; #10442 is the column side).
Comment:
https://github.com/tursodatabase/turso/issues/8337#issuecomment-6656463096.

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

### Campaign of 2026-11-05 (12 hours, in progress)

Three instances (NoREC, TLP, INDEX) on tursopg at submodule commit ff97c22.
The final numbers go here when the campaign ends. SQLRight found five
false-positive classes in the first 8 hours. Each one is fixed at its
source and has a unit test (the fourth one has a test of its helper only).
The running instances use the old library, so they can still write these
reports.

| Class | Oracle | Reports | Fix |
|---|---|---|---|
| a query holds the literal `'BEGIN VERI 1'`, and the error message repeats it | NoREC | 1 | the result extraction takes a marker only as a whole line |
| a statement hits the 2-second limit of the harness in one run only | INDEX | 1 | a Fail with `ERROR:  interrupt` in the output becomes Error |
| `max(a, b)` with two arguments taken for an aggregate | INDEX | 6 | `ir_wrapper::is_scalar_min_max` (also used by TLP) |
| a row gets the largest rowid, and the next `serial` value is random | INDEX | 1 (`bug_INDEX_f2846adf2d7c68d5`) | `is_non_deterministic` takes the literal `9223372036854775807` as non-deterministic |
| a query returns the row `END VERI 1`, which looks like a marker (`SELECT 'END VERI 1' FROM t`), or a stray marker statement sits among the normal statements | TLP | 3 (`bug_TLP_c5b14ce82655389a` at 10:33 UTC turns from FAIL to ERROR when the marker literals are renamed) | the mutator does not learn or reuse marker literals, and it does not add marker statements to the library (`is_marker_text` and `is_marker_statement` in `fix.rs`) |
