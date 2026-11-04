//! Make a failing test case smaller while the oracle still fails:
//! remove oracle SELECT groups, then whole statements, then parts of
//! statements.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use sqlright_afl::{Engine, TestCase};
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::OraCompRes;
use sqlright_oracle::result::{split_runs, RUN_MARKER};
use sqlright_pg_parser::parse_query;

use crate::harness;
use crate::seeds::split_sql;

/// The statements of every run of a test case.
type Runs = Vec<Vec<String>>;

pub fn minimize(engine: &Engine, harness_bin: &Path, text: &str, max_runs: usize, timeout: Duration) -> Result<String> {
    let mut m = Minimizer { engine, harness_bin, timeout, runs_left: max_runs, runs_done: 0 };
    let mut runs: Runs = split_runs(text).iter().map(|r| statements_of(r)).collect();
    runs.retain(|r| !r.is_empty());
    if runs.is_empty() {
        bail!("the test case has no statement");
    }
    let before: usize = runs.iter().map(|r| r.len()).sum();
    if !m.still_fails(&runs)? {
        bail!("the oracle does not fail on this test case, nothing to minimize (run `sqlright replay` to see the verdict)");
    }
    m.remove_oracle_groups(&mut runs)?;
    m.remove_statements(&mut runs)?;
    m.shrink_statements(&mut runs)?;
    let after: usize = runs.iter().map(|r| r.len()).sum();
    eprintln!("{before} statements -> {after}, {} harness runs{}", m.runs_done, if m.runs_left == 0 { " (budget used up)" } else { "" });
    Ok(join_runs(&runs))
}

struct Minimizer<'a> {
    engine: &'a Engine,
    harness_bin: &'a Path,
    timeout: Duration,
    runs_left: usize,
    runs_done: usize,
}

