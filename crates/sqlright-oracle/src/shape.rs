//! Tests on the shape of a SELECT statement and on result blocks, shared
//! by the oracles that compare whole result rows (EET, DQE, LATERAL).

use sqlright_ir::util::find_string_in;
use sqlright_ir::{DataType, IrId, IrTree, IrType};
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{CompRes, OraCompRes};

use crate::tlp::AGGREGATE_NAMES;

/// Aggregates whose value depends on the order in which the rows arrive.
/// Two plans can give two different values for the same rows.
const ORDER_DEPENDENT_AGGREGATES: &[&str] = &[
    "string_agg",
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "jsonb_object_agg",
    "group_concat",
    "json_group_array",
    "json_group_object",
    "jsonb_group_array",
    "jsonb_group_object",
    "xmlagg",
    "first",
    "last",
    "mode",
];

/// A statement with more result rows than this is not compared. It is
/// most likely a cross join, and sorting its rows is slow.
pub const MAX_COMPARED_ROWS: usize = 2000;

/// The table of a `SELECT ... FROM <table> [AS alias]` with one plain table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleTable {
    pub name: String,
    pub alias: Option<String>,
}

impl SingleTable {
    /// The name a column reference uses: the alias when there is one.
    pub fn reference(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// The expression of the WHERE clause of the statement itself (not of a
/// subquery), when it is not empty.
pub fn where_expr(t: &IrTree, stmt: IrId) -> Option<IrId> {
    let wheres = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::WhereClause, false);
    let clause = *wheres.first()?;
    if t.is_empty(clause) {
        return None;
    }
    t.left(clause)
}

/// The one table of the FROM clause, when the FROM clause is a plain table
/// name with an optional alias: no join, no subquery, no `ONLY`, no
/// `TABLESAMPLE`, no schema, no column alias list.
pub fn single_table(t: &IrTree, stmt: IrId) -> Option<SingleTable> {
    let froms = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::FromClause, false);
    let from = *froms.first()?;
    let list = t.left(from)?;
    if t.ty(list) != IrType::FromList || t.right(list).is_some() {
        return None;
    }
    let table_ref = t.left(list)?;
    if t.ty(table_ref) != IrType::TableRef || !t.prefix(table_ref).is_empty() {
        return None;
    }
    let relation = t.left(table_ref)?;
    if t.ty(relation) != IrType::RelationExpr
        || !t.prefix(relation).is_empty()
        || !t.middle(relation).is_empty()
    {
        return None;
    }
    let qualified = t.left(relation)?;
    if t.ty(qualified) != IrType::QualifiedName || t.right(qualified).is_some() {
        return None;
    }
    let name_node = t.left(qualified)?;
    if t.ty(name_node) != IrType::Identifier {
        return None;
    }
    let name = t.str_val(name_node).to_string();
    if name.is_empty() {
        return None;
    }
    let alias = match t.right(table_ref) {
        None => None,
        Some(opt_alias) => {
            if t.is_empty(opt_alias) {
                None
            } else {
                let alias_clause = t.left(opt_alias)?;
                if t.ty(alias_clause) != IrType::AliasClause || t.right(alias_clause).is_some() {
                    return None;
                }
                let alias_node = t.left(alias_clause)?;
                if t.ty(alias_node) != IrType::Identifier {
                    return None;
                }
                let alias = t.str_val(alias_node).to_string();
                if alias.is_empty() {
                    return None;
                }
                Some(alias)
            }
        }
    };
    Some(SingleTable { name, alias })
}

/// True when the statement (not its subqueries) has a window function.
pub fn has_window_function(t: &IrTree, stmt: IrId) -> bool {
    [IrType::OverClause, IrType::WindowClause]
        .iter()
        .any(|&ty| {
            ir_wrapper::get_ir_node_in_stmt_with_type_full(t, stmt, ty, false, true, true)
                .iter()
                .any(|&c| !t.is_empty(c))
        })
}

