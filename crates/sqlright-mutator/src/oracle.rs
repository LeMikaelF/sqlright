//! The oracle interface and the shared result types. Port of
//! `postgres_oracle.h` and `postgres_oracle.cpp`.
//!
//! An oracle decides which SELECT statements it can check, rewrites them
//! into equivalent forms, and compares the results of the forms.

use sqlright_ir::{IrId, IrTree, IrType};
use sqlright_pg_parser::parse_query;

use crate::ir_wrapper;
use crate::mutator::Mutator;

/// The verdict for one SELECT or for a whole test case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OraCompRes {
    Pass,
    Fail,
    /// The results could not be compared (an error, or no results).
    Error,
}

impl OraCompRes {
    pub fn as_i32(self) -> i32 {
        match self {
            OraCompRes::Pass => 1,
            OraCompRes::Fail => 0,
            OraCompRes::Error => -1,
        }
    }
}

/// The results of one oracle SELECT in its equivalent forms.
#[derive(Debug, Clone)]
pub struct CompRes {
    pub res_str_0: String,
    pub res_str_1: String,
    pub res_str_2: String,
    pub res_str_3: String,
    pub v_res_str: Vec<String>,
    pub res_int_0: i64,
    pub res_int_1: i64,
    pub res_int_2: i64,
    pub res_int_3: i64,
    pub v_res_int: Vec<i64>,
    pub comp_res: OraCompRes,
    pub explain_diff_id: Vec<i32>,
}

impl Default for CompRes {
    fn default() -> CompRes {
        CompRes {
            res_str_0: "EMPTY".to_string(),
            res_str_1: "EMPTY".to_string(),
            res_str_2: "EMPTY".to_string(),
            res_str_3: "EMPTY".to_string(),
            v_res_str: Vec::new(),
            res_int_0: -1,
            res_int_1: -1,
            res_int_2: -1,
            res_int_3: -1,
            v_res_int: Vec::new(),
            comp_res: OraCompRes::Error,
            explain_diff_id: Vec::new(),
        }
    }
}

/// The results of one test case.
#[derive(Debug, Clone, Default)]
pub struct AllCompRes {
    pub v_res: Vec<CompRes>,
    pub final_res: Option<OraCompRes>,
    pub cmd_str: String,
    pub v_cmd_str: Vec<String>,
    pub res_str: String,
    pub v_res_str: Vec<String>,
}

/// A DBMS oracle: which SELECTs it checks, how it rewrites them, and how it
/// compares the results.
pub trait Oracle {
    /// The name used on the command line and in reports, for example `NOREC`.
    fn oracle_type(&self) -> &'static str;

    /// A SELECT template the mutator starts from when the library has no
    /// usable SELECT yet.
    fn template_select_stmt(&self, mutator: &mut Mutator) -> String;

    /// How many times a test case runs. Oracles that compare runs with
    /// different schemas (INDEX) return more than one.
    fn mul_run_num(&self) -> usize {
        1
    }

    /// True when the oracle can check this SELECT. The oracle may edit the
    /// statement to make it fit (the C++ oracles do).
    fn is_oracle_select_stmt(&self, t: &mut IrTree, stmt: IrId) -> bool;

    /// True for a non-SELECT statement the oracle wants to transform.
    fn is_oracle_normal_stmt(&self, _t: &IrTree, _stmt: IrId) -> bool {
        false
    }

    /// Mark nodes that the mutator must not change.
    fn mark_all_valid_node(&self, _t: &mut IrTree, _nodes: &[IrId]) -> bool {
        true
    }

    /// Rewrite before names are filled in. Returns a new tree with the
    /// rewritten statement as root, or `None` for no rewrite.
    fn pre_fix_transform_select_stmt(&self, _mutator: &mut Mutator, _t: &mut IrTree, _stmt: IrId) -> Option<IrTree> {
        None
    }

    fn pre_fix_transform_normal_stmt(&self, _mutator: &mut Mutator, _t: &mut IrTree, _stmt: IrId) -> Option<IrTree> {
        None
    }

    /// Rewrite after names are filled in. Returns the equivalent forms, the
    /// original form first. An empty vector drops the statement.
    fn post_fix_transform_select_stmt(&self, mutator: &mut Mutator, t: &mut IrTree, stmt: IrId, multi_run_id: usize) -> Vec<IrTree>;

    fn post_fix_transform_normal_stmt(&self, _mutator: &mut Mutator, _t: &mut IrTree, _stmt: IrId, _multi_run_id: usize) -> Vec<IrTree> {
        Vec::new()
    }

    /// Compare the results and set `comp_res` of every entry and `final_res`.
    fn compare_results(&self, res: &mut AllCompRes);
}

/// The default test of the base class: every SELECT statement.
pub fn is_plain_select_stmt(t: &IrTree, stmt: IrId) -> bool {
    t.ty(stmt) == IrType::SelectStmt
}

