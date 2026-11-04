//! The TLP oracle, ternary logic partitioning (Rigger and Su, 2020). Port of
//! `postgres_tlp.cpp`.
//!
//! A predicate `p` is TRUE, FALSE or NULL for every row, so
//! `SELECT ... FROM t` equals
//! `SELECT ... WHERE p UNION ALL SELECT ... WHERE NOT p UNION ALL SELECT ... WHERE p IS NULL`.
//! For DISTINCT and GROUP BY the union removes duplicates. For aggregates
//! the partitions are aggregated again.

use std::collections::HashSet;

use sqlright_ir::util::{find_string_in, is_str_empty, string_splitter};
use sqlright_ir::{DataFlag, DataType, IrId, IrTree, IrType, Op};
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{AllCompRes, CompRes, OraCompRes, Oracle};
use sqlright_mutator::Mutator;
use sqlright_pg_parser::parse_query;

use crate::norec::parse_leading_int;

pub struct Tlp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlpType {
    AggrMin,
    AggrMax,
    AggrCount,
    AggrSum,
    AggrAvg,
    Distinct,
    GroupBy,
    Having,
    Normal,
    Unknown,
}

const TEMPLATES: &[&str] = &[
    "SELECT * FROM x WHERE x=0;",
    "SELECT x FROM x WHERE x=0 GROUP BY x;",
    "SELECT x FROM x WHERE x=0 HAVING x;",
    "SELECT DISTINCT x FROM x WHERE x=0;",
    "SELECT MIN(x) FROM x WHERE x=0;",
    "SELECT MAX(x) FROM x WHERE x=0;",
    "SELECT SUM(x) FROM x WHERE x=0;",
    "SELECT AVG(x) FROM x WHERE x=0;",
];

const OUTER_MIN: &str = "SELECT MIN(aggr) FROM (SELECT *) AS subb;";
const OUTER_MAX: &str = "SELECT MAX(aggr) FROM (SELECT *) AS subb;";
const OUTER_SUM: &str = "SELECT SUM(aggr) FROM (SELECT *) AS subb;";
const OUTER_AVG: &str = "SELECT SUM(s)/SUM(c) FROM (SELECT *) AS subb;";

/// The text of every oracle SELECT (form 0) in a marked test case.
pub fn oracle_select_blocks(cmd_str: &str) -> Vec<String> {
    let begin = "SELECT 'BEGIN VERI 0';";
    let end = "SELECT 'END VERI 0';";
    let mut out = Vec::new();
    let mut rest = cmd_str;
    while let Some(b) = rest.find(begin) {
        let after = &rest[b + begin.len()..];
        let Some(e) = after.find(end) else {
            break;
        };
        out.push(after[..e].to_string());
        rest = &after[e + end.len()..];
    }
    out
}

