//! Helpers that read and edit IR trees: the statement list under the root,
//! searches by node type, and tests on the position of a node. Port of
//! `ir_wrapper.cpp`. All functions are free functions over an [`IrTree`].
//!
//! A parsed query has this shape (see `stmtmulti` in the grammar):
//!
//! ```text
//! ParseToplevel
//!  Stmtmulti [|;|]                 <- the last one holds the empty trailing statement
//!   Stmtmulti [|;|]
//!    Stmtmulti                     <- the first one: left is the first Stmt
//!     Stmt -> CreateStmt
//!    Stmt -> InsertStmt
//!   Stmt (empty)
//! ```

use std::collections::VecDeque;
use std::sync::OnceLock;

use sqlright_ir::{IrId, IrTree, IrType, Op};

/// The name of a type without the `_N` suffix the grammar translator adds
/// to the parts of one rule: `kSimpleSelect_13` and `kSimpleSelect` compare
/// equal.
pub fn base_name(ty: IrType) -> &'static str {
    static BASE: OnceLock<Vec<&'static str>> = OnceLock::new();
    let table = BASE.get_or_init(|| {
        IrType::all()
            .iter()
            .map(|t| {
                let name = t.as_str();
                match name.find('_') {
                    Some(pos) => &name[..pos],
                    None => name,
                }
            })
            .collect()
    });
    table[ty as usize]
}

/// True when two types belong to the same grammar rule.
pub fn compare_ir_type(left: IrType, right: IrType) -> bool {
    base_name(left) == base_name(right)
}

// -- statement lists -------------------------------------------------------------
//
// The C++ assumed that the trailing empty statement is always the last link
// of the chain and lost the last statement after an append at the end. Here
// the statement list is the list of non-empty statements wherever they are,
// and an append at index N makes the new statement number N.

/// The whole `Stmtmulti` chain, first to last, including links that hold an
/// empty statement.
fn stmtmulti_chain_full(t: &IrTree, root: IrId) -> Vec<IrId> {
    let Some(mut cur) = t.left(root) else {
        return Vec::new();
    };
    if t.ty(cur) != IrType::Stmtmulti {
        return Vec::new();
    }
    let mut list = Vec::new();
    loop {
        if t.ty(cur) != IrType::Stmtmulti {
            break;
        }
        list.push(cur);
        if t.right(cur).is_none() {
            break;
        }
        match t.left(cur) {
            Some(l) => cur = l,
            None => break,
        }
    }
    list.reverse();
    list
}

/// The `Stmtmulti` links that hold a statement, first to last.
pub fn get_stmtmulti_ir_vec(t: &IrTree, root: IrId) -> Vec<IrId> {
    stmtmulti_chain_full(t, root)
        .into_iter()
        .filter(|sm| get_stmt_ir_from_stmtmulti(t, *sm).is_some())
        .collect()
}

/// The statement node (for example a `SelectStmt`) that a `Stmtmulti` holds.
/// `None` for the link that holds the empty trailing statement.
pub fn get_stmt_ir_from_stmtmulti(t: &IrTree, sm: IrId) -> Option<IrId> {
    if t.ty(sm) != IrType::Stmtmulti {
        return None;
    }
    if let Some(r) = t.right(sm) {
        return t.left(r);
    }
    let l = t.left(sm)?;
    if t.ty(l) == IrType::Stmt {
        return t.left(l);
    }
    if t.ty(l) == IrType::TransactionStmtLegacy {
        return Some(l);
    }
    None
}

/// The statement nodes of the query, in order.
pub fn get_stmt_ir_vec(t: &IrTree, root: IrId) -> Vec<IrId> {
    get_stmtmulti_ir_vec(t, root)
        .into_iter()
        .filter_map(|sm| get_stmt_ir_from_stmtmulti(t, sm))
        .collect()
}

pub fn get_stmt_num(t: &IrTree, root: IrId) -> usize {
    get_stmtmulti_ir_vec(t, root).len()
}

pub fn get_first_stmt_from_root(t: &IrTree, root: IrId) -> Option<IrId> {
    get_stmt_ir_vec(t, root).first().copied()
}