/// True when the statement has a non-empty clause of this type outside
/// its subqueries.
pub fn has_clause(t: &IrTree, stmt: IrId, ty: IrType) -> bool {
    ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, ty, false)
        .iter()
        .any(|&c| !t.is_empty(c))
}

/// True when the statement has `DISTINCT` (not `DISTINCT ON`, not inside
/// a subquery).
pub fn has_distinct(t: &IrTree, stmt: IrId) -> bool {
    if has_clause(t, stmt, IrType::DistinctClause) {
        return true;
    }
    ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::SetQuantifier, false)
        .iter()
        .any(|&sq| !t.is_empty(sq) && t.prefix(sq) == "DISTINCT")
}

/// The lowercase function names the node calls, outside subqueries of `stmt`.
fn function_names(t: &IrTree, stmt: IrId, below: IrId) -> Vec<String> {
    ir_wrapper::get_ir_node_in_stmt_with_type(t, below, IrType::FuncName, false)
        .into_iter()
        .filter(|&f| !ir_wrapper::is_in_subquery(t, stmt, f))
        .filter_map(|f| t.left(f))
        .map(|iden| {
            let name = t.str_val(iden).to_ascii_lowercase();
            name.rsplit('.').next().unwrap_or(&name).trim().to_string()
        })
        .collect()
}

fn is_aggregate_name(name: &str) -> bool {
    AGGREGATE_NAMES.contains(&name) || ORDER_DEPENDENT_AGGREGATES.contains(&name)
}

/// True when the statement (not its subqueries) calls an aggregate function.
pub fn has_aggregate_call(t: &IrTree, stmt: IrId) -> bool {
    function_names(t, stmt, stmt)
        .iter()
        .any(|n| is_aggregate_name(n))
}

/// The text without whitespace, for comparing two printed expressions.
pub fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// True when the rows of the statement can differ between two correct
/// plans, so that two equivalent forms cannot be compared:
///
/// - a window function (its values depend on the scan order without an
///   `ORDER BY` in the window),
/// - an aggregate whose value depends on the row order (`string_agg`),
/// - `GROUP BY` with a target that is neither grouped nor aggregated
///   (PostgreSQL rejects it, tursopg returns the value of some row),
/// - `HAVING` without `GROUP BY`, or an aggregate next to a bare column
///   without `GROUP BY` (the same arbitrary row).
pub fn output_depends_on_the_plan(t: &IrTree, stmt: IrId) -> bool {
    if has_window_function(t, stmt) {
        return true;
    }
    let names = function_names(t, stmt, stmt);
    if names
        .iter()
        .any(|n| ORDER_DEPENDENT_AGGREGATES.contains(&n.as_str()))
    {
        return true;
    }
    let has_group_by = has_clause(t, stmt, IrType::GroupClause);
    let has_having = has_clause(t, stmt, IrType::HavingClause);
    if has_having && !has_group_by {
        return true;
    }
    let has_aggregate = names.iter().any(|n| is_aggregate_name(n));
    if !has_group_by && !has_aggregate {
        return false;
    }
    let group_items: Vec<String> =
        ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::GroupByItem, false)
            .iter()
            .map(|&item| squeeze(&t.to_string(item)))
            .collect();
    for target in ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::TargetEl, false) {
        if ir_wrapper::is_in_subquery(t, stmt, target) {
            continue;
        }
        let Some(expr) = t.left(target) else {
            // `*` is never grouped.
            return true;
        };
        if has_ungrouped_bare_column(t, stmt, expr, &group_items) {
            return true;
        }
    }
    false
}