impl Tlp {
    /// Which kind of TLP applies to a SELECT.
    pub fn get_stmt_tlp_type(t: &IrTree, stmt: IrId) -> TlpType {
        let mut default_type = TlpType::Normal;
        if !ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::DistinctClause, false)
            .is_empty()
        {
            default_type = TlpType::Distinct;
        }
        for sq in ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::SetQuantifier, false) {
            if !t.is_empty(sq) && t.prefix(sq) == "DISTINCT" {
                default_type = TlpType::Distinct;
            }
        }
        for gc in ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::GroupClause, false) {
            if t.prefix(gc) == "GROUP BY" {
                default_type = TlpType::GroupBy;
            }
        }
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
            let Some(iden) = t.left(func_name) else {
                continue;
            };
            let name = t.str_val(iden);
            let aggr = if find_string_in(name, "count") {
                Some(TlpType::AggrCount)
            } else if find_string_in(name, "sum") {
                Some(TlpType::AggrSum)
            } else if find_string_in(name, "min") {
                Some(TlpType::AggrMin)
            } else if find_string_in(name, "max") {
                Some(TlpType::AggrMax)
            } else if find_string_in(name, "avg") {
                Some(TlpType::AggrAvg)
            } else {
                None
            };
            if let Some(a) = aggr {
                if default_type == TlpType::GroupBy {
                    return TlpType::Unknown;
                }
                return a;
            }
        }
        default_type
    }

    /// The TLP type of every oracle SELECT in a marked test case.
    fn get_v_valid_type(cmd_str: &str) -> Vec<TlpType> {
        let mut out = Vec::new();
        for block in oracle_select_blocks(cmd_str) {
            let Some(q) = parse_query(&block) else {
                continue;
            };
            let Some(stmt) = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root) else {
                continue;
            };
            out.push(Self::get_stmt_tlp_type(&q.tree, stmt));
        }
        out
    }

    /// `(p) UNION ALL (NOT (p)) UNION ALL ((p) IS NULL)` over the simple
    /// select of the statement. Returns a new tree.
    fn transform_non_aggr(t: &IrTree, stmt: IrId, is_union_all: bool) -> Option<IrTree> {
        let simple_selects =
            ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::SimpleSelect, false);
        let src = *simple_selects.first()?;
        let wheres = ir_wrapper::get_ir_node_in_stmt_with_type(t, src, IrType::WhereClause, false);
        let where_clause = *wheres.first()?;
        if t.is_empty(where_clause) {
            return None;
        }

        let mut out = IrTree::new();
        // part 1: WHERE ( p )
        let p1 = out.import(t, src);
        let w1 = *ir_wrapper::get_ir_node_in_stmt_with_type(&out, p1, IrType::WhereClause, false)
            .first()?;
        let e1 = out.left(w1)?;
        let operand1 = out.node(IrType::Operand, Op::new("(", ")", ""), None, None);
        let expr1 = out.node(IrType::Expr, Op::empty(), Some(operand1), None);
        out.swap_node(p1, e1, Some(expr1));
        out.update_left(operand1, Some(e1));

        // part 2: WHERE ( NOT ( p ) )
        let p2 = out.import(t, src);
        let w2 = *ir_wrapper::get_ir_node_in_stmt_with_type(&out, p2, IrType::WhereClause, false)
            .first()?;
        let e2 = out.left(w2)?;
        let operand0 = out.node(IrType::AExpr, Op::new("(", ")", ""), None, None);
        let unary = out.node(IrType::AExpr, Op::new("NOT", "", ""), Some(operand0), None);
        let operand_outer = out.node(IrType::AExpr, Op::new("(", ")", ""), Some(unary), None);
        let expr2 = out.node(IrType::AExpr, Op::empty(), Some(operand_outer), None);
        out.swap_node(p2, e2, Some(expr2));
        out.update_left(operand0, Some(e2));

        // part 3: WHERE ( ( p ) IS NULL )
        let p3 = out.import(t, src);
        let w3 = *ir_wrapper::get_ir_node_in_stmt_with_type(&out, p3, IrType::WhereClause, false)
            .first()?;
        let e3 = out.left(w3)?;
        let operand3 = out.node(IrType::Operand, Op::new("(", ")", ""), None, None);
        let unary3 = out.node(
            IrType::UnaryExpr,
            Op::new("", "IS NULL", ""),
            Some(operand3),
            None,
        );
        let operand3_outer = out.node(IrType::Expr, Op::new("(", ")", ""), Some(unary3), None);
        let expr3 = out.node(IrType::Expr, Op::empty(), Some(operand3_outer), None);
        out.swap_node(p3, e3, Some(expr3));
        out.update_left(operand3, Some(e3));

        let set_op = if is_union_all { "UNION ALL" } else { "UNION" };
        let inner = out.node(IrType::Unknown, Op::new("", set_op, ""), Some(p2), Some(p3));
        let root = out.node(
            IrType::Unknown,
            Op::new("", set_op, ""),
            Some(p1),
            Some(inner),
        );
        out.root = Some(root);
        Some(out)
    }

    /// Aggregates: give the aggregate an alias, partition, and aggregate the
    /// partitions again in an outer query.
    fn transform_aggr(
        t: &IrTree,
        stmt: IrId,
        is_union_all: bool,
        tlp_type: TlpType,
    ) -> Option<IrTree> {
        let mut work = t.extract(stmt);
        let root = work.root?;
        let func_names =
            ir_wrapper::get_ir_node_in_stmt_with_type(&work, root, IrType::FuncName, false);
        let aggr_func = *func_names.first()?;
        work.left(aggr_func)?;
        let targets =
            ir_wrapper::get_ir_node_in_stmt_with_type(&work, root, IrType::TargetEl, false);
        let target = *targets.first()?;
        if tlp_type != TlpType::AggrAvg {
            if work.middle(target) == "AS" {
                if let Some(iden) = work.right(target) {
                    work.set_str_val(iden, "aggr");
                }
            } else {
                let alias_id = work.leaf_str(
                    IrType::Identifier,
                    "aggr",
                    DataType::AliasName,
                    0,
                    DataFlag::Define,
                );
                let alias = work.node(
                    IrType::TargetEl,
                    Op::new("AS", "", ""),
                    Some(alias_id),
                    None,
                );
                let res = work.node(IrType::TargetEl, Op::empty(), None, Some(alias));
                work.swap_node(root, target, Some(res));
                work.update_left(res, Some(target));
            }
        } else {
            // AVG: SUM(x) AS s, COUNT(x) AS c, then SUM(s)/SUM(c) outside.
            let func_in_target =
                *ir_wrapper::get_ir_node_in_stmt_with_type(&work, target, IrType::FuncName, false)
                    .first()?;
            if work.target_el_is_exist_alias(target) {
                work.target_el_set_alias(target, "c");
                work.func_name_set_str(func_in_target, "COUNT");
                let res_1 = work.deep_copy(target);
                work.target_el_set_alias(target, "s");
                work.func_name_set_str(func_in_target, "SUM");
                let res_0 = work.deep_copy(target);
                let list = work.node(
                    IrType::TargetList,
                    Op::new("", ",", ""),
                    Some(res_0),
                    Some(res_1),
                );
                work.swap_node(root, target, Some(list));
                work.deep_drop(target);
            } else {
                work.func_name_set_str(func_in_target, "SUM");
                let alias_0 = work.leaf_str(
                    IrType::Identifier,
                    "s",
                    DataType::AliasName,
                    0,
                    DataFlag::Define,
                );
                let as_0 = work.node(IrType::TargetEl, Op::new("AS", "", ""), Some(alias_0), None);
                let copy_0 = work.deep_copy(target);
                let res_0 = work.node(IrType::TargetEl, Op::empty(), Some(copy_0), Some(as_0));
                work.func_name_set_str(func_in_target, "COUNT");
                let alias_1 = work.leaf_str(
                    IrType::Identifier,
                    "c",
                    DataType::AliasName,
                    0,
                    DataFlag::Define,
                );
                let as_1 = work.node(IrType::TargetEl, Op::new("AS", "", ""), Some(alias_1), None);
                let copy_1 = work.deep_copy(target);
                let res_1 = work.node(IrType::TargetEl, Op::empty(), Some(copy_1), Some(as_1));
                let list = work.node(
                    IrType::TargetList,
                    Op::new("", ",", ""),
                    Some(res_0),
                    Some(res_1),
                );
                work.swap_node(root, target, Some(list));
                work.deep_drop(target);
            }
        }
        let inner = Self::transform_non_aggr(&work, root, is_union_all)?;
        let outer_text = match tlp_type {
            TlpType::AggrSum => OUTER_SUM,
            TlpType::AggrMin => OUTER_MIN,
            TlpType::AggrMax => OUTER_MAX,
            TlpType::AggrAvg => OUTER_AVG,
            _ => return None,
        };
        let outer_q = parse_query(outer_text)?;
        let outer_stmt = ir_wrapper::get_first_stmt_from_root(&outer_q.tree, outer_q.root)?;
        let mut outer = outer_q.tree.extract(outer_stmt);
        let outer_root = outer.root?;
        let placeholder = *ir_wrapper::get_ir_node_in_stmt_with_type_full(
            &outer,
            outer_root,
            IrType::SelectNoParens,
            true,
            false,
            true,
        )
        .first()?;
        let inner_id = outer.import(&inner, inner.root?);
        outer.swap_node(outer_root, placeholder, Some(inner_id));
        outer.deep_drop(placeholder);
        Some(outer)
    }

    fn compare_norm(res: &mut CompRes) -> bool {
        if find_string_in(&res.res_str_0, "ERROR") || find_string_in(&res.res_str_1, "ERROR") {
            res.comp_res = OraCompRes::Error;
            return true;
        }
        let v_a = string_splitter(&res.res_str_0, '\n');
        let v_b = string_splitter(&res.res_str_1, '\n');
        if v_a.len() > 50 || v_b.len() > 50 {
            res.comp_res = OraCompRes::Error;
            return true;
        }
        let a = count_non_empty(&res.res_str_0);
        let b = count_non_empty(&res.res_str_1);
        res.res_int_0 = a;
        res.res_int_1 = b;
        if a == 0 && v_b.iter().all(|r| r == "0") {
            res.comp_res = OraCompRes::Pass;
            return false;
        }
        if a != b {
            res.comp_res = OraCompRes::Fail;
            return false;
        }
        // The C++ stopped at the row counts. The rows themselves must also
        // be the same multiset.
        let mut rows_a: Vec<&str> = v_a
            .iter()
            .map(|r| r.trim_end())
            .filter(|r| !r.is_empty())
            .collect();
        let mut rows_b: Vec<&str> = v_b
            .iter()
            .map(|r| r.trim_end())
            .filter(|r| !r.is_empty())
            .collect();
        rows_a.sort_unstable();
        rows_b.sort_unstable();
        res.comp_res = if rows_a != rows_b {
            OraCompRes::Fail
        } else {
            OraCompRes::Pass
        };
        false
    }

    fn compare_uniq(res: &mut CompRes) -> bool {
        if find_string_in(&res.res_str_0, "ERROR") || find_string_in(&res.res_str_1, "ERROR") {
            res.comp_res = OraCompRes::Error;
            return true;
        }
        let v_a = string_splitter(&res.res_str_0, '\n');
        let v_b = string_splitter(&res.res_str_1, '\n');
        if v_a.len() > 50 || v_b.len() > 50 {
            res.comp_res = OraCompRes::Error;
            return true;
        }
        let a = count_unique_non_empty(&v_a);
        let b = count_unique_non_empty(&v_b);
        res.res_int_0 = a;
        res.res_int_1 = b;
        if a == 0 && v_b.iter().all(|r| r == "0") {
            res.comp_res = OraCompRes::Pass;
            return false;
        }
        res.comp_res = if a != b {
            OraCompRes::Fail
        } else {
            OraCompRes::Pass
        };
        false
    }

    fn compare_aggr(res: &mut CompRes) -> bool {
        if find_string_in(&res.res_str_0, "ERROR") || find_string_in(&res.res_str_1, "ERROR") {
            res.comp_res = OraCompRes::Error;
            return true;
        }
        let (Some(a), Some(b)) = (
            parse_leading_int(&res.res_str_0),
            parse_leading_int(&res.res_str_1),
        ) else {
            res.comp_res = OraCompRes::Error;
            return true;
        };
        res.res_int_0 = a;
        res.res_int_1 = b;
        res.comp_res = if a != b {
            OraCompRes::Fail
        } else {
            OraCompRes::Pass
        };
        false
    }
}

