//! The INDEX oracle from the SQLRight paper, adapted from the SQLite
//! variant (`sqlite_index.cpp`) to the PostgreSQL grammar.
//!
//! Indexes must not change query results. The test case runs twice: once
//! with its `CREATE INDEX` statements and once with them replaced by empty
//! statements. UNIQUE indexes are made non-unique first, because a unique
//! index rejects rows and so changes the results by design.

use sqlright_ir::util::{find_string_in, is_str_empty, string_splitter};
use sqlright_ir::{IrId, IrTree, IrType, Op};
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{AllCompRes, CompRes, OraCompRes, Oracle};
use sqlright_mutator::Mutator;
use sqlright_pg_parser::parse_query;

use crate::norec::parse_leading_int;
use crate::tlp::oracle_select_blocks;

pub struct Index;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexType {
    Normal,
    Aggr,
}

const TEMPLATES: &[&str] = &[
    "SELECT * FROM x;",
    "SELECT * FROM x WHERE x = x;",
    "SELECT * FROM x WHERE x GROUP BY x;",
    "SELECT * FROM x WHERE x HAVING x;",
    "SELECT DISTINCT * FROM x WHERE x;",
    "SELECT MIN(x) FROM x WHERE x;",
    "SELECT MAX(x) FROM x WHERE x;",
    "SELECT SUM(x) FROM x WHERE x;",
    "SELECT COUNT(x) FROM x WHERE x;",
    "SELECT AVG(x) FROM x WHERE x;",
];

const APPEND_TEMPLATES: &[&str] = &["CREATE INDEX x ON x(x);"];

impl Index {
    /// How many `CREATE INDEX` statements to add to a test case.
    pub const RANDOM_APPEND_COUNT: usize = 5;

    /// A `CREATE INDEX` statement for the mutator to add to the test case.
    /// Its names are filled in by the instantiation step.
    pub fn random_append_stmt(mutator: &mut Mutator) -> Option<IrTree> {
        let text = APPEND_TEMPLATES[mutator.rng.rand_int(APPEND_TEMPLATES.len())];
        let q = parse_query(text)?;
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root)?;
        Some(q.tree.extract(stmt))
    }

    fn stmt_index_type(t: &IrTree, stmt: IrId) -> IndexType {
        // `sum(x) OVER (...)` returns one row per input row, and without an
        // ORDER BY in the window its values depend on the scan order. Only
        // its row count is comparable.
        if has_window_function(t, stmt) {
            return IndexType::Normal;
        }
        let targets = ir_wrapper::get_target_el_in_select_target(t, stmt);
        let Some(target) = targets.first().copied() else {
            return IndexType::Normal;
        };
        for func_name in
            ir_wrapper::get_ir_node_in_stmt_with_type(t, target, IrType::FuncName, false)
        {
            if let Some(iden) = t.left(func_name) {
                let lower = t.str_val(iden).to_ascii_lowercase();
                let name = lower.rsplit('.').next().unwrap_or(&lower).trim();
                if !["min", "max", "count", "sum", "avg"].contains(&name) {
                    continue;
                }
                // `max(a, b)` is a scalar function in tursopg (as in SQLite).
                // It returns one row for each input row, so only its row
                // count is comparable.
                if !ir_wrapper::is_scalar_min_max(t, func_name, name) {
                    return IndexType::Aggr;
                }
            }
        }
        IndexType::Normal
    }

    fn get_v_valid_type(cmd_str: &str) -> Vec<IndexType> {
        let mut out = Vec::new();
        for block in oracle_select_blocks(cmd_str) {
            let Some(q) = parse_query(&block) else {
                continue;
            };
            let Some(stmt) = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root) else {
                continue;
            };
            out.push(Self::stmt_index_type(&q.tree, stmt));
        }
        out
    }

    /// Row counts must match across runs. Returns true on an error result.
    fn compare_norm(res: &mut CompRes) -> bool {
        if res.v_res_str.len() <= 1 {
            res.comp_res = OraCompRes::Error;
            return true;
        }
        let mut counts = Vec::new();
        for s in &res.v_res_str {
            if find_string_in(s, "ERROR") {
                res.comp_res = OraCompRes::Error;
                return true;
            }
            let rows = string_splitter(s, '\n');
            if rows.len() > 50 {
                res.comp_res = OraCompRes::Error;
                return true;
            }
            let empties = rows.iter().filter(|r| is_str_empty(r)).count() as i64;
            counts.push(s.matches('\n').count() as i64 - empties);
        }
        res.v_res_int = counts.clone();
        res.comp_res = if counts.iter().any(|c| *c != counts[0]) {
            OraCompRes::Fail
        } else {
            OraCompRes::Pass
        };
        false
    }

    /// Aggregate values must match across runs. Returns true on an error result.
    fn compare_aggr(res: &mut CompRes) -> bool {
        if res.v_res_str.len() <= 1 {
            res.comp_res = OraCompRes::Error;
            return true;
        }
        let mut values = Vec::new();
        for s in &res.v_res_str {
            if find_string_in(s, "ERROR") {
                res.comp_res = OraCompRes::Error;
                return true;
            }
            match parse_leading_int(s) {
                Some(v) => values.push(v),
                None => {
                    res.comp_res = OraCompRes::Error;
                    return true;
                }
            }
        }
        res.v_res_int = values.clone();
        res.comp_res = if values.iter().any(|v| *v != values[0]) {
            OraCompRes::Fail
        } else {
            OraCompRes::Pass
        };
        false
    }
}

