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
Issues #11395 to #11408 came from the 12-hour campaign of 2026-11-05.
#11403 (a VALUES row with a different length) and #11408 (casts that
PostgreSQL does not have) have no entry in this file.
NOREC crash 000000 (src 002220) of that campaign is #11403: a longer VALUES row
panics in `op_string8` (`index out of bounds: the len is 9 but the index is 9`).
NOREC crashes 000001 (src 002305) and 000004 (src 002636) are the same panic.
camp-index2 crash 000000 (src 001852) is the same problem, with the panic in `op_integer`
(`execute.rs:5910`), only in the run without indexes.
camp-index2 crash 000001 (src 001876) is the same problem, with the panic in `op_real`
(`execute.rs:6227`).

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
A `jsonb` primary key does the same: after `INSERT INTO t VALUES (1), (2), (6)`,
`WHERE c = 6` counts 1 row and `WHERE NOT (c = 6)` counts 3 rows, so
`sum(c)` over the TLP partitions is 15 instead of 9 (round 12h-b, TLP). The text
`'1.2345678901234e-200'` in an indexed `timestamptz` column does the same (two
TLP reports of camp-tlp2, 2026-11-05). A `TIMESTAMP` primary key with
`INSERT ... VALUES (2, ...)` does the same (the first NoREC report of that
campaign, `bug_NOREC_64871ceb9d417f2f`). The first camp-index3 report
(`bug_INDEX_743ddb801784aa92`) is the `date` case with `WHERE c10 = '30'`. The
first camp-tlp3 report (`bug_TLP_fec843579ba6d2de`) is a `timetz` primary key:
after `INSERT ... VALUES (5)`, `WHERE c = '7'` finds the row through the index,
because every integer day gives `12:00:00`. All six reports of camp-norec2
(`c7 date PRIMARY KEY`, `WHERE c7 = '7'`) are the original `date` case. With a
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

The WHERE clause of DELETE and UPDATE panics too, and so does the window
form `median(a) OVER ()` in WHERE (NoREC crash 000002, comment
https://github.com/tursodatabase/turso/issues/11395#issuecomment-6650104597).

This is not finding 1. Finding 1 is a slice index panic in
`ExtFunc::step_aggregate` with GROUP BY.

Found by: the NoREC oracle, 12-hour campaign. AFL++ saved it as a crash
after 88 seconds (`SELECT count(*) FROM v0 WHERE c1 @> percentile_cont(c2, c1)`).

Issue: https://github.com/tursodatabase/turso/issues/11395

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

Issue: https://github.com/tursodatabase/turso/issues/11407.
Related: https://github.com/tursodatabase/turso/issues/9664 (an unknown name in DO UPDATE SET).

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

Issue: https://github.com/tursodatabase/turso/issues/11396
The first camp-index4 report (`bug_INDEX_5ebf3ca377cc1deb`) is the same problem, with
`WHERE v0.c2 = 'quoted table'` on a `NUMERIC(210, 10)` column.

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

Issue: https://github.com/tursodatabase/turso/issues/11397

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

Issue: https://github.com/tursodatabase/turso/issues/7977 covers it (comment
https://github.com/tursodatabase/turso/issues/7977#issuecomment-6648759035).
Open fixes: https://github.com/tursodatabase/turso/pull/10598 and https://github.com/tursodatabase/turso/pull/10544.

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

Issue: https://github.com/tursodatabase/turso/issues/11398 (#11400 is a duplicate, closed).

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

Issue: https://github.com/tursodatabase/turso/issues/9609 covers it.

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

Issue: https://github.com/tursodatabase/turso/issues/11401

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

Issue: https://github.com/tursodatabase/turso/issues/11402
camp-tlp3 crash 000000 (src 002806) is the same assertion
(`CREATE TABLE v0 (FOREIGN KEY (x, x) REFERENCES v0 (x, x))`, then `ALTER TABLE v0 DROP COLUMN c3`).

## 16. LIKE on a long string uses about 5 times its size in memory

tursopg's LIKE copies both operands into a `Vec<char>` (4 bytes per
character) before it compares them. A LIKE with a pattern that is not
a constant is slow and uses much memory.