/// Count the rows of a result block, like the C++: newlines minus empty lines.
fn count_non_empty(block: &str) -> i64 {
    let newlines = block.matches('\n').count() as i64;
    let empties = string_splitter(block, '\n')
        .iter()
        .filter(|l| is_str_empty(l))
        .count() as i64;
    newlines - empties
}

fn count_unique_non_empty(rows: &[String]) -> i64 {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut count = rows.len() as i64 - 1;
    for r in rows {
        if is_str_empty(r) || !seen.insert(r.as_str()) {
            count -= 1;
        }
    }
    count
}

impl Oracle for Tlp {
    fn oracle_type(&self) -> &'static str {
        "TLP"
    }

    fn template_select_stmt(&self, mutator: &mut Mutator) -> String {
        TEMPLATES[mutator.rng.rand_int(TEMPLATES.len())].to_string()
    }

    fn is_oracle_select_stmt(&self, t: &mut IrTree, stmt: IrId) -> bool {
        if t.ty(stmt) != IrType::SelectStmt {
            return false;
        }
        if !ir_wrapper::check_node_num(t, stmt, 300) {
            return false;
        }
        if ir_wrapper::is_exist_limit_clause(t, stmt) || ir_wrapper::is_exist_set_operator(t, stmt)
        {
            return false;
        }
        // A window function looks like an aggregate but returns one row per
        // input row, so the partitioned form cannot be compared with it.
        for ty in [IrType::OverClause, IrType::WindowClause] {
            if ir_wrapper::get_ir_node_in_stmt_with_type_full(t, stmt, ty, false, true, true)
                .iter()
                .any(|&c| !t.is_empty(c))
            {
                return false;
            }
        }
        let froms = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::FromClause, false);
        let wheres = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::WhereClause, false);
        if froms.is_empty()
            || wheres.is_empty()
            || ir_wrapper::get_num_target_el_in_select_clause(t, stmt) != 1
        {
            return false;
        }
        !t.is_empty(froms[0]) && !t.is_empty(wheres[0])
    }

    fn post_fix_transform_select_stmt(
        &self,
        _mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
        _run: usize,
    ) -> Vec<IrTree> {
        t.get_mut(stmt).parent = None;
        // Keep only the first select target, in the source statement.
        if let Some(mut target_list) =
            ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::TargetList, false)
                .first()
                .copied()
        {
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
        }
        // First form: the statement without its WHERE clause.
        let mut first = t.extract(stmt);
        let first_root = first.root.expect("root");
        let wheres = ir_wrapper::get_ir_node_in_stmt_with_type(
            &first,
            first_root,
            IrType::WhereClause,
            false,
        );
        let Some(where_clause) = wheres.last().copied() else {
            return Vec::new();
        };
        first.detach_node(first_root, where_clause);
        first.deep_drop(where_clause);

        let tlp_type = Self::get_stmt_tlp_type(t, stmt);
        let second = match tlp_type {
            TlpType::AggrAvg | TlpType::AggrMax | TlpType::AggrMin | TlpType::AggrSum => {
                Self::transform_aggr(t, stmt, true, tlp_type)
            }
            TlpType::Distinct | TlpType::GroupBy => Self::transform_non_aggr(t, stmt, false),
            TlpType::Having | TlpType::Normal => Self::transform_non_aggr(t, stmt, true),
            TlpType::AggrCount | TlpType::Unknown => None,
        };
        match second {
            Some(s) => vec![first, s],
            None => Vec::new(),
        }
    }

    fn compare_results(&self, all: &mut AllCompRes) {
        all.final_res = Some(OraCompRes::Pass);
        let mut is_all_err = true;
        let types = Self::get_v_valid_type(&all.cmd_str);
        if types.len() != all.v_res.len() {
            for res in all.v_res.iter_mut() {
                res.comp_res = OraCompRes::Error;
                res.res_int_0 = -1;
                res.res_int_1 = -1;
            }
            all.final_res = Some(OraCompRes::Error);
            return;
        }
        for (res, ty) in all.v_res.iter_mut().zip(types.iter()) {
            match ty {
                TlpType::Normal => {
                    if !Self::compare_norm(res) {
                        is_all_err = false;
                    }
                }
                TlpType::Distinct | TlpType::GroupBy => {
                    Self::compare_uniq(res);
                }
                TlpType::AggrAvg | TlpType::AggrMax | TlpType::AggrMin | TlpType::AggrSum => {
                    if !Self::compare_aggr(res) {
                        is_all_err = false;
                    }
                }
                _ => res.comp_res = OraCompRes::Error,
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
    fn compares_the_rows_not_only_their_number() {
        let mut same_rows = CompRes {
            res_str_0: "\n1 \n2 \n\n".to_string(),
            res_str_1: "\n2 \n\n1 \n".to_string(),
            ..CompRes::default()
        };
        Tlp::compare_norm(&mut same_rows);
        assert_eq!(same_rows.comp_res, OraCompRes::Pass);
        let mut other_rows = CompRes {
            res_str_0: "\n1 \n2 \n".to_string(),
            res_str_1: "\n999 \n2 \n".to_string(),
            ..CompRes::default()
        };
        Tlp::compare_norm(&mut other_rows);
        assert_eq!(other_rows.comp_res, OraCompRes::Fail);
        let mut fewer_rows = CompRes {
            res_str_0: "\n1 \n2 \n".to_string(),
            res_str_1: "\n1 \n".to_string(),
            ..CompRes::default()
        };
        Tlp::compare_norm(&mut fewer_rows);
        assert_eq!(fewer_rows.comp_res, OraCompRes::Fail);
    }

    #[test]
    fn rejects_window_functions() {
        let tlp = Tlp;
        let mut q = sqlright_pg_parser::parse_query(
            "SELECT avg(c1) OVER (PARTITION BY c2) FROM v0 WHERE c1 > 0;",
        )
        .unwrap();
        let stmt = sqlright_mutator::ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert!(!tlp.is_oracle_select_stmt(&mut q.tree, stmt));
        let mut q =
            sqlright_pg_parser::parse_query("SELECT avg(c1) FROM v0 WHERE c1 > 0;").unwrap();
        let stmt = sqlright_mutator::ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert!(tlp.is_oracle_select_stmt(&mut q.tree, stmt));
    }

    #[test]
    fn partitions_a_normal_select() {
        let mut m = Mutator::new(3);
        let mut q = parse_query("SELECT v1 FROM v0 WHERE v1 > 1;").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert!(Tlp.is_oracle_select_stmt(&mut q.tree, stmt));
        let forms = Tlp.post_fix_transform_select_stmt(&mut m, &mut q.tree, stmt, 0);
        assert_eq!(forms.len(), 2);
        let texts: Vec<String> = forms.iter().map(|f| f.to_string(f.root.unwrap())).collect();
        assert_eq!(texts[0], "SELECT v1 FROM v0");
        assert_eq!(
            texts[1],
            "SELECT v1 FROM v0 WHERE ( v1 > 1 ) UNION ALL SELECT v1 FROM v0 WHERE ( NOT ( v1 > 1 ) ) UNION ALL SELECT v1 FROM v0 WHERE ( ( v1 > 1 ) IS NULL )"
        );
        assert!(parse_query(&texts[1]).is_some());
    }

    #[test]
    fn partitions_an_aggregate() {
        let mut m = Mutator::new(3);
        let mut q = parse_query("SELECT MAX(v1) FROM v0 WHERE v1 > 1;").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        assert_eq!(Tlp::get_stmt_tlp_type(&q.tree, stmt), TlpType::AggrMax);
        let forms = Tlp.post_fix_transform_select_stmt(&mut m, &mut q.tree, stmt, 0);
        assert_eq!(forms.len(), 2);
        let text = forms[1].to_string(forms[1].root.unwrap());
        assert!(text.starts_with("SELECT max ( aggr ) FROM ("), "{text}");
        assert!(text.contains("AS aggr"), "{text}");
        assert!(parse_query(&text).is_some(), "{text}");
        let mut q2 = parse_query("SELECT AVG(v1) FROM v0 WHERE v1 > 1;").unwrap();
        let stmt2 = ir_wrapper::get_first_stmt_from_root(&q2.tree, q2.root).unwrap();
        let forms2 = Tlp.post_fix_transform_select_stmt(&mut m, &mut q2.tree, stmt2, 0);
        let text2 = forms2[1].to_string(forms2[1].root.unwrap());
        assert!(
            text2.contains("SUM ( v1 ) AS s") && text2.contains("COUNT ( v1 ) AS c"),
            "{text2}"
        );
        assert!(
            text2.starts_with("SELECT sum ( s ) / sum ( c ) FROM ("),
            "{text2}"
        );
        assert!(parse_query(&text2).is_some(), "{text2}");
    }

    #[test]
    fn counts_rows() {
        let mut res = CompRes {
            res_str_0: "1 \n2 \n".into(),
            res_str_1: "1 \n2 \n3 \n".into(),
            ..CompRes::default()
        };
        assert!(!Tlp::compare_norm(&mut res));
        assert_eq!(res.comp_res, OraCompRes::Fail);
    }
}