impl Minimizer<'_> {
    /// Run the harness; true when the oracle still reports a failure.
    /// False, without a run, when the budget is used up.
    fn still_fails(&mut self, runs: &Runs) -> Result<bool> {
        if self.runs_left == 0 {
            return Ok(false);
        }
        self.runs_left -= 1;
        self.runs_done += 1;
        let text = join_runs(runs);
        let case = TestCase { marked_runs: split_runs(&text), unmarked_runs: Vec::new() };
        let output = harness::run(self.harness_bin, &text, self.timeout)?;
        let (verdict, _) = self.engine.compare(&case, &output);
        Ok(verdict == OraCompRes::Fail)
    }

    /// Remove whole oracle groups (a pair of result blocks for NOREC and
    /// TLP, one block for INDEX) while the case still fails.
    fn remove_oracle_groups(&mut self, runs: &mut Runs) -> Result<()> {
        let group_size = if self.engine.oracle.mul_run_num() > 1 { 1 } else { 2 };
        loop {
            let blocks = block_numbers(&runs[0]);
            if blocks.len() <= group_size {
                return Ok(());
            }
            let mut changed = false;
            for group in blocks.chunks(group_size).rev() {
                let mut candidate: Runs = runs.iter().map(|r| remove_blocks(r, group)).collect();
                renumber_blocks(&mut candidate);
                if self.still_fails(&candidate)? {
                    *runs = candidate;
                    changed = true;
                    break;
                }
                if self.runs_left == 0 {
                    return Ok(());
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    /// Remove statements outside the result blocks, one at a time.
    fn remove_statements(&mut self, runs: &mut Runs) -> Result<()> {
        loop {
            let mut changed = false;
            let candidates = free_statement_indexes(&runs[0]);
            for idx in candidates.into_iter().rev() {
                let text = runs[0][idx].clone();
                let candidate: Runs = runs.iter().map(|r| without_first(r, &text)).collect();
                if self.still_fails(&candidate)? {
                    *runs = candidate;
                    changed = true;
                } else if self.runs_left == 0 {
                    return Ok(());
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    /// Delete parts of the statements outside the result blocks: every
    /// IR node whose removal still parses is tried, largest first.
    fn shrink_statements(&mut self, runs: &mut Runs) -> Result<()> {
        let mut idx = 0;
        while idx < runs[0].len() {
            if !free_statement_indexes(&runs[0]).contains(&idx) {
                idx += 1;
                continue;
            }
            loop {
                let old = runs[0][idx].clone();
                let Some(new_text) = self.find_shrink(runs, &old)? else {
                    break;
                };
                for r in runs.iter_mut() {
                    if let Some(pos) = r.iter().position(|s| *s == old) {
                        r[pos] = new_text.clone();
                    }
                }
            }
            if self.runs_left == 0 {
                return Ok(());
            }
            idx += 1;
        }
        Ok(())
    }

    /// The first smaller text of `stmt` for which the case still fails.
    fn find_shrink(&mut self, runs: &Runs, stmt: &str) -> Result<Option<String>> {
        let Some(q) = parse_query(stmt) else {
            return Ok(None);
        };
        let mut nodes: Vec<_> = q.tree.all_nodes(q.root).into_iter().filter(|&n| n != q.root).collect();
        nodes.sort_by_key(|&n| std::cmp::Reverse(q.tree.count_nodes(n)));
        for node in nodes {
            let Some(mut t) = parse_query(stmt) else {
                return Ok(None);
            };
            if !t.tree.swap_node(t.root, node, None) {
                continue;
            }
            let new_text = single_statement_text(&t.tree, t.root);
            if new_text == stmt || new_text.trim().is_empty() || parse_query(&new_text).is_none() {
                continue;
            }
            let candidate: Runs = runs
                .iter()
                .map(|r| r.iter().map(|s| if s == stmt { new_text.clone() } else { s.clone() }).collect())
                .collect();
            if self.still_fails(&candidate)? {
                return Ok(Some(new_text));
            }
            if self.runs_left == 0 {
                return Ok(None);
            }
        }
        Ok(None)
    }
}

/// The text of a parsed single statement, without the semicolon.
fn single_statement_text(tree: &sqlright_ir::IrTree, root: sqlright_ir::IrId) -> String {
    tree.to_string(root).trim().trim_end_matches(';').trim().to_string()
}

/// The statements of one run, without semicolons.
fn statements_of(run_text: &str) -> Vec<String> {
    if let Some(q) = parse_query(run_text) {
        let stmts = ir_wrapper::get_stmt_ir_vec(&q.tree, q.root);
        if !stmts.is_empty() {
            return stmts.iter().map(|&s| q.tree.to_string(s).trim().trim_end_matches(';').trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
    }
    split_sql(run_text)
}

fn join_runs(runs: &Runs) -> String {
    let mut out = String::new();
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            out.push_str(&format!("{RUN_MARKER} {i}\n"));
        }
        for s in run {
            out.push_str(s);
            out.push_str(";\n");
        }
    }
    out
}

/// `SELECT 'BEGIN VERI n'` gives `Some((true, n))`, `SELECT 'END VERI n'` gives `Some((false, n))`.
fn marker(stmt: &str) -> Option<(bool, usize)> {
    let upper = stmt.to_ascii_uppercase();
    let (begin, rest) = if let Some(p) = upper.find("'BEGIN VERI ") {
        (true, &upper[p + "'BEGIN VERI ".len()..])
    } else if let Some(p) = upper.find("'END VERI ") {
        (false, &upper[p + "'END VERI ".len()..])
    } else {
        return None;
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok().map(|n| (begin, n))
}

/// The block numbers of a run, in order.
fn block_numbers(run: &[String]) -> Vec<usize> {
    run.iter().filter_map(|s| marker(s)).filter(|(begin, _)| *begin).map(|(_, n)| n).collect()
}

/// The run without the blocks with these numbers (markers included).
fn remove_blocks(run: &[String], numbers: &[usize]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skipping = false;
    for s in run {
        match marker(s) {
            Some((true, n)) if numbers.contains(&n) => skipping = true,
            Some((false, n)) if numbers.contains(&n) => {
                skipping = false;
                continue;
            }
            _ => {}
        }
        if !skipping {
            out.push(s.clone());
        }
    }
    out
}

/// Number the remaining blocks 0, 1, 2, ... in every run.
fn renumber_blocks(runs: &mut Runs) {
    let mut old_numbers = block_numbers(&runs[0]);
    old_numbers.sort_unstable();
    old_numbers.dedup();
    for run in runs.iter_mut() {
        for s in run.iter_mut() {
            if let Some((begin, n)) = marker(s) {
                if let Some(new) = old_numbers.iter().position(|&o| o == n) {
                    *s = format!("SELECT '{} VERI {new}'", if begin { "BEGIN" } else { "END" });
                }
            }
        }
    }
}

/// The indexes of the statements that are not in a result block.
fn free_statement_indexes(run: &[String]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut in_block = false;
    for (i, s) in run.iter().enumerate() {
        match marker(s) {
            Some((true, _)) => in_block = true,
            Some((false, _)) => in_block = false,
            None if !in_block => out.push(i),
            None => {}
        }
    }
    out
}

/// The run without the first statement equal to `text`.
fn without_first(run: &[String], text: &str) -> Vec<String> {
    let mut out = run.to_vec();
    if let Some(pos) = out.iter().position(|s| s == text) {
        out.remove(pos);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_renumbers_result_blocks() {
        let run: Vec<String> = ["CREATE TABLE v0 (c1 INT)", "SELECT 'BEGIN VERI 0'", "SELECT 1", "SELECT 'END VERI 0'", "SELECT 'BEGIN VERI 1'", "SELECT 2", "SELECT 'END VERI 1'", "SELECT 'BEGIN VERI 2'", "SELECT 3", "SELECT 'END VERI 2'", "SELECT 'BEGIN VERI 3'", "SELECT 4", "SELECT 'END VERI 3'"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(block_numbers(&run), vec![0, 1, 2, 3]);
        assert_eq!(free_statement_indexes(&run), vec![0]);
        let mut runs = vec![remove_blocks(&run, &[0, 1])];
        renumber_blocks(&mut runs);
        assert_eq!(runs[0], vec!["CREATE TABLE v0 (c1 INT)", "SELECT 'BEGIN VERI 0'", "SELECT 3", "SELECT 'END VERI 0'", "SELECT 'BEGIN VERI 1'", "SELECT 4", "SELECT 'END VERI 1'"]);
    }

    #[test]
    fn splits_a_run_into_statements() {
        let stmts = statements_of("CREATE TABLE v0 ( c1 INT ); \nSELECT 'BEGIN VERI 0'; \nSELECT count ( * ) FROM v0 WHERE c1 = 1; \n");
        assert_eq!(stmts, vec!["CREATE TABLE v0 ( c1 INT )", "SELECT 'BEGIN VERI 0'", "SELECT count ( * ) FROM v0 WHERE c1 = 1"]);
    }
}
