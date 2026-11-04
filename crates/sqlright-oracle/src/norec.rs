//! The NoREC oracle (Rigger and Su, 2020). Port of `postgres_norec.cpp`.
//!
//! The original query `SELECT COUNT(*) FROM t WHERE p` lets the optimizer
//! use indexes and other shortcuts. The rewritten query
//! `SELECT SUM(x) FROM (SELECT (p)::INT AS x FROM t) AS r` evaluates `p` for
//! every row. Both must give the same number.

use sqlright_ir::util::find_string_in;
use sqlright_ir::{IrId, IrTree, IrType, Op};
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{AllCompRes, OraCompRes, Oracle};
use sqlright_mutator::Mutator;
use sqlright_pg_parser::parse_query;

pub struct Norec;

const TEMPLATE: &str = "SELECT COUNT ( * ) FROM x WHERE x=0;";
/// The rewritten form. `CASE WHEN` instead of the C++ `( true ) :: INT`: a
/// database that accepts a non-boolean WHERE expression (tursopg does)
/// counts the rows where the expression is true in both forms, while the
/// cast would sum the expression values.
const POST_FIX_TEMPLATE: &str =
    "SELECT COALESCE( SUM(countt), 0) FROM ( SELECT ALL CASE WHEN ( true ) THEN 1 ELSE 0 END as countt FROM v2 ORDER BY ( v1 ) ) as ress;";

impl Norec {
    /// Keep only the first select target. The C++ edits the statement in
    /// place inside `is_oracle_select_stmt`.
    fn cut_extra_targets(t: &mut IrTree, stmt: IrId) -> bool {
        let lists = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::TargetList, false);
        let Some(mut target_list) = lists.first().copied() else {
            return false;
        };
        while t.ty(target_list) == IrType::TargetList {
            let Some(extra) = t.right(target_list) else {
                break;
            };
            t.replace_op(target_list, Op::empty());
            t.update_right(target_list, None);
            t.deep_drop(extra);
            match t.left(target_list) {
                Some(l) => target_list = l,
                None => break,
            }
        }
        true
    }

    /// True when the first target is `COUNT ( * )` in the expected IR shape.
    fn has_count_star(t: &IrTree, stmt: IrId) -> bool {
        for func_name in ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::FuncName, false)
        {
            let chain = [
                "kFuncApplication",
                "kFuncExpr",
                "kCExpr",
                "kAExpr",
                "kTargetEl",
                "kTargetList",
                "kOptTargetList",
                "kSimpleSelect",
            ];
            let matches = chain.iter().enumerate().all(|(i, name)| {
                ir_wrapper::get_parent_type_str(t, func_name, i as i32 + 1) == *name
            });
            if !matches {
                continue;
            }
            let Some(func_app) = t.parent(func_name) else {
                continue;
            };
            if t.middle(func_app) != "( * )" {
                continue;
            }
            if let Some(iden) = t.left(func_name) {
                let name = t.str_val(iden);
                if name == "count" || name == "COUNT" {
                    return true;
                }
            }
        }
        false
    }
}