pub fn get_last_stmt_from_root(t: &IrTree, root: IrId) -> Option<IrId> {
    get_stmt_ir_vec(t, root).last().copied()
}

/// The position of `stmt` in the statement list, when it is one.
pub fn get_stmt_idx(t: &IrTree, root: IrId, stmt: IrId) -> Option<usize> {
    get_stmt_ir_vec(t, root).iter().position(|s| *s == stmt)
}

/// Insert statement node `stmt` (a `CreateStmt`, `SelectStmt`, ...) so that
/// it becomes statement number `idx`. `idx == len` appends after the last
/// statement. The node must be in `t` already.
pub fn append_stmt_at_idx(t: &mut IrTree, root: IrId, stmt: IrId, idx: usize) -> bool {
    let list = get_stmtmulti_ir_vec(t, root);
    if list.is_empty() || idx > list.len() {
        return false;
    }
    let wrapped = t.node(IrType::Stmt, Op::empty(), Some(stmt), None);
    if idx == 0 {
        // The first link holds its statement on the left. Move that statement
        // to the right and put a new first link on the left.
        let first = list[0];
        if t.right(first).is_some() {
            return false;
        }
        let old_stmt = t.left(first);
        let new_first = t.node(IrType::Stmtmulti, Op::empty(), Some(wrapped), None);
        t.update_right(first, old_stmt);
        t.update_left(first, Some(new_first));
        t.replace_op(first, Op::mid(";"));
        return true;
    }
    // Insert right after statement idx - 1.
    let insert_pos = list[idx - 1];
    let new_res = t.node(IrType::Stmtmulti, Op::mid(";"), None, Some(wrapped));
    if !t.swap_node(root, insert_pos, Some(new_res)) {
        return false;
    }
    t.update_left(new_res, Some(insert_pos));
    true
}

pub fn append_stmt_at_end(t: &mut IrTree, root: IrId, stmt: IrId) -> bool {
    let total = get_stmt_num(t, root);
    if total < 1 {
        return false;
    }
    append_stmt_at_idx(t, root, stmt, total)
}

/// Remove the statement at `idx`. Fails when it is the only statement.
pub fn remove_stmt_at_idx_and_free(t: &mut IrTree, root: IrId, idx: usize) -> bool {
    let list = get_stmtmulti_ir_vec(t, root);
    if idx >= list.len() || list.len() <= 1 {
        return false;
    }
    let rov = list[idx];
    let Some(parent) = t.parent(rov) else {
        return false;
    };
    if t.right(rov).is_some() {
        // The statement is on the right: splice the link out.
        let next = t.left(rov);
        if !t.swap_node(parent, rov, next) {
            return false;
        }
        t.update_left(rov, None);
        t.deep_drop(rov);
    } else {
        // The first link: its parent becomes the first link.
        let right = t.right(parent);
        t.update_left(parent, right);
        t.update_right(parent, None);
        t.replace_op(parent, Op::empty());
        t.deep_drop(rov);
    }
    true
}

pub fn remove_stmt_and_free(t: &mut IrTree, root: IrId, stmt: IrId) -> bool {
    match get_stmt_idx(t, root, stmt) {
        Some(idx) => remove_stmt_at_idx_and_free(t, root, idx),
        None => false,
    }
}

/// Replace a statement by another one at the same position.
pub fn replace_stmt_and_free(t: &mut IrTree, root: IrId, old_stmt: IrId, new_stmt: IrId) -> bool {
    let Some(idx) = get_stmt_idx(t, root, old_stmt) else {
        return false;
    };
    if !remove_stmt_at_idx_and_free(t, root, idx) {
        return false;
    }
    append_stmt_at_idx(t, root, new_stmt, idx)
}

// -- searches ----------------------------------------------------------------------

/// All nodes in the subtree of `stmt` (pre-order, `stmt` included).
pub fn preorder(t: &IrTree, stmt: IrId) -> Vec<IrId> {
    let mut out = Vec::new();
    t.preorder(stmt, &mut out);
    out
}