```sql
SELECT '' LIKE repeat('s', 200000000);              -- 3.6 s, 1174 MB
SELECT repeat('s', 200000000) = repeat('t', 1);     -- 0.14 s, 219 MB
```

The copy is in `exec_textlike` (`postgres/frontend/functions.rs`). A
constant pattern takes another path and is fast. With a pattern of
632,696,932 characters, one LIKE takes 11.4 seconds and almost 4 GB.

Expected PostgreSQL behavior: LIKE reads the strings in place, with no
copy.

Found by: the INDEX instance of the 12-hour campaign of 2026-11-05, as an
AFL++ hang. The test case has two runs, so the LIKE ran twice, for 23
seconds in total. The AFL++ time limit is 20 seconds.

Issue: https://github.com/tursodatabase/turso/issues/11411

A longer pattern aborts the process under a 4 GB memory limit:
`SELECT '' LIKE repeat('2', 863178415)` stops with `memory allocation of
3452713680 bytes failed` (AFL++ crash, INDEX instance). Comment:
https://github.com/tursodatabase/turso/issues/11411#issuecomment-6649235187
INDEX crashes 000004 to 000006 (src 002167) are the same abort, with an operand of more than 700 million characters on either side of LIKE.
INDEX hangs 000002 and 000003 (src 002881) are the same LIKE: one aborts, one takes 10 seconds.
INDEX crashes 000007 and 000008 are the same LIKE abort.

## 17. Arithmetic with a text column is accepted, and an index on it blocks inserts

PostgreSQL has no `integer / text` operator. tursopg accepts it. In an
expression index, the division runs for each new row, so a row with a
text that is not a number cannot be inserted. Without the index, the
insert succeeds.

```sql
CREATE TABLE t (a int, b text);
CREATE INDEX ti ON t ((1 / b));
INSERT INTO t VALUES (1, 'p');   -- tursopg: ERROR:  invalid input syntax for type numeric: "p"
SELECT 1 + 'p'::text;            -- tursopg: 1 ('p' is read as 0)
```

`/` reads the text as a numeric and fails. `+` reads it as 0. Thus an
index on `(a + b)` does not block the insert.

Expected PostgreSQL behavior: `CREATE INDEX` fails with
`operator does not exist: integer / text`.

Found by: the INDEX oracle, 12-hour campaign of 2026-11-05
(`CREATE INDEX i6 ON v0 ((1 / c4))`).

Issue: https://github.com/tursodatabase/turso/issues/11387 covers it, as one
more expression that PostgreSQL rejects (comment
https://github.com/tursodatabase/turso/issues/11387#issuecomment-6649270385).
Related: https://github.com/tursodatabase/turso/issues/10618 (an untyped literal, `'x' / 1`).

`text = boolean` is accepted too (PostgreSQL: `operator does not exist: text =
boolean`). With a long string of digits it is slow: `SELECT repeat('2',
976796771) = FALSE` takes 12.3 seconds. NoREC hangs 000000 and 000001 of the
12-hour campaign had this cause. Comment:
https://github.com/tursodatabase/turso/issues/11387#issuecomment-6652189285

## 18. lpad and rpad are about 11 times slower than repeat

```sql
SELECT length(lpad('nan', 910408173));    -- 22.5 s, 1765 MB
SELECT length(repeat(' ', 910408173));    -- 1.9 s, 897 MB
```

`pad` in `core/functions/string.rs` adds the padding one character at a
time, then copies it again with `format!`. The 1 GB size limit works:
`lpad('nan', 1000000001)` fails with `string or blob too big`.

Found by: the TLP instance of the 12-hour campaign of 2026-11-05, as an
AFL++ hang (`c9 = lpad('nan', '910408173')` in three partitions).

Issue: https://github.com/tursodatabase/turso/issues/11417

## 19. Crash: a column type or cast with more than 7 array dimensions

```sql
CREATE TABLE v0 (c1 INT, c2 INT[][][][][][][1200][3]);
SELECT NULL::int[][][][][][][][];
```

```text
thread 'main' panicked at core/schema.rs:10746:13:
array dimensions must be <= 7
```

`map_pg_type` (`postgres/parser/translator.rs`) counts the `[]` suffixes
without the `MAX_ARRAY_DIMENSIONS` check that the SQLite parser makes.
Then `set_array_dimensions` fails its assertion.

Issue: https://github.com/tursodatabase/turso/issues/11416 (closed on
2026-11-05 by https://github.com/tursodatabase/turso/pull/11423, after
ff97c22). camp-tlp3 crash 000001 (src 004577) found it again.

