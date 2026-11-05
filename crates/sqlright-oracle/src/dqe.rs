//! The DQE oracle, differential query execution (Song and others, ICSE
//! 2023), as SQLancer implements it.
//!
//! `SELECT`, `UPDATE` and `DELETE` with the same WHERE clause must touch
//! the same rows. The oracle takes `SELECT ... FROM t WHERE p` and runs:
//!
//! 1. `SELECT * FROM t WHERE p`
//! 2. `UPDATE t SET c = c WHERE p RETURNING *` (a column `c` of `t`)
//! 3. `DELETE FROM t WHERE p RETURNING *`
//!
//! The three must return the same rows, in any order. SQLancer adds a row
//! id column and an `updated` column to every table to find the rows an
//! UPDATE touched; tursopg has `RETURNING`, so the rows come from the
//! statements themselves. The DELETE removes the rows, so the next oracle
//! SELECT of the test case sees a smaller table. That is fine: each oracle
//! SELECT is compared with its own UPDATE and DELETE.

use sqlright_ir::util::find_string_in;
use sqlright_ir::{IrId, IrTree, IrType};
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{AllCompRes, OraCompRes, Oracle};
use sqlright_mutator::Mutator;
use sqlright_pg_parser::parse_query;

use crate::shape::{
    compare_row_sets, has_aggregate_call, has_clause, has_distinct, has_window_function,
    single_table, subquery_reads_table, where_expr, SingleTable,
};

pub struct Dqe;

const TEMPLATES: &[&str] = &[
    "SELECT * FROM x WHERE x = 0;",
    "SELECT * FROM x WHERE x > 0 AND x < 10;",
    "SELECT * FROM x WHERE x IS NULL;",
    "SELECT * FROM x WHERE x IN (0, 1, 2);",
    "SELECT * FROM x WHERE x LIKE 'x';",
    "SELECT * FROM x AS a WHERE a.x <> 0 OR a.x IS NULL;",
    "SELECT * FROM x WHERE x = (SELECT MAX(x) FROM x);",
];

impl Dqe {
    /// `SELECT ... FROM <one table> WHERE p` without a clause that changes
    /// the rows: no aggregate, no grouping, no DISTINCT, no LIMIT, no
    /// window function, no set operator.
    fn is_plain_select_on_one_table(t: &IrTree, stmt: IrId) -> bool {
        if t.ty(stmt) != IrType::SelectStmt {
            return false;
        }
        if !ir_wrapper::check_node_num(t, stmt, 300) {
            return false;
        }
        if ir_wrapper::is_exist_limit_clause(t, stmt)
            || ir_wrapper::is_exist_set_operator(t, stmt)
            || has_clause(t, stmt, IrType::GroupClause)
            || has_clause(t, stmt, IrType::HavingClause)
            || has_clause(t, stmt, IrType::IntoClause)
            || has_clause(t, stmt, IrType::ForLockingClause)
            || has_clause(t, stmt, IrType::WithClause)
            || has_distinct(t, stmt)
            || has_window_function(t, stmt)
            || has_aggregate_call(t, stmt)
        {
            return false;
        }
        single_table(t, stmt).is_some() && where_expr(t, stmt).is_some()
    }

    /// The three statements as text. The UPDATE needs a column of the
    /// table; without one, only the SELECT and the DELETE are made.
    fn statements(table: &SingleTable, predicate: &str, column: Option<&str>) -> Vec<String> {
        let from = match &table.alias {
            Some(alias) => format!("{} AS {alias}", table.name),
            None => table.name.clone(),
        };
        let mut out = vec![format!("SELECT * FROM {from} WHERE {predicate}")];
        if let Some(column) = column {
            out.push(format!(
                "UPDATE {from} SET {column} = {column} WHERE {predicate} RETURNING *"
            ));
        }
        out.push(format!("DELETE FROM {from} WHERE {predicate} RETURNING *"));
        out
    }
}