fn has_window_function(t: &IrTree, stmt: IrId) -> bool {
    [IrType::OverClause, IrType::WindowClause]
        .iter()
        .any(|&ty| {
            ir_wrapper::get_ir_node_in_stmt_with_type_full(t, stmt, ty, false, true, true)
                .iter()
                .any(|&c| !t.is_empty(c))
        })
}

impl Oracle for Index {
    fn oracle_type(&self) -> &'static str {
        "INDEX"
    }

    fn template_select_stmt(&self, mutator: &mut Mutator) -> String {
        TEMPLATES[mutator.rng.rand_int(TEMPLATES.len())].to_string()
    }

    fn mul_run_num(&self) -> usize {
        2
    }

    fn is_oracle_normal_stmt(&self, t: &IrTree, stmt: IrId) -> bool {
        t.ty(stmt) == IrType::IndexStmt
    }

    fn is_oracle_select_stmt(&self, t: &mut IrTree, stmt: IrId) -> bool {
        if t.ty(stmt) != IrType::SelectStmt {
            return false;
        }
        // LIMIT without ORDER BY picks rows the planner likes, so indexes
        // change its result by design.
        !ir_wrapper::is_exist_limit_clause(t, stmt)
    }

    fn pre_fix_transform_normal_stmt(
        &self,
        _mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
    ) -> Option<IrTree> {
        if !self.is_oracle_normal_stmt(t, stmt) {
            return None;
        }
        let mut copy = t.extract(stmt);
        let root = copy.root?;
        for opt_unique in
            ir_wrapper::get_ir_node_in_stmt_with_type(&copy, root, IrType::OptUnique, false)
        {
            copy.replace_op(opt_unique, Op::empty());
        }
        Some(copy)
    }

    fn post_fix_transform_normal_stmt(
        &self,
        _mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
        run: usize,
    ) -> Vec<IrTree> {
        if run == 0 {
            return vec![t.extract(stmt)];
        }
        // Later runs have no index: an empty statement takes its place.
        let mut empty = IrTree::new();
        let r = empty.leaf(IrType::Stmt, "");
        empty.root = Some(r);
        vec![empty]
    }

    fn post_fix_transform_select_stmt(
        &self,
        _mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
        _run: usize,
    ) -> Vec<IrTree> {
        vec![t.extract(stmt)]
    }

    fn compare_results(&self, all: &mut AllCompRes) {
        if all.v_cmd_str.is_empty() || all.v_res_str.is_empty() {
            all.final_res = Some(OraCompRes::Error);
            return;
        }
        all.final_res = Some(OraCompRes::Pass);
        let types = Self::get_v_valid_type(&all.v_cmd_str[0]);
        let mut is_all_err = true;
        for (i, res) in all.v_res.iter_mut().enumerate() {
            let Some(ty) = types.get(i) else {
                res.comp_res = OraCompRes::Error;
                break;
            };
            let err = match ty {
                IndexType::Normal => Self::compare_norm(res),
                IndexType::Aggr => Self::compare_aggr(res),
            };
            if !err {
                is_all_err = false;
            }
            if res.comp_res == OraCompRes::Fail {
                all.final_res = Some(OraCompRes::Fail);
            }
        }
        if is_all_err && all.final_res != Some(OraCompRes::Fail) {
            all.final_res = Some(OraCompRes::Error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_unique_and_drops_indexes_in_run_two() {
        let mut m = Mutator::new(5);
        let mut q = parse_query("CREATE UNIQUE INDEX i0 ON v0 (c1);").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert!(Index.is_oracle_normal_stmt(&q.tree, stmt));
        let pre = Index
            .pre_fix_transform_normal_stmt(&mut m, &mut q.tree, stmt)
            .unwrap();
        let pre_root = pre.root.unwrap();
        assert_eq!(pre.to_string(pre_root), "CREATE INDEX i0 ON v0 ( c1 )");
        let mut pre = pre;
        let run1 = Index.post_fix_transform_normal_stmt(&mut m, &mut pre, pre_root, 1);
        assert_eq!(run1[0].to_string(run1[0].root.unwrap()), "");
    }

    fn index_type_of(sql: &str) -> IndexType {
        let q = parse_query(sql).unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        Index::stmt_index_type(&q.tree, stmt)
    }

    #[test]
    fn a_scalar_min_or_max_is_not_an_aggregate() {
        // One argument: the aggregate. Two or more: the scalar function.
        assert_eq!(index_type_of("SELECT max(c) FROM t WHERE c;"), IndexType::Aggr);
        assert_eq!(index_type_of("SELECT min(c) FROM t WHERE c;"), IndexType::Aggr);
        assert_eq!(index_type_of("SELECT max(c, 80) FROM t WHERE c;"), IndexType::Normal);
        assert_eq!(index_type_of("SELECT min(c, 1, 2) FROM t WHERE c;"), IndexType::Normal);
        // Other aggregates and count(*) stay aggregates.
        assert_eq!(index_type_of("SELECT count(*) FROM t WHERE c;"), IndexType::Aggr);
        assert_eq!(index_type_of("SELECT sum(c) FROM t WHERE c;"), IndexType::Aggr);
        // A name that only contains an aggregate name is not an aggregate.
        assert_eq!(index_type_of("SELECT summary(c) FROM t WHERE c;"), IndexType::Normal);
    }

    #[test]
    fn compares_only_the_row_count_of_a_window_function() {
        let cmd = "SELECT 'BEGIN VERI 0'; SELECT sum(c1) OVER (ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) FROM v0 WHERE c1; SELECT 'END VERI 0'; \
                   SELECT 'BEGIN VERI 0'; SELECT sum(c1) FROM v0 WHERE c1; SELECT 'END VERI 0';";
        assert_eq!(
            Index::get_v_valid_type(cmd),
            vec![IndexType::Normal, IndexType::Aggr]
        );
        let mut all = AllCompRes::default();
        all.v_cmd_str.push(cmd.into());
        all.v_res_str.push("x".into());
        all.v_res.push(CompRes {
            v_res_str: vec!["1.0 \n2.0 \n".into(), "2.0 \n5.0 \n".into()],
            ..CompRes::default()
        });
        all.v_res.push(CompRes {
            v_res_str: vec!["3 \n".into(), "3 \n".into()],
            ..CompRes::default()
        });
        Index.compare_results(&mut all);
        assert_eq!(all.final_res, Some(OraCompRes::Pass));
    }

    #[test]
    fn compares_row_counts_across_runs() {
        let mut all = AllCompRes::default();
        all.v_cmd_str.push(
            "SELECT 'BEGIN VERI 0'; SELECT * FROM v0 WHERE c1 > 1; SELECT 'END VERI 0';".into(),
        );
        all.v_res_str.push("x".into());
        let r = CompRes {
            v_res_str: vec!["1 \n2 \n".into(), "1 \n".into()],
            ..CompRes::default()
        };
        all.v_res.push(r);
        Index.compare_results(&mut all);
        assert_eq!(all.final_res, Some(OraCompRes::Fail));
    }
}