## 20. A partial index on `oid` makes every insert fail

```sql
CREATE TABLE t (c text);
CREATE INDEX i ON t (c) WHERE oid > 0;   -- tursopg: accepted
INSERT INTO t VALUES ('x');              -- tursopg: ERROR:  no such column: oid
```

The same happens with `rowid`. Issue:
https://github.com/tursodatabase/turso/issues/9663 (open, its comment has the
`oid` form). Found again by camp-index4 (`bug_INDEX_d183e4aa6bc84b93`,
`CREATE INDEX i6 ON v0 (c1) WHERE COALESCE(oid, 1) > 0`).

## Not turso bugs: harness and oracle limits

- A statement that returns several gigabytes, for example
  `SELECT repeat(1, 737393991) FROM v0` on a table with 6 rows, aborts
  the harness with `memory allocation of 5899154112 bytes failed`. The
  harness keeps all output in one `Vec<u8>`, and the vector doubles when
  it grows. PostgreSQL also sends these rows to the client. Three AFL++
  crashes of the 12-hour campaign of 2026-11-05 had this cause.
- A string literal that is the same as a result marker (`'END VERI 1'`)
  is printed as a row. Then the oracle takes the row for the marker and
  compares the wrong blocks. The instantiation step takes this literal
  from the marker statements. One TLP report of the same campaign had
  this cause. With the literal changed, the test case passes.
  A second TLP report (`bug_TLP_d7f1618f37722191`) had the same cause. The
  engine now removes the marker statements of a queue entry before it adds
  the entry to the mutation library. A running instance uses the fix only
  after a restart with a rebuilt `libsqlright_afl.so`.
  TLP reports `bug_TLP_dfd1111479ded61c` and `bug_TLP_850d9d3fcbe3cabe` came
  from the old library and have the same cause.
  NoREC report `bug_NOREC_08c86f7b2460e9c6` too: there the literal comes out
  in an error message that prints the statement AST.
- TLP crash 000000 (src 003184) is `quote_literal(lpad('temp', 704328054))`.
  `quote_literal` asks for 2 bytes per character (1,408,656,110 bytes) and
  the process aborts under the 4 GB limit. PostgreSQL asks for the same
  size and refuses it, because one allocation is limited to 1 GB.

- A window function without ORDER BY in its OVER clause, for example
  `avg(c8) OVER (ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING)`, gives values
  that depend on the row order. An index on `c8` changes the scan order,
  so the INDEX oracle saw a difference (`bug_INDEX_cce4e92ce76bb8a2`,
  camp-index2). Both results are correct. 40 more reports of camp-index2
  had the same cause (`min(c) OVER (ROWS BETWEEN 2 FOLLOWING AND 3
  FOLLOWING)`). The INDEX oracle compared only the row count of a window
  query, but a row that is all NULL prints as an empty line, which the
  count leaves out. The INDEX oracle now does not take a SELECT with a
  window function.

- The non-deterministic filter matched function names as substrings. The
  aggregate name `sum_int_randomrestart` (from a `CREATE AGGREGATE` of the
  PostgreSQL regression tests) contains `random`. camp-index3 had 2928
  queue entries with that statement, and it skipped 14,233 of 14,399 test
  cases. The filter now matches whole names only.

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