impl Oracle for Dqe {
    fn oracle_type(&self) -> &'static str {
        "DQE"
    }

    fn template_select_stmt(&self, mutator: &mut Mutator) -> String {
        TEMPLATES[mutator.rng.rand_int(TEMPLATES.len())].to_string()
    }

    fn is_oracle_select_stmt(&self, t: &mut IrTree, stmt: IrId) -> bool {
        Self::is_plain_select_on_one_table(t, stmt)
    }

    fn post_fix_transform_select_stmt(
        &self,
        mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
        _run: usize,
    ) -> Vec<IrTree> {
        t.get_mut(stmt).parent = None;
        if !Self::is_plain_select_on_one_table(t, stmt) {
            return Vec::new();
        }
        let Some(table) = single_table(t, stmt) else {
            return Vec::new();
        };
        // A subquery on the table itself reads rows the DELETE removes.
        if subquery_reads_table(t, stmt, &table.name) {
            return Vec::new();
        }
        let Some(predicate) = where_expr(t, stmt) else {
            return Vec::new();
        };
        let predicate = t.to_string(predicate);
        let column = mutator
            .ctx
            .m_tables
            .get(&table.name)
            .and_then(|cols| cols.first())
            .cloned();
        let mut forms = Vec::new();
        for text in Self::statements(&table, &predicate, column.as_deref()) {
            let Some(q) = parse_query(&format!("{text};")) else {
                return Vec::new();
            };
            let Some(first) = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root) else {
                return Vec::new();
            };
            forms.push(q.tree.extract(first));
        }
        forms
    }

    fn compare_results(&self, all: &mut AllCompRes) {
        all.final_res = Some(OraCompRes::Pass);
        // A trigger can change the rows an UPDATE or a DELETE returns.
        let has_trigger = find_string_in(&all.cmd_str, "TRIGGER");
        let mut is_all_err = true;
        for res in all.v_res.iter_mut() {
            if has_trigger {
                res.comp_res = OraCompRes::Error;
                continue;
            }
            if !compare_row_sets(res) {
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
    use sqlright_mutator::oracle::CompRes;

    fn forms(sql: &str, columns: &[&str]) -> Vec<String> {
        let mut m = Mutator::new(1);
        m.ctx.m_tables.insert(
            "v0".to_string(),
            columns.iter().map(|c| c.to_string()).collect(),
        );
        let mut q = parse_query(sql).unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        Dqe.post_fix_transform_select_stmt(&mut m, &mut q.tree, stmt, 0)
            .iter()
            .map(|f| f.to_string(f.root.unwrap()))
            .collect()
    }

    #[test]
    fn accepts_plain_selects_on_one_table() {
        for sql in [
            "SELECT * FROM v0 WHERE c1 = 1;",
            "SELECT c1, c2 FROM v0 AS a WHERE a.c1 > 0 ORDER BY c2;",
            "SELECT c1 FROM v0 WHERE c1 IN (SELECT c2 FROM v1);",
        ] {
            let mut q = parse_query(sql).unwrap();
            let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
            assert!(Dqe.is_oracle_select_stmt(&mut q.tree, stmt), "{sql}");
        }
        for sql in [
            "SELECT * FROM v0;",
            "SELECT * FROM v0, v1 WHERE c1 = 1;",
            "SELECT * FROM v0 JOIN v1 ON TRUE WHERE c1 = 1;",
            "SELECT count(*) FROM v0 WHERE c1 = 1;",
            "SELECT DISTINCT c1 FROM v0 WHERE c1 = 1;",
            "SELECT c1 FROM v0 WHERE c1 = 1 GROUP BY c1;",
            "SELECT c1 FROM v0 WHERE c1 = 1 LIMIT 2;",
            "SELECT sum(c1) OVER () FROM v0 WHERE c1 = 1;",
        ] {
            let mut q = parse_query(sql).unwrap();
            let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
            assert!(!Dqe.is_oracle_select_stmt(&mut q.tree, stmt), "{sql}");
        }
    }

    #[test]
    fn makes_the_select_update_and_delete_forms() {
        let texts = forms(
            "SELECT c1 FROM v0 AS a WHERE a.c1 > 1 OR c2 IS NULL;",
            &["c1", "c2"],
        );
        // The IR printer puts spaces around the dot of a qualified name.
        assert_eq!(
            texts,
            vec![
                "SELECT * FROM v0 AS a WHERE a . c1 > 1 OR c2 IS NULL",
                "UPDATE v0 AS a SET c1 = c1 WHERE a . c1 > 1 OR c2 IS NULL RETURNING *",
                "DELETE FROM v0 AS a WHERE a . c1 > 1 OR c2 IS NULL RETURNING *",
            ]
        );
        let texts = forms("SELECT c1 FROM v0 WHERE c1 > 1;", &[]);
        assert_eq!(
            texts,
            vec![
                "SELECT * FROM v0 WHERE c1 > 1",
                "DELETE FROM v0 WHERE c1 > 1 RETURNING *",
            ]
        );
    }

    #[test]
    fn gives_no_forms_when_a_subquery_reads_the_table() {
        let texts = forms(
            "SELECT c1 FROM v0 WHERE c1 = (SELECT max(c1) FROM v0);",
            &["c1"],
        );
        assert!(texts.is_empty());
        let texts = forms(
            "SELECT c1 FROM v0 WHERE c1 = (SELECT max(c1) FROM v1);",
            &["c1"],
        );
        assert_eq!(texts.len(), 3);
    }

    #[test]
    fn compares_three_forms_and_skips_cases_with_triggers() {
        let mut all = AllCompRes {
            cmd_str: "CREATE TABLE v0 (c1 INT);".into(),
            ..AllCompRes::default()
        };
        all.v_res.push(CompRes {
            res_str_0: "\n1 a \n2 b \n".into(),
            res_str_1: "\n2 b \n1 a \n".into(),
            res_str_2: "\n1 a \n2 b \n".into(),
            ..CompRes::default()
        });
        all.v_res.push(CompRes {
            res_str_0: "\n1 a \n".into(),
            res_str_1: "\n1 a \n".into(),
            res_str_2: "\n".into(),
            ..CompRes::default()
        });
        Dqe.compare_results(&mut all);
        assert_eq!(all.v_res[0].comp_res, OraCompRes::Pass);
        assert_eq!(all.v_res[1].comp_res, OraCompRes::Fail);
        assert_eq!(all.final_res, Some(OraCompRes::Fail));
        all.cmd_str = "CREATE TRIGGER x BEFORE DELETE ON v0 ...".into();
        Dqe.compare_results(&mut all);
        assert_eq!(all.final_res, Some(OraCompRes::Error));
    }
}