impl Oracle for Norec {
    fn oracle_type(&self) -> &'static str {
        "NOREC"
    }

    fn template_select_stmt(&self, _mutator: &mut Mutator) -> String {
        TEMPLATE.to_string()
    }

    fn is_oracle_select_stmt(&self, t: &mut IrTree, stmt: IrId) -> bool {
        if t.ty(stmt) != IrType::SelectStmt {
            return false;
        }
        if ir_wrapper::is_exist_group_clause(t, stmt)
            || ir_wrapper::is_exist_having_clause(t, stmt)
            || ir_wrapper::is_exist_limit_clause(t, stmt)
            || ir_wrapper::is_exist_set_operator(t, stmt)
        {
            return false;
        }
        if !Self::cut_extra_targets(t, stmt) {
            return false;
        }
        let froms = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::FromClause, false);
        let wheres = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::WhereClause, false);
        if froms.is_empty()
            || wheres.is_empty()
            || ir_wrapper::get_num_target_el_in_select_clause(t, stmt) != 1
        {
            return false;
        }
        if t.is_empty(froms[0]) || t.is_empty(wheres[0]) {
            return false;
        }
        // `count(*) FILTER (WHERE ...)` counts fewer rows than the WHERE
        // clause alone, and the rewrite only keeps the WHERE clause.
        if ir_wrapper::get_ir_node_in_stmt_with_type_full(
            t,
            stmt,
            IrType::FilterClause,
            false,
            true,
            true,
        )
        .iter()
        .any(|&c| !t.is_empty(c))
        {
            return false;
        }
        Self::has_count_star(t, stmt)
    }

    fn post_fix_transform_select_stmt(
        &self,
        _mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
        _run: usize,
    ) -> Vec<IrTree> {
        let mut out = Vec::new();
        t.get_mut(stmt).parent = None;
        if !self.is_oracle_select_stmt(t, stmt) {
            return out;
        }
        // The original form, without window clauses.
        let mut first = t.extract(stmt);
        let first_root = first.root.expect("root");
        for ty in [IrType::OverClause, IrType::WindowClause] {
            let clauses = ir_wrapper::get_ir_node_in_stmt_with_type(&first, first_root, ty, false);
            if let Some(clause) = clauses.first().copied() {
                let empty = first.node(ty, Op::empty(), None, None);
                first.swap_node(first_root, clause, Some(empty));
                first.deep_drop(clause);
            }
        }
        out.push(first);

        // The rewritten form: the template with FROM, WHERE and ORDER BY of the original.
        let Some(template) = parse_query(POST_FIX_TEMPLATE) else {
            return Vec::new();
        };
        let Some(template_stmt) =
            ir_wrapper::get_first_stmt_from_root(&template.tree, template.root)
        else {
            return Vec::new();
        };
        let mut trans = template.tree.extract(template_stmt);
        let trans_root = trans.root.expect("root");

        let src_orders =
            ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::SortClause, false);
        let dest_orders =
            ir_wrapper::get_ir_node_in_stmt_with_type(&trans, trans_root, IrType::SortClause, true);
        let Some(dest_order) = dest_orders.first().copied() else {
            return Vec::new();
        };
        if let Some(src_order) = src_orders.first().copied() {
            let copy = trans.import(t, src_order);
            if !trans.swap_node(trans_root, dest_order, Some(copy)) {
                return Vec::new();
            }
            trans.deep_drop(dest_order);
        } else {
            trans.detach_node(trans_root, dest_order);
            trans.deep_drop(dest_order);
        }

        let src_wheres =
            ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::WhereClause, false);
        let Some(src_where_expr) = src_wheres.first().and_then(|w| t.left(*w)) else {
            return Vec::new();
        };
        let dest_consts =
            ir_wrapper::get_ir_node_in_stmt_with_type(&trans, trans_root, IrType::AexprConst, true);
        let Some(dest_where) = dest_consts
            .iter()
            .copied()
            .find(|&c| trans.to_string(c).trim().eq_ignore_ascii_case("true"))
        else {
            return Vec::new();
        };
        let where_copy = trans.import(t, src_where_expr);
        if !trans.swap_node(trans_root, dest_where, Some(where_copy)) {
            return Vec::new();
        }
        trans.deep_drop(dest_where);

        let src_froms =
            ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::FromClause, false);
        let Some(src_from) = src_froms.first().copied() else {
            return Vec::new();
        };
        let dest_froms =
            ir_wrapper::get_ir_node_in_stmt_with_type(&trans, trans_root, IrType::FromClause, true);
        let Some(dest_from) = dest_froms.first().copied() else {
            return Vec::new();
        };
        let from_copy = trans.import(t, src_from);
        if !trans.swap_node(trans_root, dest_from, Some(from_copy)) {
            return Vec::new();
        }
        trans.deep_drop(dest_from);

        out.push(trans);
        out
    }

    fn compare_results(&self, all: &mut AllCompRes) {
        all.final_res = Some(OraCompRes::Pass);
        let mut is_all_err = true;
        for res in all.v_res.iter_mut() {
            if find_string_in(&res.res_str_0, "Error") || find_string_in(&res.res_str_1, "Error") {
                res.comp_res = OraCompRes::Error;
                res.res_int_0 = -1;
                res.res_int_1 = -1;
                continue;
            }
            let (Some(a), Some(b)) = (
                parse_leading_int(&res.res_str_0),
                parse_leading_int(&res.res_str_1),
            ) else {
                res.comp_res = OraCompRes::Error;
                continue;
            };
            res.res_int_0 = a;
            res.res_int_1 = b;
            is_all_err = false;
            if a != b {
                res.comp_res = OraCompRes::Fail;
                all.final_res = Some(OraCompRes::Fail);
            } else {
                res.comp_res = OraCompRes::Pass;
            }
        }
        if is_all_err && all.final_res != Some(OraCompRes::Fail) {
            all.final_res = Some(OraCompRes::Error);
        }
    }
}

