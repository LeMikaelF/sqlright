//! The EET oracle, equivalent expression transformation (Jiang and Su,
//! OSDI 2024), as SQLancer implements it.
//!
//! An expression keeps its value when it is wrapped in a form that the
//! database cannot simplify. For a predicate `p` and an expression `e`:
//!
//! - `true_expr(p)` is `(p) OR (NOT (p)) OR ((p) IS NULL)`: always true.
//! - `false_expr(p)` is `(p) AND (NOT (p)) AND ((p) IS NOT NULL)`: always false.
//! - Rule 1: `e` becomes `false_expr OR e` (boolean context only).
//! - Rule 2: `e` becomes `true_expr AND e` (boolean context only).
//! - Rule 3: `e` becomes `CASE WHEN false_expr THEN dead ELSE e END`.
//! - Rule 4: `e` becomes `CASE WHEN true_expr THEN e ELSE dead END`.
//! - Rule 5 and 6: `e` becomes `CASE WHEN p THEN e ELSE e END`.
//!
//! The predicates `p` are sub-expressions of the WHERE clause itself, so
//! they have the names of the test case. The dead branch is `NULL` or a
//! copy of `e`. The original SELECT and the transformed SELECT must return
//! the same rows.

use sqlright_ir::{IrId, IrTree, IrType, Op};
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{AllCompRes, OraCompRes, Oracle};
use sqlright_mutator::rng::Rng;
use sqlright_mutator::Mutator;

use crate::shape::{
    compare_row_sets, has_clause, has_window_function, output_depends_on_the_plan, where_expr,
};

pub struct Eet;

const TEMPLATES: &[&str] = &[
    "SELECT * FROM x WHERE x = 0;",
    "SELECT x FROM x WHERE x = 0;",
    "SELECT x, x FROM x WHERE x > 0 AND x < 10;",
    "SELECT x FROM x WHERE x IS NULL OR x = 'x';",
    "SELECT x FROM x WHERE x = 0 ORDER BY x;",
    "SELECT DISTINCT x FROM x WHERE x = 0;",
    "SELECT COUNT(*) FROM x WHERE x = 0;",
    "SELECT MAX(x), MIN(x) FROM x WHERE x = 0;",
    "SELECT x, SUM(x) FROM x WHERE x = 0 GROUP BY x;",
    "SELECT x FROM x WHERE x IN (SELECT x FROM x WHERE x = 0);",
];

/// How many rule applications one statement gets at most. Every
/// application copies a predicate, so the text grows fast.
const MAX_APPLICATIONS: usize = 8;

/// A predicate larger than this is not copied into `true_expr` and
/// `false_expr`.
const MAX_PREDICATE_NODES: usize = 40;

/// Operators whose result is a boolean. An expression with one of them at
/// its top can be the `p` of `true_expr(p)` in PostgreSQL too, where a
/// non-boolean `p` is an error.
const BOOLEAN_OPERATORS: &[&str] = &[
    "=",
    "<>",
    "!=",
    "<",
    ">",
    "<=",
    ">=",
    "AND",
    "OR",
    "IN",
    "NOT IN",
    "LIKE",
    "NOT LIKE",
    "ILIKE",
    "NOT ILIKE",
    "SIMILAR TO",
    "NOT SIMILAR TO",
    "BETWEEN",
    "NOT BETWEEN",
    "BETWEEN SYMMETRIC",
    "NOT BETWEEN SYMMETRIC",
    "IS NULL",
    "IS NOT NULL",
    "ISNULL",
    "NOTNULL",
    "IS TRUE",
    "IS NOT TRUE",
    "IS FALSE",
    "IS NOT FALSE",
    "IS UNKNOWN",
    "IS NOT UNKNOWN",
    "IS DISTINCT FROM",
    "IS NOT DISTINCT FROM",
    "OVERLAPS",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    FalseOr,
    TrueAnd,
    CaseFalseDead,
    CaseTrueDead,
    CaseSame,
}

