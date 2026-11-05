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
A `jsonb` primary key does the same: after `INSERT INTO t VALUES (1), (2), (6)`,
`WHERE c = 6` counts 1 row and `WHERE NOT (c = 6)` counts 3 rows, so
`sum(c)` over the TLP partitions is 15 instead of 9 (round 12h-b, TLP). With a
valid date (`INSERT INTO v0 VALUES ('2024-01-02')`) both forms agree.
PostgreSQL rejects the insert (`column "c1" is of type date but expression
is of type integer`), so the first problem is the accepted insert; given
the accepted row, the index and the expression must still agree.

Related: with the index, `WHERE c1 = 'abc'` fails with `invalid date
value`; without the index the same query returns 0 rows.

Found by: the NoREC oracle, round 3 (the only report of that round that
was not a transaction-marker false positive).

Variant, found by the EET oracle in the campaign of 2026-11-05: a plain
`timestamp` column with a normal (not unique) index and an integer
constant. `INSERT INTO v0 VALUES (1601)` stores `-4708-04-12 12:00:00`.
Then `WHERE c2 = 1601` finds the row only through `CREATE INDEX ON v0 (c2)`,
and `SELECT c2 = 1601` is false. The variant is a comment on issue 11386:
https://github.com/tursodatabase/turso/issues/11386#issuecomment-6653231258

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
The LATERAL oracle found it again in the campaign of 2026-11-05.

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

SQLite 3.53.1 inserts the row when the index is not unique, and fails
only with a unique index. A non-unique index cannot cause a conflict, so
SQLite does not compile the DO UPDATE branch. Issue #10393 (fixed by
#10639, included in ff97c22) made turso compile the branch only when a
constraint check can jump to it. But `build_constraints_to_check` in
`core/translate/insert.rs` adds every index, also the non-unique ones.
The INDEX campaign of 2026-11-05 found the same difference again after
3 minutes.

Issue: https://github.com/tursodatabase/turso/issues/11407

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

## 16. Crash: a VALUES row with more values than the table has columns

tursopg compares only the first row of a multi-row `VALUES` list with
the column count. A shorter row takes the missing values from the row
before it. A longer row loses its extra values. A row with many extra
values writes past the register file and panics.

```sql
CREATE TABLE t (a INT, b INT);
INSERT INTO t VALUES (1, 2), (3);                       -- tursopg: stores 3 2
INSERT INTO t VALUES (1, 2), (3, 4, 5, 6, 7, 8, 9);     -- tursopg: panics
```

```text
thread 'main' panicked at core/vdbe/execute.rs:5910:5:
index out of bounds: the len is 9 but the index is 9
```

The instruction that panics changes with the type of the extra value:
`op_integer` (line 5910), `op_real` (line 6227) or `op_string8`
(line 6257).

Expected PostgreSQL behavior: the error
`VALUES lists must all be the same length`.

Issue: https://github.com/tursodatabase/turso/issues/11403 (it existed
before this campaign).

Found by: the NoREC instance of the campaign of 2026-11-05, as two
AFL++ crashes after 21 minutes.

## 17. An integer expression compared with a string literal is never equal

A bare integer column compared with a string literal works: tursopg
converts the literal to the type of the column. An integer expression
(`a + 0`, `abs(a)`, `CASE WHEN TRUE THEN a END`) compared with the same
literal uses the SQLite type order instead. An integer is always less
than a text, so `=`, `>=`, `IN` and `BETWEEN` are false and `<` is true.

```sql
CREATE TABLE t (a INT);
INSERT INTO t VALUES (1), (3);
SELECT a FROM t WHERE a >= '1';                           -- 1, 3
SELECT a FROM t WHERE (a + 0) >= '1';                     -- no row
SELECT a FROM t WHERE CASE WHEN TRUE THEN a END = '3';    -- no row
SELECT a, (a + 0) >= '1', -a < '0' FROM t;                -- 1 0 1 and 3 0 1
```

A `FLOAT8` or `TEXT` column does not show the problem, and
`(a + 0) >= '1'::int` works.

Expected PostgreSQL behavior: the untyped literal takes the type of the
other operand, so every query returns both rows.

The same happens with other column types and literals. A `polygon`
column compared with the integer `0` is true for a polygon row
(PostgreSQL has no such operator), and the wrapped column is false:

```sql
CREATE TABLE v0 (c1 int4, c3 polygon);
INSERT INTO v0 VALUES (1, '(3.0,1.0),(4.0,2.0)'::polygon);
SELECT count(*) FROM v0 WHERE c3 = 0;                            -- 1
SELECT count(*) FROM v0 WHERE (CASE WHEN TRUE THEN c3 END) = 0;  -- 0
```

Found by: the EET oracle, in the first minutes of the 12-hour campaign
of 2026-11-05 (every report that wrapped an operand of a comparison in
`CASE WHEN ... END`). The EET oracle now leaves every operand of a
comparison alone, with a comment that names the issue, so that the
campaign can find other problems.

Issue: https://github.com/tursodatabase/turso/issues/11415

## 18. Crash: CREATE TABLE with 8 array dimensions

A column type with 8 `[]` panics in `CREATE TABLE`. A type with 7 `[]`
works. The panic also occurs with a domain as the base type.

```sql
CREATE TABLE v3 (c4 INT[][][][][][][][]);
```

```text
thread 'main' panicked at core/schema.rs:10746:13:
array dimensions must be <= 7
```

The call path is `apply_written_type`, then `set_array_dimensions`.

Expected PostgreSQL behavior: PostgreSQL does not use the number of
dimensions of a declared column type, so the statement works. If turso
keeps a limit of 7, it must return an error and not panic.

Related: https://github.com/tursodatabase/turso/issues/10851 (the same
limit when turso opens a database made by SQLite).

Found by: the DQE oracle, as an AFL++ crash after 30 minutes. The EET
instance found it again (46 crashes in all).

Issue: https://github.com/tursodatabase/turso/issues/11416

## 19. Crash: UPDATE of a primary key column when WHERE names a RETURNING alias

An UPDATE that changes a column of a composite primary key panics when
the WHERE clause uses a name that is only an output alias of RETURNING.

```sql
CREATE TABLE v0 (c1 INT, c2 INT, c3 INT, PRIMARY KEY (c1, c2));
UPDATE v0 SET c1 = 6 WHERE a5 = 2 RETURNING c3 AS a5;
```

```text
thread 'main' panicked at core/translate/expr/translator.rs:2626:33:
column 2 of table t1 is not stored in the index and the table cursor is not open
```

The panic needs all of these: `SET` changes a primary key column, WHERE
uses the alias, and the alias is for a column that is not in the primary
key. It does not occur with `SET c3 = 6`, with `RETURNING c2 AS a5`, with
a table without a primary key, or with a `SELECT`.

Expected PostgreSQL behavior: the error `column "a5" does not exist`.

Found by: the LATERAL oracle, as two AFL++ crashes (the statement is in
the setup of the test case).

Issue: https://github.com/tursodatabase/turso/issues/11421

## 20. A text constant and a `timestamp` column: table scan and index disagree

A `timestamp` column compared with a text constant that has no time part
is false in a table scan and true through an index.

```sql
CREATE TABLE v0 (c2 TIMESTAMP);
INSERT INTO v0 VALUES ('2026-10-08');
SELECT c2 = '2026-10-08' FROM v0;                    -- 0
SELECT c2 = '2026-10-08'::timestamp FROM v0;         -- 1
SELECT count(*) FROM v0 WHERE c2 = '2026-10-08';     -- 0
CREATE INDEX ON v0 (c2);
SELECT count(*) FROM v0 WHERE c2 = '2026-10-08';     -- 1
```

The scan compares the stored text `2026-10-08 00:00:00` with the text of the
constant. Expected PostgreSQL behavior: every query gives 1.

Found by: the EET oracle. A CASE around the predicate turns the index
lookup into an expression, and the two forms return different rows. This
is a variant of an open issue, so it is a comment:
https://github.com/tursodatabase/turso/issues/11011#issuecomment-6655290026

Issue: https://github.com/tursodatabase/turso/issues/11011

## 21. An expression index changes a comparison with TRUE or FALSE

In a scan, tursopg compares text with `TRUE` or `FALSE` as a number:
`'1' = TRUE` and `'0' = FALSE` are true. An index on an expression does
not use this conversion, so the index lookup finds no row.

```sql
CREATE TABLE t (c text);
INSERT INTO t VALUES ('1');
SELECT upper(c) = TRUE FROM t;                    -- 1
CREATE INDEX i ON t ((upper(c)));
SELECT count(*) FROM t WHERE upper(c) = TRUE;     -- 0 (1 without the index)
SELECT count(*) FROM t WHERE NOT (upper(c) = TRUE); -- 0
```