/// True when a column reference of the target expression is neither
/// inside an aggregate call nor part of a GROUP BY expression. Such a
/// column has the value of some row of the group (`sum(c1) + c2`).
fn has_ungrouped_bare_column(t: &IrTree, stmt: IrId, expr: IrId, group_items: &[String]) -> bool {
    ir_wrapper::preorder(t, expr)
        .into_iter()
        .filter(|&n| t.ty(n) == IrType::Columnref && !ir_wrapper::is_in_subquery(t, stmt, n))
        .any(|column| {
            let mut cur = column;
            loop {
                if group_items.contains(&squeeze(&t.to_string(cur))) {
                    return false;
                }
                if is_aggregate_call(t, cur) {
                    return false;
                }
                if cur == expr {
                    return true;
                }
                match t.parent(cur) {
                    Some(p) => cur = p,
                    None => return true,
                }
            }
        })
}

/// True when the node is a call of an aggregate function.
fn is_aggregate_call(t: &IrTree, node: IrId) -> bool {
    if !ir_wrapper::compare_ir_type(t.ty(node), IrType::FuncApplication) {
        return false;
    }
    let Some(func_name) = t.left(node) else {
        return false;
    };
    if t.ty(func_name) != IrType::FuncName {
        return false;
    }
    let Some(iden) = t.left(func_name) else {
        return false;
    };
    let name = t.str_val(iden).to_ascii_lowercase();
    is_aggregate_name(name.rsplit('.').next().unwrap_or(&name).trim())
}

/// True when a subquery of the statement reads the table.
pub fn subquery_reads_table(t: &IrTree, stmt: IrId, table: &str) -> bool {
    ir_wrapper::preorder(t, stmt).into_iter().any(|id| {
        t.ty(id) == IrType::Identifier
            && t.data_type(id) == DataType::TableName
            && t.str_val(id).eq_ignore_ascii_case(table)
            && ir_wrapper::is_in_subquery(t, stmt, id)
    })
}

/// The rows of a result block, in order. The block is the text between
/// the `BEGIN VERI n` and `END VERI n` markers after `extract_query_result`:
/// the rest of the marker line, then one line per row. A row of NULLs is
/// an empty line and is kept, unlike in the TLP row count.
pub fn result_rows(block: &str) -> Vec<String> {
    let mut parts: Vec<&str> = block.split('\n').collect();
    if parts.len() < 2 {
        return Vec::new();
    }
    parts.remove(0);
    parts.pop();
    parts.iter().map(|r| r.trim_end().to_string()).collect()
}

/// True when two printed values are the same, or both are numbers that
/// differ by less than one part in a billion. Two plans can add floating
/// point values in another order.
fn same_value(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (Ok(x), Ok(y)) = (a.parse::<f64>(), b.parse::<f64>()) else {
        return false;
    };
    if !x.is_finite() || !y.is_finite() {
        return false;
    }
    let tolerance = 1e-9 * x.abs().max(y.abs()).max(1.0);
    (x - y).abs() <= tolerance
}

fn same_row(a: &str, b: &str) -> bool {
    let va: Vec<&str> = a.split(' ').collect();
    let vb: Vec<&str> = b.split(' ').collect();
    va.len() == vb.len() && va.iter().zip(vb.iter()).all(|(x, y)| same_value(x, y))
}

/// True when the two lists hold the same rows, in any order.
pub fn same_rows(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut sa: Vec<&String> = a.iter().collect();
    let mut sb: Vec<&String> = b.iter().collect();
    sa.sort();
    sb.sort();
    sa.iter().zip(sb.iter()).all(|(x, y)| same_row(x, y))
}