/// Nodes of type `ty` below `stmt`. With `ignore_type_suffix`, types of the
/// same rule match (`SimpleSelect_1` matches `SimpleSelect`). Unless
/// `ignore_is_subquery` is set, only nodes whose subquery position equals
/// `is_subquery` are returned.
pub fn get_ir_node_in_stmt_with_type_full(
    t: &IrTree,
    stmt: IrId,
    ty: IrType,
    is_subquery: bool,
    ignore_is_subquery: bool,
    ignore_type_suffix: bool,
) -> Vec<IrId> {
    let matching: Vec<IrId> = preorder(t, stmt)
        .into_iter()
        .filter(|id| {
            let nt = t.ty(*id);
            if ignore_type_suffix {
                compare_ir_type(nt, ty)
            } else {
                nt == ty
            }
        })
        .collect();
    if ignore_is_subquery {
        return matching;
    }
    matching
        .into_iter()
        .filter(|id| is_in_subquery(t, stmt, *id) == is_subquery)
        .collect()
}

/// Nodes of type `ty` (suffix ignored) below `stmt`, in or out of subqueries.
pub fn get_ir_node_in_stmt_with_type(
    t: &IrTree,
    stmt: IrId,
    ty: IrType,
    is_subquery: bool,
) -> Vec<IrId> {
    get_ir_node_in_stmt_with_type_full(t, stmt, ty, is_subquery, false, true)
}

/// The number of arguments of the function call whose name node is
/// `func_name`. It is the number of `FuncArgList` nodes along the left side of
/// the argument list. The result is `None` for a call this function does not
/// read (for example `count(*)` or a call with a VARIADIC argument).
pub fn func_arg_count(t: &IrTree, func_name: IrId) -> Option<usize> {
    let call = t.parent(func_name)?;
    match t.ty(call) {
        // `f()`: the name is the only child.
        IrType::FuncApplication if t.right(call).is_none() => Some(0),
        // `f(a, b)`: the name and the argument list are the two children.
        IrType::FuncApplication_1 => {
            let mut count = 0;
            let mut list = t.right(call);
            while let Some(node) = list {
                if t.ty(node) != IrType::FuncArgList {
                    break;
                }
                count += 1;
                list = t.left(node);
            }
            Some(count)
        }
        _ => None,
    }
}

/// True for `min(a, b)` and `max(a, b)` (the function name is `name`). With
/// two or more arguments they are scalar functions in tursopg, as in SQLite.
/// They return one row for each input row and are not aggregates.
pub fn is_scalar_min_max(t: &IrTree, func_name: IrId, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let base = lower.rsplit('.').next().unwrap_or(&lower).trim();
    matches!(base, "min" | "max") && func_arg_count(t, func_name).is_some_and(|n| n >= 2)
}

pub fn is_exist_ir_node_in_stmt_with_type(
    t: &IrTree,
    stmt: IrId,
    ty: IrType,
    is_subquery: bool,
) -> bool {
    !get_ir_node_in_stmt_with_type(t, stmt, ty, is_subquery).is_empty()
}

/// True when `node` is inside a subquery of `stmt`.
pub fn is_in_subquery(t: &IrTree, stmt: IrId, node: IrId) -> bool {
    let mut cur = Some(node);
    while let Some(c) = cur {
        if c == stmt {
            return false;
        }
        let ty = t.ty(c);
        if ty == IrType::Stmtmulti {
            return false;
        }
        if ty == IrType::SelectStmt
            && t.parent(c).is_some()
            && get_parent_type_str(t, c, 1) != "kStmt"
        {
            return true;
        }
        if ty == IrType::SelectWithParens {
            let p = get_parent_type_str(t, c, 1);
            if p != "kSelectWithParens" && p != "kSelectStmt" && p != "kSelectClause" {
                return true;
            }
        }
        cur = t.parent(c);
    }
    false
}

/// The base name of the type of the `depth`-th enclosing node that belongs
/// to a different grammar rule, or `kUnknown`.
pub fn get_parent_type_str(t: &IrTree, node: IrId, depth: i32) -> &'static str {
    match get_p_parent_with_a_type(t, node, depth) {
        Some(p) => base_name(t.ty(p)),
        None => "kUnknown",
    }
}