An index on `(c || '')` does the same. An index on the plain column `c`
does not change `WHERE c = TRUE`. PostgreSQL rejects `text = boolean`.

Found by: the TLP instance of the 12-hour campaign of 2026-11-05
(`SELECT * FROM v0` returned 4 rows, the partitions on
`upper(c1) = TRUE` returned 2).

Issue: https://github.com/tursodatabase/turso/issues/11436

## 22. Crash: a FROM subquery reads a CTE column without a FROM clause

A FROM subquery without a FROM clause names a column of a CTE. When
another subquery reads the CTE, turso panics in the code generation.

```sql
WITH c AS (SELECT 5 AS x) SELECT * FROM (SELECT x WHERE EXISTS (SELECT 1 FROM c));
```

```text
thread 'main' panicked at core/translate/expr/translator.rs:2771:22:
Outer query subquery result_columns_start_reg must be set in program
```

Without `EXISTS (SELECT 1 FROM c)`, turso gives `no such column: x`
(the fix of #9521 plans only the CTEs that a subquery reads). The
SQLite frontend on main (65154da) panics the same way. SQLite gives
`no such column: x`.

Found by: the NoREC instance of the 12-hour campaign of 2026-11-05, as
an AFL++ crash (an INSERT with twelve nested CTEs named `x`). Reduced
with a reducer that removes parenthesized groups.

Issue: https://github.com/tursodatabase/turso/issues/11462

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

### 12-hour campaign of 2026-11-05

tursopg at ff97c22. One `afl-fuzz` instance per oracle, from 02:31 to
14:31 UTC. Seeds: 1,837 files (both corpora, filtered by the harness).
The instances used the oracles of commit 6b1824e. The SQLRight fixes
made during the campaign (afbea86, 1933e2e, 962d1e1, dbf2ed9, and the
result marker fixes 3f6b925 and ada818b of another branch) were not
active in the running instances.

| Oracle | Executions | Test cases | Reports | Crashes | Hangs | Queue | Edges |
|---|---|---|---|---|---|---|---|
| NoREC | 1,776k | 1,577,686 | 641 | 236 | 4 | 30,601 | 172,513 |
| TLP | 1,022k | 857,325 | 34 | 3 | 10 | 26,142 | 172,233 |
| INDEX | 751k | 632,818 | 382 | 117 | 2 | 18,717 | 161,715 |

New turso issues: #11407 (finding 8, ON CONFLICT with a non-unique
index), #11436 (finding 21), #11462 (finding 22). Comments with new
forms: #11403 (a plain VALUES list panics), #11386 (timetz, every
integer finds the row), #11395 (an extension aggregate inside an
aggregate).

Reports by cause:

- NoREC: about 550 window clauses kept in the original form (SQLRight,
  962d1e1), about 75 timestamptz text comparisons (#11011), jsonb and
  date keys (#11386), 1 FROM taken from a subquery (SQLRight, afbea86),
  a few result markers in values (SQLRight, 3f6b925).
- TLP: date, time, timetz and timestamptz keys (#11386, #11011), the
  expression index with TRUE (#11436), result markers in values
  (SQLRight, 3f6b925), 2 aliases with a double quote (SQLRight, open),
  1 aggregate in a subquery over an outer column (SQLRight, open).
- INDEX: expression and partial indexes that fail on some rows
  (SQLRight, 1933e2e), an INSERT stopped by the time limit in one run
  only (SQLRight, dbf2ed9), ON CONFLICT DO UPDATE (#11407), bare
  columns with HAVING (#7977), date, timestamp and jsonb keys (#11386),
  1 bare column next to an aggregate (SQLRight, 1933e2e).

Crashes by location, all known: stack overflow on nested CTEs with one
name (131, #11401), extension aggregate in scalar context (116,
#11395), more than 7 array dimensions (89, #11416, fixed after
ff97c22), CTE column read without FROM (11, #11462), make_date (4,
#11398), ALTER TABLE DROP without columns (2, #11402), VALUES rows of
different lengths (1, #11403), INSERT with one DEFAULT for several
columns (1, #10088). One TLP crash file (`sig:09`) is a worker process
that was stopped by hand.

Hangs: NUMERIC with a scale of hundreds of millions (#11397), and
lpad or rpad to hundreds of millions of characters. `lpad` builds the
padding one character at a time: 360 million characters take 9.9
seconds. No issue was opened for the speed of lpad.