/// The integer at the start of a result block, like `std::stoi`.
pub fn parse_leading_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let end = s
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+')))
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    s[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_filter_clause() {
        let mut q =
            parse_query("SELECT count(*) FILTER (WHERE v1 > 0) FROM v0 WHERE v1 < 10;").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert!(!Norec.is_oracle_select_stmt(&mut q.tree, stmt));
        let mut q = parse_query("SELECT count(*) FROM v0 WHERE v1 < 10;").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert!(Norec.is_oracle_select_stmt(&mut q.tree, stmt));
    }

    #[test]
    fn accepts_count_star_with_from_and_where() {
        let mut q = parse_query("SELECT COUNT(*) FROM v0 WHERE v1 = 1;").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert!(Norec.is_oracle_select_stmt(&mut q.tree, stmt));
        let mut q2 = parse_query("SELECT v1 FROM v0 WHERE v1 = 1;").unwrap();
        let stmt2 = ir_wrapper::get_first_stmt_from_root(&q2.tree, q2.root).unwrap();
        assert!(!Norec.is_oracle_select_stmt(&mut q2.tree, stmt2));
        let mut q3 = parse_query("SELECT COUNT(*) FROM v0 WHERE v1 = 1 LIMIT 3;").unwrap();
        let stmt3 = ir_wrapper::get_first_stmt_from_root(&q3.tree, q3.root).unwrap();
        assert!(!Norec.is_oracle_select_stmt(&mut q3.tree, stmt3));
    }

    #[test]
    fn rewrites_where_into_select() {
        let mut m = Mutator::new(1);
        let mut q = parse_query("SELECT COUNT(*) FROM v0 WHERE v1 = 1 ORDER BY v2;").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        let forms = Norec.post_fix_transform_select_stmt(&mut m, &mut q.tree, stmt, 0);
        assert_eq!(forms.len(), 2);
        let texts: Vec<String> = forms.iter().map(|f| f.to_string(f.root.unwrap())).collect();
        assert_eq!(
            texts[0],
            "SELECT count ( * ) FROM v0 WHERE v1 = 1 ORDER BY v2"
        );
        assert_eq!(texts[1], "SELECT COALESCE ( sum ( countt ), 0 ) FROM ( SELECT ALL CASE WHEN ( v1 = 1 ) THEN 1 ELSE 0 END AS countt FROM v0 ORDER BY v2 ) AS ress");
        assert!(parse_query(&texts[1]).is_some());
    }

    #[test]
    fn compares_counts() {
        let mut all = AllCompRes::default();
        let r = sqlright_mutator::oracle::CompRes {
            res_str_0: "3".into(),
            res_str_1: "4".into(),
            ..Default::default()
        };
        all.v_res.push(r);
        Norec.compare_results(&mut all);
        assert_eq!(all.final_res, Some(OraCompRes::Fail));
    }
}