/// The `depth`-th enclosing node whose rule differs from the one below it.
/// Nodes of the same rule (`SimpleSelect_1`, `SimpleSelect_2`, ...) count as
/// one level.
pub fn get_p_parent_with_a_type(t: &IrTree, node: IrId, mut depth: i32) -> Option<IrId> {
    let mut prev_type = t.ty(node);
    let mut cur = node;
    while let Some(parent) = t.parent(cur) {
        let parent_type = t.ty(parent);
        if parent_type == prev_type
            || (parent_type != IrType::Unknown && !compare_ir_type(parent_type, prev_type))
        {
            prev_type = parent_type;
            depth -= 1;
            if depth <= 0 {
                return Some(parent);
            }
        }
        cur = parent;
    }
    None
}

fn is_exist_non_empty_clause(t: &IrTree, stmt: IrId, ty: IrType) -> bool {
    get_ir_node_in_stmt_with_type(t, stmt, ty, false)
        .into_iter()
        .any(|c| !t.is_empty(c))
}

pub fn is_exist_group_clause(t: &IrTree, stmt: IrId) -> bool {
    is_exist_non_empty_clause(t, stmt, IrType::GroupClause)
}

pub fn is_exist_having_clause(t: &IrTree, stmt: IrId) -> bool {
    is_exist_non_empty_clause(t, stmt, IrType::HavingClause)
}

pub fn is_exist_limit_clause(t: &IrTree, stmt: IrId) -> bool {
    is_exist_non_empty_clause(t, stmt, IrType::LimitClause)
}

fn is_exist_set_op(t: &IrTree, stmt: IrId, ty: IrType, keyword: &str) -> bool {
    get_ir_node_in_stmt_with_type_full(t, stmt, ty, false, false, false)
        .into_iter()
        .any(|n| t.middle(n) == keyword)
}

pub fn is_exist_union_select(t: &IrTree, stmt: IrId) -> bool {
    is_exist_set_op(t, stmt, IrType::SimpleSelect_13, "UNION")
}

pub fn is_exist_intersect_select(t: &IrTree, stmt: IrId) -> bool {
    is_exist_set_op(t, stmt, IrType::SimpleSelect_14, "INTERSECT")
}

pub fn is_exist_except_select(t: &IrTree, stmt: IrId) -> bool {
    is_exist_set_op(t, stmt, IrType::SimpleSelect_15, "EXCEPT")
}

pub fn is_exist_set_operator(t: &IrTree, stmt: IrId) -> bool {
    is_exist_union_select(t, stmt)
        || is_exist_intersect_select(t, stmt)
        || is_exist_except_select(t, stmt)
}

/// The last `target_el` of the first select target list (one node, or none).
pub fn get_target_el_in_select_target(t: &IrTree, stmt: IrId) -> Vec<IrId> {
    let lists = get_ir_node_in_stmt_with_type(t, stmt, IrType::TargetList, false);
    let mut res = Vec::new();
    if let Some(first) = lists.first() {
        if let Some(r) = t.right(*first) {
            res.push(r);
        } else if let Some(l) = t.left(*first) {
            res.push(l);
        }
    }
    res
}

pub fn get_num_target_el_in_select_clause(t: &IrTree, stmt: IrId) -> usize {
    get_target_el_in_select_target(t, stmt).len()
}

/// The type of the statement that holds `node`, or `Unknown`.
pub fn get_cur_stmt_type_from_sub_ir(t: &IrTree, node: IrId) -> IrType {
    let mut cur = node;
    while let Some(parent) = t.parent(cur) {
        let ty = t.ty(cur);
        if ty == IrType::Stmt {
            return t.left(cur).map_or(IrType::Unknown, |l| t.ty(l));
        }
        if ty == IrType::Stmtmulti {
            let child = match t.right(cur) {
                Some(r) => r,
                None => match t.left(cur) {
                    Some(l) => l,
                    None => return IrType::Unknown,
                },
            };
            if t.ty(child) == IrType::Stmt {
                return t.left(child).map_or(IrType::Unknown, |l| t.ty(l));
            }
            return t.ty(child);
        }
        cur = parent;
    }
    IrType::Unknown
}