/// True when the first statement of `sql` is one the oracle checks.
pub fn is_oracle_select_str(oracle: &dyn Oracle, sql: &str) -> bool {
    let Some(mut q) = parse_query(sql) else {
        return false;
    };
    let Some(stmt) = ir_wrapper::get_first_stmt_from_root(&q.tree, q.root) else {
        return false;
    };
    oracle.is_oracle_select_stmt(&mut q.tree, stmt)
}

pub fn count_oracle_select_stmts(oracle: &dyn Oracle, t: &mut IrTree, root: IrId) -> usize {
    let stmts = ir_wrapper::get_stmt_ir_vec(t, root);
    stmts.into_iter().filter(|s| oracle.is_oracle_select_stmt(t, *s)).count()
}

/// Remove every SELECT statement of the query.
pub fn remove_select_stmt_from_ir(t: &mut IrTree, root: IrId) {
    for stmt in ir_wrapper::get_stmt_ir_vec(t, root) {
        if t.ty(stmt) == IrType::SelectStmt {
            ir_wrapper::remove_stmt_and_free(t, root, stmt);
        }
    }
}

/// Remove every statement the oracle would check.
pub fn remove_oracle_select_stmt_from_ir(oracle: &dyn Oracle, t: &mut IrTree, root: IrId) {
    for stmt in ir_wrapper::get_stmt_ir_vec(t, root) {
        if oracle.is_oracle_select_stmt(t, stmt) {
            ir_wrapper::remove_stmt_and_free(t, root, stmt);
        }
    }
}

/// Produce one oracle-compatible SELECT: take a SELECT from the library or
/// the oracle template, mutate it while the oracle still accepts it, and
/// return it as the root of a new tree. The C++ `get_random_mutated_select_stmt`.
pub fn get_random_mutated_select_stmt(oracle: &dyn Oracle, mutator: &mut Mutator) -> Option<IrTree> {
    mutator.stats.total_rand_valid += 1;
    // The C++ loops until it succeeds. Give up after many tries instead.
    for _ in 0..200 {
        let (ori_valid_select, use_temp) = mutator.get_valid_str_from_lib(oracle);
        let Some(mut parsed) = parse_query(&ori_valid_select) else {
            continue;
        };
        if !ir_wrapper::check_node_num(&parsed.tree, parsed.root, 300) {
            continue;
        }
        let Some(cur_stmt) = ir_wrapper::get_first_stmt_from_root(&parsed.tree, parsed.root) else {
            continue;
        };
        if !oracle.is_oracle_select_stmt(&mut parsed.tree, cur_stmt) {
            continue;
        }
        // A library SELECT is returned as is two times out of three.
        if !use_temp && mutator.rng.rand_int(3) < 2 {
            return Some(parsed.tree.extract(cur_stmt));
        }
        let nodes = parsed.tree.all_nodes(parsed.root);
        oracle.mark_all_valid_node(&mut parsed.tree, &nodes);
        let ori_struct = mutator.extract_struct_copy(&parsed.tree, parsed.root);

        for _trial in 0..30 {
            mutator.stats.num_oracle_select_mutate += 1;
            // Pick a node that the oracle did not fix. Never the root.
            let mut chosen = None;
            for _ in 0..100 {
                let idx = mutator.rng.rand_int(nodes.len() - 1);
                let cand = nodes[idx];
                if parsed.tree.get(cand).is_node_struct_fixed {
                    continue;
                }
                chosen = Some(cand);
                break;
            }
            let Some(mutate_node) = chosen else {
                break;
            };
            let new_sub = match mutator.rng.rand_int(3) {
                0 => mutator.strategy_delete(&parsed.tree, mutate_node),
                1 => mutator.strategy_insert(&parsed.tree, mutate_node),
                _ => mutator.strategy_replace(&parsed.tree, mutate_node),
            };
            let Some(new_sub) = new_sub else {
                continue;
            };
            let new_node = parsed.tree.import(&new_sub, new_sub.root.expect("subtree root"));
            if !parsed.tree.swap_node(parsed.root, mutate_node, Some(new_node)) {
                continue;
            }
            let new_valid_select_str = parsed.tree.to_string(parsed.root);
            let new_struct = if new_valid_select_str != ori_valid_select {
                Some(mutator.extract_struct_copy(&parsed.tree, parsed.root))
            } else {
                None
            };
            // Put the original node back.
            parsed.tree.swap_node(parsed.root, new_node, Some(mutate_node));
            let Some(new_struct) = new_struct else {
                continue;
            };
            let Some(mut verified) = parse_query(&new_valid_select_str) else {
                continue;
            };
            let Some(verified_stmt) = ir_wrapper::get_first_stmt_from_root(&verified.tree, verified.root) else {
                continue;
            };
            if oracle.is_oracle_select_stmt(&mut verified.tree, verified_stmt) && new_struct != ori_struct {
                if use_temp {
                    mutator.stats.total_temp += 1;
                }
                mutator.stats.num_oracle_select_succeed += 1;
                return Some(verified.tree.extract(verified_stmt));
            }
        }
    }
    None
}