impl Oracle for Eet {
    fn oracle_type(&self) -> &'static str {
        "EET"
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
        if ir_wrapper::is_exist_limit_clause(t, stmt)
            || ir_wrapper::is_exist_set_operator(t, stmt)
            || has_clause(t, stmt, IrType::IntoClause)
            || has_clause(t, stmt, IrType::ForLockingClause)
            || has_window_function(t, stmt)
        {
            return false;
        }
        let froms = ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::FromClause, false);
        let Some(from) = froms.first() else {
            return false;
        };
        if t.is_empty(*from) || where_expr(t, stmt).is_none() {
            return false;
        }
        !output_depends_on_the_plan(t, stmt)
    }

    fn post_fix_transform_select_stmt(
        &self,
        mutator: &mut Mutator,
        t: &mut IrTree,
        stmt: IrId,
        _run: usize,
    ) -> Vec<IrTree> {
        t.get_mut(stmt).parent = None;
        // The names are filled in after `is_oracle_select_stmt`: a grouped
        // target can have become an ungrouped one.
        if !self.is_oracle_select_stmt(t, stmt) {
            return Vec::new();
        }
        let first = t.extract(stmt);
        let mut second = t.extract(stmt);
        let root = second.root.expect("root");
        if !transform_statement(&mut second, root, &mut mutator.rng) {
            return Vec::new();
        }
        vec![first, second]
    }

    fn compare_results(&self, all: &mut AllCompRes) {
        all.final_res = Some(OraCompRes::Pass);
        let mut is_all_err = true;
        for res in all.v_res.iter_mut() {
            // The oracle emits two forms.
            res.res_str_2 = "EMPTY".to_string();
            res.res_str_3 = "EMPTY".to_string();
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

/// Transform the WHERE clause and some select targets of the statement in
/// place. Returns false when nothing could be transformed.
pub fn transform_statement(t: &mut IrTree, stmt: IrId, rng: &mut Rng) -> bool {
    let Some(predicate) = where_expr(t, stmt) else {
        return false;
    };
    // The predicates `p` of the rules, copied before the tree changes.
    let small: Vec<IrId> = boolean_subexpressions(t, stmt, predicate)
        .into_iter()
        .filter(|&n| t.count_nodes(n) <= MAX_PREDICATE_NODES)
        .collect();
    let mut sources: Vec<IrId> = small.into_iter().map(|n| t.deep_copy(n)).collect();
    if sources.is_empty() {
        let copy = t.deep_copy(predicate);
        sources.push(is_null(t, copy));
    }
    // The nodes to wrap and their context, decided before the tree changes.
    let mut sites: Vec<(IrId, bool)> = Vec::new();
    for node in ir_wrapper::preorder(t, predicate) {
        if !ir_wrapper::compare_ir_type(t.ty(node), IrType::AExpr)
            || ir_wrapper::is_in_subquery(t, stmt, node)
        {
            continue;
        }
        if node == predicate || rng.chance(1, 2) {
            sites.push((node, is_boolean_context(t, predicate, node)));
        }
    }
    let targets = scalar_targets(t, stmt);
    for target in targets {
        if rng.chance(1, 3) {
            sites.push((target, false));
        }
    }
    let mut applied = 0;
    for (node, boolean) in sites {
        if applied >= MAX_APPLICATIONS {
            break;
        }
        let rule = pick_rule(rng, boolean);
        let source = sources[rng.rand_int(sources.len())];
        apply_rule(t, stmt, node, rule, source, rng);
        applied += 1;
    }
    applied > 0
}

/// True when only the truth value of the node matters: the whole WHERE
/// predicate, and the operands of `AND`, `OR` and `NOT`. Parentheses and
/// the grammar's wrapper nodes pass the context of their parent down.
fn is_boolean_context(t: &IrTree, predicate: IrId, node: IrId) -> bool {
    if node == predicate {
        return true;
    }
    let Some(parent) = t.parent(node) else {
        return false;
    };
    let middle = t.middle(parent).trim();
    let prefix = t.prefix(parent).trim();
    if middle == "AND" || middle == "OR" || prefix == "NOT" {
        return true;
    }
    // The parenthesis node of the grammar has an empty `opt_indirection`
    // on its right.
    let right_is_empty = t.right(parent).is_none_or(|r| t.is_empty(r));
    let is_wrapper = right_is_empty
        && (t.get(parent).op.as_ref().is_none_or(|op| op.is_empty())
            || (prefix == "(" && middle == ")"));
    is_wrapper && is_boolean_context(t, predicate, parent)
}

/// The sub-expressions of the predicate whose value is a boolean: the
/// predicate itself and every comparison, `AND`, `OR`, `NOT`, `IS ...`,
/// `IN`, `LIKE`, `BETWEEN` and `EXISTS` below it (not inside subqueries).
fn boolean_subexpressions(t: &IrTree, stmt: IrId, predicate: IrId) -> Vec<IrId> {
    let mut out = vec![predicate];
    for node in ir_wrapper::preorder(t, predicate) {
        if node == predicate || ir_wrapper::is_in_subquery(t, stmt, node) {
            continue;
        }
        let ty = t.ty(node);
        if !ir_wrapper::compare_ir_type(ty, IrType::AExpr)
            && !ir_wrapper::compare_ir_type(ty, IrType::CExpr)
        {
            continue;
        }
        let middle = t.middle(node).trim().to_ascii_uppercase();
        let prefix = t.prefix(node).trim().to_ascii_uppercase();
        if BOOLEAN_OPERATORS.contains(&middle.as_str()) || prefix == "NOT" || prefix == "EXISTS" {
            out.push(node);
        }
    }
    out
}

/// The target expressions that a scalar rule can wrap: every target that
/// is not `*`, when the statement has no aggregate and no `GROUP BY`
/// (there a bare column inside the predicate `p` is not grouped).
fn scalar_targets(t: &IrTree, stmt: IrId) -> Vec<IrId> {
    if has_clause(t, stmt, IrType::GroupClause) || crate::shape::has_aggregate_call(t, stmt) {
        return Vec::new();
    }
    ir_wrapper::get_ir_node_in_stmt_with_type(t, stmt, IrType::TargetEl, false)
        .into_iter()
        .filter(|&target| !ir_wrapper::is_in_subquery(t, stmt, target))
        .filter_map(|target| t.left(target))
        .filter(|&expr| ir_wrapper::compare_ir_type(t.ty(expr), IrType::AExpr))
        .collect()
}

fn pick_rule(rng: &mut Rng, boolean: bool) -> Rule {
    let rules: &[Rule] = if boolean {
        &[
            Rule::FalseOr,
            Rule::TrueAnd,
            Rule::CaseFalseDead,
            Rule::CaseTrueDead,
            Rule::CaseSame,
        ]
    } else {
        &[Rule::CaseFalseDead, Rule::CaseTrueDead, Rule::CaseSame]
    };
    rules[rng.rand_int(rules.len())]
}

/// Wrap `node` (inside `stmt`) in the form of the rule, with `source` as
/// the predicate `p`.
fn apply_rule(t: &mut IrTree, stmt: IrId, node: IrId, rule: Rule, source: IrId, rng: &mut Rng) {
    // `node` stays in the tree until the wrapper exists; `holder` keeps
    // its place inside the wrapper until then.
    let holder = t.leaf(IrType::AExpr, "");
    let wrapper = match rule {
        Rule::FalseOr => {
            let f = false_expr(t, source);
            binary(t, f, "OR", holder)
        }
        Rule::TrueAnd => {
            let tr = true_expr(t, source);
            binary(t, tr, "AND", holder)
        }
        Rule::CaseFalseDead => {
            let f = false_expr(t, source);
            let dead = dead_branch(t, node, rng);
            case_when(t, f, dead, holder)
        }
        Rule::CaseTrueDead => {
            let tr = true_expr(t, source);
            let dead = dead_branch(t, node, rng);
            case_when(t, tr, holder, dead)
        }
        Rule::CaseSame => {
            let p = t.deep_copy(source);
            let copy = t.deep_copy(node);
            if rng.chance(1, 2) {
                case_when(t, p, holder, copy)
            } else {
                case_when(t, p, copy, holder)
            }
        }
    };
    if !t.swap_node(stmt, node, Some(wrapper)) {
        return;
    }
    let parent = t.parent(holder).expect("the holder is inside the wrapper");
    if t.left(parent) == Some(holder) {
        t.update_left(parent, Some(node));
    } else {
        t.update_right(parent, Some(node));
    }
    t.deep_drop(holder);
}

/// `NULL`, or a copy of the expression (the `copy_expr` form of the paper).
fn dead_branch(t: &mut IrTree, node: IrId, rng: &mut Rng) -> IrId {
    if rng.chance(1, 2) {
        t.leaf(IrType::AexprConst, "NULL")
    } else {
        t.deep_copy(node)
    }
}

fn paren(t: &mut IrTree, e: IrId) -> IrId {
    t.node(IrType::AExpr, Op::new("(", ")", ""), Some(e), None)
}

fn not(t: &mut IrTree, e: IrId) -> IrId {
    let inner = paren(t, e);
    t.node(IrType::AExpr, Op::new("NOT", "", ""), Some(inner), None)
}

fn is_null(t: &mut IrTree, e: IrId) -> IrId {
    let inner = paren(t, e);
    t.node(IrType::AExpr, Op::new("", "IS NULL", ""), Some(inner), None)
}

fn is_not_null(t: &mut IrTree, e: IrId) -> IrId {
    let inner = paren(t, e);
    t.node(
        IrType::AExpr,
        Op::new("", "IS NOT NULL", ""),
        Some(inner),
        None,
    )
}

fn binary(t: &mut IrTree, a: IrId, keyword: &'static str, b: IrId) -> IrId {
    let l = paren(t, a);
    let r = paren(t, b);
    t.node(IrType::AExpr, Op::new("", keyword, ""), Some(l), Some(r))
}

/// `(p) OR (NOT (p)) OR ((p) IS NULL)`, with three copies of the source.
fn true_expr(t: &mut IrTree, source: IrId) -> IrId {
    let p1 = t.deep_copy(source);
    let p2 = t.deep_copy(source);
    let p3 = t.deep_copy(source);
    let negated = not(t, p2);
    let null = is_null(t, p3);
    let first = binary(t, p1, "OR", negated);
    binary(t, first, "OR", null)
}

/// `(p) AND (NOT (p)) AND ((p) IS NOT NULL)`, with three copies of the source.
fn false_expr(t: &mut IrTree, source: IrId) -> IrId {
    let p1 = t.deep_copy(source);
    let p2 = t.deep_copy(source);
    let p3 = t.deep_copy(source);
    let negated = not(t, p2);
    let not_null = is_not_null(t, p3);
    let first = binary(t, p1, "AND", negated);
    binary(t, first, "AND", not_null)
}

/// `( CASE WHEN ( c ) THEN ( a ) ELSE ( b ) END )`.
fn case_when(t: &mut IrTree, c: IrId, a: IrId, b: IrId) -> IrId {
    let c = paren(t, c);
    let a = paren(t, a);
    let b = paren(t, b);
    let tail = t.node(IrType::AExpr, Op::new("", "ELSE", "END"), Some(a), Some(b));
    let head = t.node(
        IrType::AExpr,
        Op::new("CASE WHEN", "THEN", ""),
        Some(c),
        Some(tail),
    );
    paren(t, head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlright_mutator::oracle::CompRes;
    use sqlright_pg_parser::parse_query;

    fn forms(sql: &str, seed: u64) -> Vec<String> {
        let mut m = Mutator::new(seed);
        let mut q = parse_query(sql).unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        Eet.post_fix_transform_select_stmt(&mut m, &mut q.tree, stmt, 0)
            .iter()
            .map(|f| f.to_string(f.root.unwrap()))
            .collect()
    }

    #[test]
    fn accepts_selects_with_from_and_where() {
        for sql in [
            "SELECT * FROM v0 WHERE c1 = 1;",
            "SELECT c1, c2 FROM v0 AS a WHERE a.c1 > 0 ORDER BY c2;",
            "SELECT count(*) FROM v0 WHERE c1 = 1;",
            "SELECT c1, sum(c2) FROM v0 WHERE c1 = 1 GROUP BY c1 HAVING sum(c2) > 0;",
        ] {
            let mut q = parse_query(sql).unwrap();
            let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
            assert!(Eet.is_oracle_select_stmt(&mut q.tree, stmt), "{sql}");
        }
        for sql in [
            "SELECT * FROM v0;",
            "SELECT 1 WHERE 1 = 1;",
            "SELECT * FROM v0 WHERE c1 = 1 LIMIT 1;",
            "SELECT c1 FROM v0 WHERE c1 = 1 UNION SELECT c1 FROM v0;",
            "SELECT c2 FROM v0 WHERE c1 = 1 GROUP BY c1;",
            "SELECT sum(c1) OVER () FROM v0 WHERE c1 = 1;",
            "SELECT string_agg(c1, ',') FROM v0 WHERE c1 = 1;",
        ] {
            let mut q = parse_query(sql).unwrap();
            let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
            assert!(!Eet.is_oracle_select_stmt(&mut q.tree, stmt), "{sql}");
        }
    }

    #[test]
    fn transforms_the_predicate_into_a_form_that_parses() {
        for seed in 1..40 {
            let texts = forms("SELECT c1 FROM v0 WHERE c1 > 1 AND c2 = 'x';", seed);
            assert_eq!(texts.len(), 2, "seed {seed}");
            assert_eq!(texts[0], "SELECT c1 FROM v0 WHERE c1 > 1 AND c2 = 'x'");
            assert_ne!(texts[1], texts[0], "seed {seed}");
            assert!(
                parse_query(&texts[1]).is_some(),
                "seed {seed}: {}",
                texts[1]
            );
            assert!(texts[1].contains("c1 > 1"), "{}", texts[1]);
        }
    }

    #[test]
    fn wraps_the_whole_predicate_every_time() {
        let texts = forms("SELECT * FROM v0 WHERE c1 = 1;", 3);
        let second = &texts[1];
        let wrapped =
            second.contains("CASE WHEN") || second.contains(") OR (") || second.contains(") AND (");
        assert!(wrapped, "{second}");
    }

    #[test]
    fn keeps_the_subquery_of_the_predicate() {
        for seed in 1..10 {
            let texts = forms(
                "SELECT c1 FROM v0 WHERE c1 IN (SELECT c2 FROM v1 WHERE c3 > 0);",
                seed,
            );
            assert_eq!(texts.len(), 2);
            assert!(
                texts[1].contains("SELECT c2 FROM v1 WHERE c3 > 0"),
                "{}",
                texts[1]
            );
            assert!(parse_query(&texts[1]).is_some(), "{}", texts[1]);
        }
    }

    #[test]
    fn builds_the_true_and_false_forms() {
        let mut t = IrTree::new();
        let p = t.leaf(IrType::AExpr, "c1 > 1");
        let tr = true_expr(&mut t, p);
        assert_eq!(
            t.to_string(tr),
            "( ( c1 > 1 ) OR ( NOT ( c1 > 1 ) ) ) OR ( ( c1 > 1 ) IS NULL )"
        );
        let f = false_expr(&mut t, p);
        assert_eq!(
            t.to_string(f),
            "( ( c1 > 1 ) AND ( NOT ( c1 > 1 ) ) ) AND ( ( c1 > 1 ) IS NOT NULL )"
        );
        let a = t.leaf(IrType::AExpr, "c2");
        let b = t.leaf(IrType::AExpr, "NULL");
        let c = case_when(&mut t, p, a, b);
        assert_eq!(
            t.to_string(c),
            "( CASE WHEN ( c1 > 1 ) THEN ( c2 ) ELSE ( NULL ) END )"
        );
    }

    #[test]
    fn knows_the_boolean_context() {
        let q =
            parse_query("SELECT 1 FROM v0 WHERE (c1 > 1 AND NOT (c2 = 2)) OR c3 + 1 = 4;").unwrap();
        let stmt = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root).unwrap();
        let predicate = where_expr(&q.tree, stmt).unwrap();
        let mut boolean = Vec::new();
        let mut scalar = Vec::new();
        for node in ir_wrapper::preorder(&q.tree, predicate) {
            if !ir_wrapper::compare_ir_type(q.tree.ty(node), IrType::AExpr) {
                continue;
            }
            let text = q.tree.to_string(node);
            if is_boolean_context(&q.tree, predicate, node) {
                boolean.push(text);
            } else {
                scalar.push(text);
            }
        }
        assert!(boolean.contains(&"c1 > 1".to_string()), "{boolean:?}");
        assert!(boolean.contains(&"c2 = 2".to_string()), "{boolean:?}");
        assert!(boolean.contains(&"c3 + 1 = 4".to_string()), "{boolean:?}");
        assert!(scalar.contains(&"c3 + 1".to_string()), "{scalar:?}");
        assert!(scalar.contains(&"c3".to_string()), "{scalar:?}");
    }

    #[test]
    fn compares_the_rows_of_the_two_forms() {
        let mut all = AllCompRes::default();
        all.v_res.push(CompRes {
            res_str_0: "\n1 a \n2 b \n".into(),
            res_str_1: "\n2 b \n1 a \n".into(),
            ..CompRes::default()
        });
        all.v_res.push(CompRes {
            res_str_0: "\n1 a \n".into(),
            res_str_1: "\n1 a \n1 a \n".into(),
            ..CompRes::default()
        });
        Eet.compare_results(&mut all);
        assert_eq!(all.v_res[0].comp_res, OraCompRes::Pass);
        assert_eq!(all.v_res[1].comp_res, OraCompRes::Fail);
        assert_eq!(all.final_res, Some(OraCompRes::Fail));
    }
}