/// True when `par` is `sub` or one of its ancestors.
pub fn is_ir_in(t: &IrTree, sub: IrId, par: IrId) -> bool {
    let mut cur = Some(sub);
    while let Some(c) = cur {
        if c == par {
            return true;
        }
        cur = t.parent(c);
    }
    false
}

/// True when `sub` or one of its ancestors has type `par_type`.
pub fn is_ir_in_type(t: &IrTree, sub: IrId, par_type: IrType) -> bool {
    let mut cur = Some(sub);
    while let Some(c) = cur {
        if t.ty(c) == par_type {
            return true;
        }
        cur = t.parent(c);
    }
    false
}

/// Attach `app` as the left or right child of `parent`. With `is_replace`
/// an existing child is dropped first.
pub fn append_components_at_ir(
    t: &mut IrTree,
    parent: IrId,
    app: IrId,
    is_left: bool,
    is_replace: bool,
) -> bool {
    let existing = if is_left {
        t.left(parent)
    } else {
        t.right(parent)
    };
    if let Some(old) = existing {
        if !is_replace {
            return false;
        }
        t.detach_node(parent, old);
        t.deep_drop(old);
    }
    if is_left {
        t.update_left(parent, Some(app));
    } else {
        t.update_right(parent, Some(app));
    }
    true
}

/// Detach and drop `node` from its parent.
pub fn remove_components_at_ir(t: &mut IrTree, node: IrId) -> bool {
    let Some(parent) = t.parent(node) else {
        return false;
    };
    t.detach_node(parent, node);
    t.deep_drop(node);
    true
}

/// All `Stmt` nodes of a tree, breadth first (the C++ `extract_statement`).
pub fn extract_statement(t: &IrTree, root: IrId) -> Vec<IrId> {
    let mut res = Vec::new();
    let mut bfs = VecDeque::from([root]);
    while let Some(node) = bfs.pop_front() {
        if t.ty(node) == IrType::Stmt {
            res.push(node);
        }
        if let Some(l) = t.left(node) {
            bfs.push_back(l);
        }
        if let Some(r) = t.right(node) {
            bfs.push_back(r);
        }
    }
    res
}

/// False when a statement of the tree has more than `limit` nodes.
pub fn check_node_num(t: &IrTree, root: IrId, limit: usize) -> bool {
    extract_statement(t, root)
        .into_iter()
        .all(|stmt| t.count_nodes(stmt) <= limit)
}

