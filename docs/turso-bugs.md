# turso bugs found with SQLRight

Findings from running the SQLRight rewrite against tursopg (the PostgreSQL
frontend of turso). The turso version is the submodule commit in this
repository. Each entry has a minimal SQL reproducer. Run a reproducer with:

```bash
printf '<sql>' > repro.sql
crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg --replay repro.sql
```

The list is for the turso team to turn into issues. SQLRight does not fix
turso bugs.

## 1. Crash: two-argument extension aggregate in a GROUP BY query

`percentile_cont` with two column arguments inside a `GROUP BY` query
panics with a slice index error in `core/ext/function.rs`
(`ExtFunc::step_aggregate`, `NativeAggregate`):

```sql
CREATE TABLE t(a float8);
INSERT INTO t VALUES (0.5);
SELECT percentile_cont(a, a) FROM t GROUP BY a;
```

```text
thread 'main' panicked at core/ext/function.rs:111:45:
range end index 28 out of range for slice of length 27
```

The same query without `GROUP BY` returns `0.5`, and
`SELECT percentile_cont(0.5, a) FROM t GROUP BY a` works too. The crash
needs two column arguments and a `GROUP BY`. The register range of the
aggregate arguments (`registers[start_reg..start_reg + argc]`) goes past
the end of the register file, so the planner gives the aggregate fewer
argument registers than `argc` in the grouped path.

Found by: the AFL++ dry run on a seed from the PostgreSQL regression test
`aggregates.sql` (the original statement is
`select p, percentile_cont(p,p) from generate_series(1,5) x, (values (0::float8),(0.1),...) v(p) group by p order by p;`).