/// Compare the rows of the forms of one oracle SELECT. The forms are the
/// result blocks that are not `EMPTY` (an oracle can emit two or three).
/// Returns true when the forms could not be compared.
pub fn compare_row_sets(res: &mut CompRes) -> bool {
    let blocks: Vec<&String> = [
        &res.res_str_0,
        &res.res_str_1,
        &res.res_str_2,
        &res.res_str_3,
    ]
    .into_iter()
    .filter(|s| s.as_str() != "EMPTY")
    .collect();
    if blocks.len() < 2 {
        res.comp_res = OraCompRes::Error;
        return true;
    }
    if blocks.iter().any(|s| find_string_in(s, "ERROR")) {
        res.comp_res = OraCompRes::Error;
        return true;
    }
    let rows: Vec<Vec<String>> = blocks.iter().map(|s| result_rows(s)).collect();
    if rows.iter().any(|r| r.len() > MAX_COMPARED_ROWS) {
        res.comp_res = OraCompRes::Error;
        return true;
    }
    let counts: Vec<i64> = rows.iter().map(|r| r.len() as i64).collect();
    res.res_int_0 = counts[0];
    res.res_int_1 = counts[1];
    if let Some(c) = counts.get(2) {
        res.res_int_2 = *c;
    }
    if let Some(c) = counts.get(3) {
        res.res_int_3 = *c;
    }
    let all_same = rows.iter().skip(1).all(|r| same_rows(&rows[0], r));
    res.comp_res = if all_same {
        OraCompRes::Pass
    } else {
        OraCompRes::Fail
    };
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlright_pg_parser::parse_query;

    fn first_stmt(sql: &str) -> (IrTree, IrId) {
        let q = parse_query(sql).unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        (q.tree, stmt)
    }

    #[test]
    fn finds_the_single_table_and_its_alias() {
        let (t, s) = first_stmt("SELECT c1 FROM v0 AS a0 WHERE a0.c1 > 0;");
        assert_eq!(
            single_table(&t, s),
            Some(SingleTable {
                name: "v0".into(),
                alias: Some("a0".into())
            })
        );
        let (t, s) = first_stmt("SELECT c1 FROM v0 WHERE c1 > 0;");
        assert_eq!(
            single_table(&t, s),
            Some(SingleTable {
                name: "v0".into(),
                alias: None
            })
        );
        let (t, s) = first_stmt("SELECT c1 FROM v0 a0 WHERE c1 > 0;");
        assert_eq!(single_table(&t, s).unwrap().alias, Some("a0".into()));
        for sql in [
            "SELECT c1 FROM v0, v1 WHERE c1 > 0;",
            "SELECT c1 FROM v0 JOIN v1 ON TRUE WHERE c1 > 0;",
            "SELECT c1 FROM (SELECT 1 AS c1) AS s WHERE c1 > 0;",
            "SELECT c1 FROM ONLY v0 WHERE c1 > 0;",
            "SELECT c1 FROM v0 AS a0 (x) WHERE c1 > 0;",
            "SELECT c1 FROM s.v0 WHERE c1 > 0;",
            "SELECT 1;",
        ] {
            let (t, s) = first_stmt(sql);
            assert_eq!(single_table(&t, s), None, "{sql}");
        }
    }

    #[test]
    fn knows_when_the_rows_depend_on_the_plan() {
        for sql in [
            "SELECT c1 FROM v0 WHERE c1 > 0 GROUP BY c2;",
            "SELECT * FROM v0 WHERE c1 > 0 GROUP BY c1;",
            "SELECT c1 FROM v0 WHERE c1 > 0 HAVING sum(c2) > 1;",
            "SELECT c1, sum(c2) FROM v0 WHERE c1 > 0;",
            "SELECT sum(c1) + c2 FROM v0 WHERE c1 > 0;",
            "SELECT sum(c1) + c2 FROM v0 WHERE c1 > 0 GROUP BY c1;",
            "SELECT c1 + c2 FROM v0 WHERE c1 > 0 GROUP BY c1;",
            "SELECT max(c1), c2 + 1 FROM v0 WHERE c1 > 0 GROUP BY c1;",
            "SELECT string_agg(c1, ',') FROM v0 WHERE c1 > 0;",
            "SELECT sum(c1) OVER () FROM v0 WHERE c1 > 0;",
        ] {
            let (t, s) = first_stmt(sql);
            assert!(output_depends_on_the_plan(&t, s), "{sql}");
        }
        for sql in [
            "SELECT c1 FROM v0 WHERE c1 > 0;",
            "SELECT * FROM v0 WHERE c1 > 0;",
            "SELECT c1, c2 FROM v0 WHERE c1 > 0 GROUP BY c1, c2;",
            "SELECT c1, sum(c2) FROM v0 WHERE c1 > 0 GROUP BY c1;",
            "SELECT sum(c1) + c2 FROM v0 WHERE c1 > 0 GROUP BY c2;",
            "SELECT c1 + 1, max(c2) FROM v0 WHERE c1 > 0 GROUP BY c1;",
            "SELECT sum(c1 + c2) + 1 FROM v0 WHERE c1 > 0;",
            "SELECT sum(c2), max(c1) FROM v0 WHERE c1 > 0;",
            "SELECT count(*) FROM v0 WHERE c1 > 0;",
            "SELECT c1 FROM v0 WHERE c1 > (SELECT c2 FROM v1 GROUP BY c3 HAVING sum(c4) > 1);",
            "SELECT max((SELECT c3 FROM v1 WHERE v1.c4 = c2)) FROM v0 WHERE c1 > 0;",
        ] {
            let (t, s) = first_stmt(sql);
            assert!(!output_depends_on_the_plan(&t, s), "{sql}");
        }
    }

    #[test]
    fn finds_a_subquery_on_the_same_table() {
        let (t, s) = first_stmt("SELECT * FROM v0 WHERE c1 IN (SELECT c1 FROM v0 WHERE c2 > 0);");
        assert!(subquery_reads_table(&t, s, "v0"));
        assert!(!subquery_reads_table(&t, s, "v1"));
        let (t, s) = first_stmt("SELECT * FROM v0 WHERE c1 IN (SELECT c1 FROM v1);");
        assert!(!subquery_reads_table(&t, s, "v0"));
    }

    #[test]
    fn keeps_rows_of_nulls_and_compares_in_any_order() {
        // The block as `extract_query_result` gives it: the end of the
        // marker line, one line per row, a NULL row as an empty line.
        assert_eq!(result_rows("\n1 2 \n\n3 \n"), vec!["1 2", "", "3"]);
        assert_eq!(result_rows("\n"), Vec::<String>::new());
        assert_eq!(result_rows("\n\n"), vec![""]);
        let a: Vec<String> = vec!["1 2".into(), "".into(), "3".into()];
        let b: Vec<String> = vec!["3".into(), "1 2".into(), "".into()];
        assert!(same_rows(&a, &b));
        let c: Vec<String> = vec!["3".into(), "1 2".into()];
        assert!(!same_rows(&a, &c));
        let d: Vec<String> = vec!["1.0000000001".into()];
        let e: Vec<String> = vec!["1".into()];
        assert!(same_rows(&d, &e));
        let f: Vec<String> = vec!["1.1".into()];
        assert!(!same_rows(&d, &f));
    }

    #[test]
    fn compares_two_or_three_forms() {
        let mut same = CompRes {
            res_str_0: "\n1 \n2 \n".into(),
            res_str_1: "\n2 \n1 \n".into(),
            res_str_2: "\n1 \n2 \n".into(),
            ..CompRes::default()
        };
        assert!(!compare_row_sets(&mut same));
        assert_eq!(same.comp_res, OraCompRes::Pass);
        let mut other = CompRes {
            res_str_0: "\n1 \n2 \n".into(),
            res_str_1: "\n2 \n1 \n".into(),
            res_str_2: "\n1 \n".into(),
            ..CompRes::default()
        };
        assert!(!compare_row_sets(&mut other));
        assert_eq!(other.comp_res, OraCompRes::Fail);
        let mut error = CompRes {
            res_str_0: "\n1 \n".into(),
            res_str_1: "Error".into(),
            ..CompRes::default()
        };
        assert!(compare_row_sets(&mut error));
        assert_eq!(error.comp_res, OraCompRes::Error);
        let mut one_form = CompRes {
            res_str_0: "\n1 \n".into(),
            ..CompRes::default()
        };
        assert!(compare_row_sets(&mut one_form));
    }
}