/// The name of the schema that `node` creates when it is the name part of
/// a `CREATE SCHEMA` statement, in lower case.
pub fn defined_schema_name(t: &IrTree, node: IrId) -> Option<String> {
    if !matches!(t.ty(node), IrType::CreateSchemaStmt | IrType::OptSchemaName) {
        return None;
    }
    let iden = t.left(node).filter(|&l| t.ty(l) == IrType::Identifier)?;
    Some(t.str_val(iden).to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlright_pg_parser::parse_query;

    #[test]
    fn statement_list_round_trip() {
        let mut q =
            parse_query("CREATE TABLE v0 (v1 INT); INSERT INTO v0 VALUES (1); SELECT v1 FROM v0;")
                .unwrap();
        let stmts = get_stmt_ir_vec(&q.tree, q.root);
        assert_eq!(stmts.len(), 3);
        assert_eq!(q.tree.ty(stmts[0]), IrType::CreateStmt);
        assert_eq!(q.tree.ty(stmts[1]), IrType::InsertStmt);
        assert_eq!(q.tree.ty(stmts[2]), IrType::SelectStmt);
        assert_eq!(get_stmt_idx(&q.tree, q.root, stmts[1]), Some(1));

        // append a statement at the end and at the start
        let extra = parse_query("DROP TABLE v0;").unwrap();
        let drop_stmt = get_first_stmt_from_root(&extra.tree, extra.root).unwrap();
        let imported = q.tree.import(&extra.tree, drop_stmt);
        assert!(append_stmt_at_end(&mut q.tree, q.root, imported));
        assert_eq!(get_stmt_num(&q.tree, q.root), 4);
        let s = q.tree.to_string(q.root);
        assert!(s.contains("DROP TABLE v0"), "{s}");

        let imported2 = q.tree.import(&extra.tree, drop_stmt);
        assert!(append_stmt_at_idx(&mut q.tree, q.root, imported2, 0));
        let stmts = get_stmt_ir_vec(&q.tree, q.root);
        assert_eq!(stmts.len(), 5);
        assert_eq!(q.tree.ty(stmts[0]), IrType::DropStmt);

        // remove the first and a middle statement
        assert!(remove_stmt_at_idx_and_free(&mut q.tree, q.root, 0));
        assert_eq!(get_stmt_num(&q.tree, q.root), 4);
        assert!(remove_stmt_at_idx_and_free(&mut q.tree, q.root, 1));
        let stmts = get_stmt_ir_vec(&q.tree, q.root);
        assert_eq!(stmts.len(), 3);
        assert_eq!(q.tree.ty(stmts[0]), IrType::CreateStmt);
        assert_eq!(q.tree.ty(stmts[1]), IrType::SelectStmt);
        assert_eq!(q.tree.ty(stmts[2]), IrType::DropStmt);
        // insert in the middle
        let imported3 = q.tree.import(&extra.tree, drop_stmt);
        assert!(append_stmt_at_idx(&mut q.tree, q.root, imported3, 1));
        let stmts = get_stmt_ir_vec(&q.tree, q.root);
        assert_eq!(stmts.len(), 4);
        assert_eq!(q.tree.ty(stmts[1]), IrType::DropStmt);
        assert_eq!(q.tree.ty(stmts[2]), IrType::SelectStmt);
        // removing the last statement keeps the trailing empty statement
        assert!(remove_stmt_at_idx_and_free(&mut q.tree, q.root, 3));
        assert_eq!(get_stmt_num(&q.tree, q.root), 3);
        // the whole thing still parses
        let s = q.tree.to_string(q.root);
        assert!(parse_query(&s).is_some(), "{s}");
    }

    #[test]
    fn subquery_detection() {
        let q = parse_query("SELECT v1 FROM v0 WHERE v1 IN (SELECT v2 FROM v3);").unwrap();
        let stmt = get_first_stmt_from_root(&q.tree, q.root).unwrap();
        let froms = get_ir_node_in_stmt_with_type(&q.tree, stmt, IrType::FromClause, false);
        assert_eq!(froms.len(), 1);
        let sub_froms = get_ir_node_in_stmt_with_type(&q.tree, stmt, IrType::FromClause, true);
        assert_eq!(sub_froms.len(), 1);
        assert!(!is_exist_set_operator(&q.tree, stmt));
        let q2 = parse_query("SELECT 1 UNION SELECT 2;").unwrap();
        let stmt2 = get_first_stmt_from_root(&q2.tree, q2.root).unwrap();
        assert!(is_exist_set_operator(&q2.tree, stmt2));
    }

    #[test]
    fn parent_type_chain() {
        let q = parse_query("SELECT COUNT(*) FROM v0 WHERE v1 = 1;").unwrap();
        let stmt = get_first_stmt_from_root(&q.tree, q.root).unwrap();
        let func_names = get_ir_node_in_stmt_with_type(&q.tree, stmt, IrType::FuncName, false);
        assert_eq!(func_names.len(), 1);
        let f = func_names[0];
        assert_eq!(get_parent_type_str(&q.tree, f, 1), "kFuncApplication");
        assert_eq!(get_parent_type_str(&q.tree, f, 2), "kFuncExpr");
        assert_eq!(get_parent_type_str(&q.tree, f, 3), "kCExpr");
        assert_eq!(get_parent_type_str(&q.tree, f, 4), "kAExpr");
        assert_eq!(get_parent_type_str(&q.tree, f, 5), "kTargetEl");
        assert_eq!(get_parent_type_str(&q.tree, f, 6), "kTargetList");
        assert_eq!(get_parent_type_str(&q.tree, f, 7), "kOptTargetList");
        assert_eq!(get_parent_type_str(&q.tree, f, 8), "kSimpleSelect");
    }
}
