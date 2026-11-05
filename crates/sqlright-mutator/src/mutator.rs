//! The mutator: libraries of seen IR nodes, the three mutation strategies,
//! and the pipeline from a mutated tree to query strings. Port of
//! `mutate.cpp` (the instantiation step is in `fix.rs`).
//!
//! Library design (from the SQLRight paper): the library does not keep IR
//! trees. It keeps the query string and the pre-order index of the node in
//! that query. To fetch a node, the mutator parses the string again and
//! takes the node at that index.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use sqlright_ir::util::{ensure_semicolon_at_query_end, hash_str, string_splitter, trimmed};
use sqlright_ir::{DataFlag, DataType, IrId, IrTree, IrType, IrValue, Op, RelOptionType};
use sqlright_pg_parser::{parse_query, ParsedQuery};

use crate::fix::FixContext;
use crate::ir_wrapper;
use crate::oracle::Oracle;
use crate::rng::Rng;

/// A node starts to be skipped by the mutator after this many mutations.
const LUCKY_NUMBER: usize = 500;

/// How a statement takes part in the oracle check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtType {
    NotOracle,
    OracleSelect,
    OracleNormal,
}

/// A library entry: the query the node came from and the node's position in
/// the pre-order list of that query.
#[derive(Debug, Clone)]
pub struct LibEntry {
    pub query: Rc<str>,
    pub node_id: usize,
}

/// Counters shown in the fuzzer status screen.
#[derive(Debug, Clone, Default)]
pub struct MutatorStats {
    pub total_rand_valid: u64,
    pub total_temp: u64,
    pub num_oracle_select_mutate: u64,
    pub num_oracle_select_succeed: u64,
    pub total_mutate_num: u64,
    pub total_mutate_failed: u64,
}

/// The argument type of a reloption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefArgType {
    Boolean,
    Integer,
    FloatingPoint,
    Str,
    OnOffAuto,
}

pub struct Mutator {
    pub rng: Rng,
    pub stats: MutatorStats,

    /// Nodes by type: where to find a node of each type.
    pub real_ir_set: HashMap<IrType, Vec<LibEntry>>,
    /// Parents by the type of their left child; the entry points at the parent.
    pub left_lib_set: HashMap<IrType, Vec<LibEntry>>,
    /// Parents by the type of their right child; the entry points at the parent.
    pub right_lib_set: HashMap<IrType, Vec<LibEntry>>,
    /// Hashes of the text of library nodes, by type, to skip duplicates.
    pub ir_library_2d_hash: HashMap<IrType, HashSet<u64>>,
    /// SELECT statements the oracle accepts, as text.
    pub all_valid_pstr_vec: Vec<Rc<str>>,
    norec_hash: HashSet<u64>,
    stripped_string_hash: HashSet<u64>,
    global_hash: HashSet<u64>,

    pub common_string_library: Vec<String>,
    pub value_library: Vec<u64>,
    pub string_types: HashSet<IrType>,
    pub int_types: HashSet<IrType>,
    pub float_types: HashSet<IrType>,
    pub not_mutatable_types: HashSet<IrType>,
    pub split_substmt_types: HashSet<IrType>,

    pub m_reloption: HashMap<IrType, Vec<(String, DefArgType)>>,
    pub v_aggregate_func: Vec<String>,
    pub v_sys_column_name: Vec<String>,
    pub v_sys_catalogs_name: Vec<String>,

    /// The names and literals seen in the current test case.
    pub ctx: FixContext,
    id_counter: u64,
}

impl Mutator {
    pub fn new(seed: u64) -> Mutator {
        let mut m = Mutator {
            rng: Rng::seeded(seed),
            stats: MutatorStats::default(),
            real_ir_set: HashMap::new(),
            left_lib_set: HashMap::new(),
            right_lib_set: HashMap::new(),
            ir_library_2d_hash: HashMap::new(),
            all_valid_pstr_vec: Vec::new(),
            norec_hash: HashSet::new(),
            stripped_string_hash: HashSet::new(),
            global_hash: HashSet::new(),
            common_string_library: Vec::new(),
            value_library: Vec::new(),
            string_types: HashSet::new(),
            int_types: HashSet::new(),
            float_types: HashSet::new(),
            not_mutatable_types: HashSet::new(),
            split_substmt_types: HashSet::new(),
            m_reloption: HashMap::new(),
            v_aggregate_func: Vec::new(),
            v_sys_column_name: Vec::new(),
            v_sys_catalogs_name: Vec::new(),
            ctx: FixContext::default(),
            id_counter: 0,
        };
        m.init_library();
        m
    }

    // -- names ---------------------------------------------------------------------

    pub fn reset_id_counter(&mut self) {
        self.id_counter = 0;
    }

    fn next_id(&mut self) -> u64 {
        let v = self.id_counter;
        self.id_counter += 1;
        v
    }

    pub fn gen_id_name(&mut self) -> String {
        format!("v{}", self.next_id())
    }

    pub fn gen_column_name(&mut self) -> String {
        format!("c{}", self.next_id())
    }

    pub fn gen_index_name(&mut self) -> String {
        format!("i{}", self.next_id())
    }

    pub fn gen_alias_name(&mut self) -> String {
        format!("a{}", self.next_id())
    }

    pub fn gen_statistic_name(&mut self) -> String {
        format!("s{}", self.next_id())
    }

    pub fn gen_sequence_name(&mut self) -> String {
        format!("seq{}", self.next_id())
    }

    pub fn gen_view_name(&mut self) -> String {
        format!("view{}", self.next_id())
    }

    // -- fixed data ------------------------------------------------------------------

    /// The fixed tables of the C++ `init_library`.
    fn init_library(&mut self) {
        self.value_library = vec![
            0,
            i64::MAX as u64,
            u64::MAX,
            8,
            i8::MIN as i64 as u64,
            i8::MAX as u64,
            u8::MAX as u64,
            i8::MIN as i64 as u64,
            i8::MAX as u64,
            16,
            i16::MIN as i64 as u64,
            i32::MIN as i64 as u64,
            i32::MAX as u64,
            i8::MIN as i64 as u64,
            i8::MIN as i64 as u64,
            u32::MAX as u64,
            f32::MAX as u64,
            f64::MAX as u64,
            f64::MAX as u64,
            f32::MIN_POSITIVE as u64,
            f64::MIN_POSITIVE as u64,
            f64::MIN_POSITIVE as u64,
        ];
        self.float_types.insert(IrType::FloatLiteral);
        self.int_types.insert(IrType::IntLiteral);
        self.string_types.insert(IrType::StringLiteral);
        self.split_substmt_types.insert(IrType::SelectNoParens);
        for t in [
            IrType::ParseToplevel,
            IrType::Stmtlist,
            IrType::Stmt,
            IrType::CreateStmt,
            IrType::DropStmt,
            IrType::SelectStmt,
            IrType::UpdateStmt,
            IrType::InsertStmt,
        ] {
            self.not_mutatable_types.insert(t);
        }
        for s in [
            "HELLO", "WORLD", "test", "files", "music", "score", "green", "red", "right", "left",
            "plot", "cov", "bug", "sample",
        ] {
            self.common_string_library.push(s.to_string());
        }
        let storage: Vec<(String, DefArgType)> = [
            ("fillfactor", DefArgType::Integer),
            ("toast_tuple_target", DefArgType::Integer),
            ("parallel_workers", DefArgType::Integer),
            ("autovacuum_enabled", DefArgType::Boolean),
            ("vacuum_index_cleanup", DefArgType::OnOffAuto),
            ("vacuum_truncate", DefArgType::Boolean),
            ("autovacuum_vacuum_threshold", DefArgType::Integer),
            ("autovacuum_vacuum_scale_factor", DefArgType::FloatingPoint),
            ("autovacuum_vacuum_insert_threshold", DefArgType::Integer),
            (
                "autovacuum_vacuum_insert_scale_factor",
                DefArgType::FloatingPoint,
            ),
            ("autovacuum_analyze_scale_factor", DefArgType::FloatingPoint),
            ("autovacuum_vacuum_cost_delay", DefArgType::Integer),
            ("autovacuum_vacuum_cost_limit", DefArgType::Integer),
            ("autovacuum_freeze_min_age", DefArgType::Integer),
            ("autovacuum_freeze_max_age", DefArgType::Integer),
            ("autovacuum_multixact_freeze_min_age", DefArgType::Integer),
            ("autovacuum_multixact_freeze_max_age", DefArgType::Integer),
            ("autovacuum_multixact_freeze_table_age", DefArgType::Integer),
            ("log_autovacuum_min_duration", DefArgType::Integer),
            ("user_catalog_table", DefArgType::Boolean),
        ]
        .into_iter()
        .map(|(n, t)| (n.to_string(), t))
        .collect();
        self.m_reloption.insert(IrType::CreateStmt, storage);
        self.v_aggregate_func = ["SUM", "COUNT", "MAX", "MIN", "AVG"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        self.v_sys_column_name = ["oid", "tableoid", "xmin", "cmin", "xmax", "cmax", "ctid"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        self.v_sys_catalogs_name = SYS_CATALOG_NAMES.iter().map(|s| s.to_string()).collect();
    }

    // -- library ----------------------------------------------------------------------

    /// Add every statement of a seed file to the library. Each line is one
    /// statement. The structure of the statement is kept, the names are
    /// replaced by placeholders first (`extract_struct`).
    pub fn init_from_file(
        &mut self,
        oracle: &dyn Oracle,
        path: &std::path::Path,
    ) -> std::io::Result<usize> {
        let text = std::fs::read_to_string(path)?;
        let mut added = 0;
        for line in text.lines() {
            if self.init_from_line(oracle, line) {
                added += 1;
            }
        }
        Ok(added)
    }

    /// Add one seed statement to the library. Returns false when it does not parse.
    pub fn init_from_line(&mut self, oracle: &dyn Oracle, line: &str) -> bool {
        let Some(mut q) = parse_query(line) else {
            return false;
        };
        let strip_sql = self.extract_struct(&mut q.tree, q.root);
        let Some(q2) = parse_query(&strip_sql) else {
            return false;
        };
        let text = q2.tree.to_string(q2.root);
        self.add_all_to_library(oracle, &text);
        true
    }

    /// Split a query into statements and add each one to the library. SELECTs
    /// the oracle accepts go to the SELECT library as well.
    pub fn add_all_to_library(&mut self, oracle: &dyn Oracle, whole_query_str: &str) {
        if whole_query_str
            .chars()
            .all(|c| c.is_whitespace() || c == '\0')
        {
            return;
        }
        for piece in string_splitter(whole_query_str, ';') {
            let mut current = trimmed(&piece);
            if current.is_empty() {
                continue;
            }
            current.push(';');
            if is_result_marker(&current) {
                continue;
            }
            let Some(mut q) = parse_query(&current) else {
                continue;
            };
            let stmts = ir_wrapper::get_stmt_ir_vec(&q.tree, q.root);
            let Some(stmt) = stmts.first().copied() else {
                return;
            };
            if oracle.is_oracle_select_stmt(&mut q.tree, stmt) {
                self.add_to_valid_lib(&q, &current);
            } else {
                self.add_to_library(&q, &current);
            }
        }
    }

    fn add_to_valid_lib(&mut self, q: &ParsedQuery, select: &str) {
        let h = hash_str(select);
        if !self.norec_hash.insert(h) {
            return;
        }
        let text: Rc<str> = Rc::from(select);
        self.all_valid_pstr_vec.push(text.clone());
        self.add_to_library_core(q, q.root, &text);
    }

    fn add_to_library(&mut self, q: &ParsedQuery, query: &str) {
        if query.is_empty() {
            return;
        }
        let p_type = q.tree.ty(q.root);
        let h = hash_str(query);
        if !self.ir_library_2d_hash.entry(p_type).or_default().insert(h) {
            return;
        }
        let text: Rc<str> = Rc::from(query);
        self.add_to_library_core(q, q.root, &text);
    }

    fn add_to_library_core(&mut self, q: &ParsedQuery, node: IrId, text: &Rc<str>) {
        let p_type = q.tree.ty(node);
        if p_type != IrType::ParseToplevel {
            let h = hash_str(&q.tree.to_string(node));
            if !self.ir_library_2d_hash.entry(p_type).or_default().insert(h) {
                return;
            }
        }
        let node_id = q.tree.get(node).uniq_id_in_tree;
        if node_id < 0 {
            return;
        }
        let entry = LibEntry {
            query: text.clone(),
            node_id: node_id as usize,
        };
        self.real_ir_set
            .entry(p_type)
            .or_default()
            .push(entry.clone());
        if let (Some(l), Some(r)) = (q.tree.left(node), q.tree.right(node)) {
            self.left_lib_set
                .entry(q.tree.ty(l))
                .or_default()
                .push(entry.clone());
            self.right_lib_set
                .entry(q.tree.ty(r))
                .or_default()
                .push(entry);
        }
        if let Some(l) = q.tree.left(node) {
            self.add_to_library_core(q, l, text);
        }
        if let Some(r) = q.tree.right(node) {
            self.add_to_library_core(q, r, text);
        }
    }

    /// True when this (stripped) query was seen before. Remembers it.
    pub fn is_stripped_str_in_lib(&mut self, stripped: &str) -> bool {
        !self.stripped_string_hash.insert(hash_str(stripped))
    }

    pub fn get_valid_collection_size(&self) -> usize {
        self.all_valid_pstr_vec.len()
    }

    pub fn get_ir_library_2d_hash_kstatement_size(&self) -> usize {
        self.ir_library_2d_hash
            .get(&IrType::Stmt)
            .map_or(0, |s| s.len())
    }

    /// Parse a library entry and return the node it points at, when the
    /// type matches `want`.
    fn fetch_entry(&mut self, entry: &LibEntry) -> Option<(ParsedQuery, IrId)> {
        let q = parse_query(&entry.query)?;
        let node = *q.nodes.get(entry.node_id)?;
        Some((q, node))
    }

    /// A random library node of type `ty`, as a new tree. When the library
    /// has none, an empty string literal (the C++ returns that too).
    pub fn get_from_library_with_type(&mut self, ty: IrType) -> IrTree {
        if let Some(entries) = self.real_ir_set.get(&ty) {
            if !entries.is_empty() {
                let entry = entries[self.rng.rand_int(entries.len())].clone();
                if let Some((q, node)) = self.fetch_entry(&entry) {
                    if q.tree.ty(node) == ty {
                        return q.tree.extract(node);
                    }
                }
            }
        }
        let mut t = IrTree::new();
        let r = t.leaf(IrType::StringLiteral, "");
        t.root = Some(r);
        t
    }

    /// A random library node of type `ty`, or `None` when the library has none.
    pub fn get_ir_with_type(&mut self, ty: IrType) -> Option<IrTree> {
        let has = self.real_ir_set.get(&ty).is_some_and(|v| !v.is_empty());
        if !has {
            return None;
        }
        let t = self.get_from_library_with_type(ty);
        let root = t.root?;
        if t.ty(root) != ty {
            return None;
        }
        Some(t)
    }

    /// A right sibling for a node of type `ty`: the right child of a library
    /// parent whose left child has that type.
    pub fn get_from_library_with_left_type(&mut self, ty: IrType) -> Option<IrTree> {
        let entries = self.left_lib_set.get(&ty)?;
        if entries.is_empty() {
            return None;
        }
        let entry = entries[self.rng.rand_int(entries.len())].clone();
        let (q, parent) = self.fetch_entry(&entry)?;
        let l = q.tree.left(parent)?;
        if q.tree.ty(l) != ty {
            return None;
        }
        let r = q.tree.right(parent)?;
        Some(q.tree.extract(r))
    }

    /// A left sibling for a node of type `ty`.
    pub fn get_from_library_with_right_type(&mut self, ty: IrType) -> Option<IrTree> {
        let entries = self.right_lib_set.get(&ty)?;
        if entries.is_empty() {
            return None;
        }
        let entry = entries[self.rng.rand_int(entries.len())].clone();
        let (q, parent) = self.fetch_entry(&entry)?;
        let r = q.tree.right(parent)?;
        if q.tree.ty(r) != ty {
            return None;
        }
        let l = q.tree.left(parent)?;
        Some(q.tree.extract(l))
    }

    /// A SELECT to start from: half of the time one from the library, else
    /// the oracle template. Returns the text and whether it is the template.
    pub fn get_valid_str_from_lib(&mut self, oracle: &dyn Oracle) -> (String, bool) {
        loop {
            if !self.all_valid_pstr_vec.is_empty() && self.rng.rand_int(2) < 1 {
                let s = self.all_valid_pstr_vec[self.rng.rand_int(self.all_valid_pstr_vec.len())]
                    .to_string();
                if s.is_empty() || !crate::oracle::is_oracle_select_str(oracle, &s) {
                    continue;
                }
                return (trimmed(&s), false);
            }
            let s = oracle.template_select_stmt(self);
            return (trimmed(&s), true);
        }
    }

    pub fn get_a_string(&mut self) -> String {
        if self.common_string_library.is_empty() {
            "hello".to_string()
        } else {
            self.common_string_library[self.rng.rand_int(self.common_string_library.len())].clone()
        }
    }

    // -- structure extraction ---------------------------------------------------------

    /// Replace names and literals by placeholders, in place, and return the
    /// text. Keeps function names, `FixLater` nodes and literal nodes.
    pub fn extract_struct(&self, t: &mut IrTree, root: IrId) -> String {
        self.extract_struct_core(t, root);
        trimmed(&t.to_string(root))
    }

    /// Like [`Mutator::extract_struct`] on a copy of the tree.
    pub fn extract_struct_copy(&self, t: &IrTree, root: IrId) -> String {
        let mut copy = t.extract(root);
        let r = copy.root.expect("root");
        self.extract_struct(&mut copy, r)
    }

    fn extract_struct_core(&self, t: &mut IrTree, id: IrId) {
        let n = t.get(id);
        if n.data_flag == DataFlag::NoModi
            || n.data_type == DataType::FunctionName
            || n.ty == IrType::FuncName
            || n.data_type == DataType::FixLater
            || n.data_type == DataType::Literal
        {
            return;
        }
        let (left, right, ty, data_type) = (n.left, n.right, n.ty, n.data_type);
        if let Some(l) = left {
            self.extract_struct_core(t, l);
        }
        if let Some(r) = right {
            self.extract_struct_core(t, r);
        }
        match ty {
            IrType::IntLiteral => {
                let n = t.get_mut(id);
                n.value = IrValue::Int(0);
                n.str_val = "0".to_string();
                return;
            }
            IrType::FloatLiteral => {
                let n = t.get_mut(id);
                n.value = IrValue::Float(0.0);
                n.str_val = "0.0".to_string();
                return;
            }
            IrType::BoolLiteral => {
                let n = t.get_mut(id);
                n.value = IrValue::Bool(true);
                n.str_val = "true".to_string();
                return;
            }
            _ => {}
        }
        if left.is_some() || right.is_some() {
            return;
        }
        if data_type != DataType::Whatever {
            t.set_str_val(id, "x");
            return;
        }
        if self.string_types.contains(&ty) {
            t.set_str_val(id, "'x'");
        } else if self.int_types.contains(&ty) {
            t.get_mut(id).value = IrValue::Int(1);
        } else if self.float_types.contains(&ty) {
            t.get_mut(id).value = IrValue::Float(1.0);
        }
    }

    // -- mutation strategies -------------------------------------------------------

    fn lucky_enough_to_be_mutated(&mut self, mutated_times: u32) -> bool {
        self.rng.rand_int(mutated_times as usize + 1) < LUCKY_NUMBER
    }

    /// Copy the node and drop its left child, its right child, or both.
    pub fn strategy_delete(&mut self, t: &IrTree, cur: IrId) -> Option<IrTree> {
        let mut res = t.extract(cur);
        let root = res.root?;
        let choice = self.rng.rand_int(3);
        if choice == 0 || choice == 2 {
            if let Some(l) = res.left(root) {
                res.update_left(root, None);
                res.deep_drop(l);
            }
        }
        if choice == 1 || choice == 2 {
            if let Some(r) = res.right(root) {
                res.update_right(root, None);
                res.deep_drop(r);
            }
        }
        Some(res)
    }

    /// Give the node a missing child from the library, or swap the node for
    /// a library node of the same type.
    pub fn strategy_insert(&mut self, t: &IrTree, cur: IrId) -> Option<IrTree> {
        let (left, right) = (t.left(cur), t.right(cur));
        if right.is_none() && left.is_some() {
            let left_type = t.ty(left.unwrap());
            if let Some(new_right) = self.get_from_library_with_left_type(left_type) {
                let mut res = t.extract(cur);
                let root = res.root?;
                let nr = res.import(&new_right, new_right.root?);
                res.update_right(root, Some(nr));
                return Some(res);
            }
        } else if right.is_some() && left.is_none() {
            let right_type = t.ty(right.unwrap());
            if let Some(new_left) = self.get_from_library_with_right_type(right_type) {
                let mut res = t.extract(cur);
                let root = res.root?;
                let nl = res.import(&new_left, new_left.root?);
                res.update_left(root, Some(nl));
                return Some(res);
            }
        }
        Some(self.get_from_library_with_type(t.ty(cur)))
    }

    /// Replace the left child, the right child, or both by library nodes of
    /// the same types. The role (data type) of the old child is kept. This
    /// is the SQLite variant of the C++ strategy; the PostgreSQL variant
    /// replaced children by an empty string literal, which was a bug.
    pub fn strategy_replace(&mut self, t: &IrTree, cur: IrId) -> Option<IrTree> {
        let choice = self.rng.rand_int(3);
        let do_left = choice == 0 || choice == 2;
        let do_right = choice == 1 || choice == 2;
        if (do_left && t.left(cur).is_none()) || (do_right && t.right(cur).is_none()) {
            return None;
        }
        let mut res = t.extract(cur);
        let root = res.root?;
        if do_left {
            let old = res.left(root)?;
            let new_sub = self.get_ir_with_type(res.ty(old))?;
            let nid = res.import(&new_sub, new_sub.root?);
            let dt = res.data_type(old);
            res.set_data_type(nid, dt);
            res.update_left(root, Some(nid));
            res.deep_drop(old);
        }
        if do_right {
            let old = res.right(root)?;
            let new_sub = self.get_ir_with_type(res.ty(old))?;
            let nid = res.import(&new_sub, new_sub.root?);
            let dt = res.data_type(old);
            res.set_data_type(nid, dt);
            res.update_right(root, Some(nid));
            res.deep_drop(old);
        }
        Some(res)
    }

    /// Apply the three strategies to one node. Returns the new subtrees.
    pub fn mutate(&mut self, t: &mut IrTree, input: IrId) -> Vec<IrTree> {
        let mut res = Vec::new();
        let times = t.get(input).mutated_times;
        if !self.lucky_enough_to_be_mutated(times) {
            return res;
        }
        if let Some(d) = self.strategy_delete(t, input) {
            res.push(d);
        }
        if let Some(i) = self.strategy_insert(t, input) {
            res.push(i);
        }
        if let Some(r) = self.strategy_replace(t, input) {
            res.push(r);
        }
        let new_times = times + res.len() as u32;
        t.get_mut(input).mutated_times = new_times;
        for sub in &mut res {
            if let Some(r) = sub.root {
                sub.get_mut(r).mutated_times = new_times;
            }
        }
        res
    }

    /// Remove a random statement, and insert a random non-SELECT statement
    /// from the library. Returns the new query trees.
    pub fn mutate_stmtlist(&mut self, t: &IrTree, root: IrId) -> Vec<IrTree> {
        let mut res = Vec::new();
        // delete
        let mut cur = t.extract(root);
        let cur_root = cur.root.expect("root");
        let n = ir_wrapper::get_stmt_num(&cur, cur_root);
        let idx = self.rng.rand_int(n);
        ir_wrapper::remove_stmt_at_idx_and_free(&mut cur, cur_root, idx);
        res.push(cur);
        // insert
        let mut cur = t.extract(root);
        let cur_root = cur.root.expect("root");
        let n = ir_wrapper::get_stmt_num(&cur, cur_root);
        let insert_pos = self.rng.rand_int(n);
        let mut new_stmt: Option<IrTree> = None;
        for _ in 0..100 {
            let cand = self.get_from_library_with_type(IrType::Stmt);
            let Some(cand_root) = cand.root else {
                return res;
            };
            let Some(l) = cand.left(cand_root) else {
                return res;
            };
            if cand.ty(l) == IrType::SelectStmt {
                continue;
            }
            new_stmt = Some(cand.extract(l));
            break;
        }
        let Some(new_stmt) = new_stmt else {
            return res;
        };
        let imported = cur.import(&new_stmt, new_stmt.root.expect("root"));
        if ir_wrapper::append_stmt_at_idx(&mut cur, cur_root, imported, insert_pos) {
            res.push(cur);
        }
        res
    }

    /// Mutate one node of the query and return every new query (as full
    /// trees) that was not seen before.
    pub fn mutate_all(&mut self, t: &mut IrTree, root: IrId, ir_to_mutate: IrId) -> Vec<IrTree> {
        let mut res = Vec::new();
        let ty = t.ty(ir_to_mutate);
        if ty == IrType::ParseToplevel {
            return res;
        }
        if ty == IrType::Stmtmulti {
            for mutated in self.mutate_stmtlist(t, root) {
                let r = mutated.root.expect("root");
                let h = hash_str(&mutated.to_string(r));
                if !self.global_hash.insert(h) {
                    continue;
                }
                res.push(mutated);
            }
            return res;
        }
        for new_sub in self.mutate(t, ir_to_mutate) {
            self.stats.total_mutate_num += 1;
            let Some(sub_root) = new_sub.root else {
                continue;
            };
            let new_node = t.import(&new_sub, sub_root);
            if !t.swap_node(root, ir_to_mutate, Some(new_node)) {
                self.stats.total_mutate_failed += 1;
                continue;
            }
            let text = t.to_string(root);
            let h = hash_str(&text);
            if !self.global_hash.insert(h) {
                t.swap_node(root, new_node, Some(ir_to_mutate));
                self.stats.total_mutate_failed += 1;
                continue;
            }
            res.push(t.extract(root));
            t.swap_node(root, new_node, Some(ir_to_mutate));
        }
        res
    }

    /// Add `CREATE TABLE`, `INSERT` and `CREATE INDEX` statements from the
    /// library at the start and at the end of the query.
    pub fn add_missing_create_table_stmt(&mut self, t: &mut IrTree, root: IrId) -> bool {
        if t.ty(root) != IrType::ParseToplevel {
            return false;
        }
        for pass in 0..2 {
            let mut stmts = Vec::new();
            for ty in [IrType::CreateStmt, IrType::InsertStmt, IrType::IndexStmt] {
                let Some(sub) = self.get_ir_with_type(ty) else {
                    return false;
                };
                let r = sub.root.expect("root");
                if sub.left(r).is_none() {
                    return false;
                }
                stmts.push(sub);
            }
            for (i, sub) in stmts.iter().enumerate() {
                let node = t.import(sub, sub.root.expect("root"));
                if pass == 0 {
                    ir_wrapper::append_stmt_at_idx(t, root, node, i);
                } else {
                    ir_wrapper::append_stmt_at_end(t, root, node);
                }
            }
        }
        true
    }

    // -- the validation pipeline -----------------------------------------------------

    /// Reset the state that belongs to one test case.
    pub fn pre_validate(&mut self) {
        self.reset_id_counter();
        self.ctx.reset_data_library();
    }

    /// Classify every statement and apply the pre-fix transformation of the
    /// oracle. Returns one tree per statement (the statement is its root).
    pub fn pre_fix_transform(
        &mut self,
        oracle: &dyn Oracle,
        t: &mut IrTree,
        root: IrId,
    ) -> (Vec<IrTree>, Vec<StmtType>) {
        let mut trees = Vec::new();
        let mut types = Vec::new();
        for stmt in ir_wrapper::get_stmt_ir_vec(t, root) {
            let stmt_type = if oracle.is_oracle_normal_stmt(t, stmt) {
                StmtType::OracleNormal
            } else if oracle.is_oracle_select_stmt(t, stmt) {
                StmtType::OracleSelect
            } else {
                StmtType::NotOracle
            };
            types.push(stmt_type);
            let trans = match stmt_type {
                StmtType::OracleNormal => oracle.pre_fix_transform_normal_stmt(self, t, stmt),
                StmtType::OracleSelect => oracle.pre_fix_transform_select_stmt(self, t, stmt),
                StmtType::NotOracle => None,
            };
            trees.push(trans.unwrap_or_else(|| t.extract(stmt)));
        }
        (trees, types)
    }

    /// Fill in the names of one statement so it fits the statements before
    /// it. `stmt` is the statement node (the root of its tree).
    pub fn validate(&mut self, t: &mut IrTree, stmt: IrId) -> bool {
        self.fix_one_stmt(t, stmt)
    }

    fn fix_one_stmt(&mut self, t: &mut IrTree, stmt: IrId) -> bool {
        self.ctx.reset_data_library_single_stmt();
        let (substmts, saved) = self.split_to_substmt(t, stmt);
        if substmts.len() > 10 {
            Self::connect_back(t, &saved);
            return false;
        }
        let mut ir_to_fix: Vec<Vec<IrId>> = Vec::new();
        for sub in &substmts {
            ir_to_fix.push(crate::fix::collect_fixable(t, *sub));
        }
        Self::connect_back(t, &saved);
        self.fix_dependency(t, stmt, &ir_to_fix)
    }

    /// Detach the subqueries of a statement so that each can be handled on
    /// its own. Returns the statement and its subqueries (outer first), and
    /// what to reconnect.
    fn split_to_substmt(&self, t: &mut IrTree, stmt: IrId) -> (Vec<IrId>, Vec<(IrId, bool, IrId)>) {
        let mut res = vec![stmt];
        let mut saved = Vec::new();
        let mut bfs = std::collections::VecDeque::from([stmt]);
        while let Some(node) = bfs.pop_front() {
            if let Some(l) = t.left(node) {
                bfs.push_back(l);
            }
            if let Some(r) = t.right(node) {
                bfs.push_back(r);
            }
            if let Some(l) = t.left(node) {
                if self.split_substmt_types.contains(&t.ty(l))
                    && ir_wrapper::is_in_subquery(t, stmt, l)
                {
                    res.push(l);
                    saved.push((node, true, l));
                }
            }
            if let Some(r) = t.right(node) {
                if self.split_substmt_types.contains(&t.ty(r))
                    && ir_wrapper::is_in_subquery(t, stmt, r)
                {
                    res.push(r);
                    saved.push((node, false, r));
                }
            }
        }
        for sub in res.iter().skip(1) {
            t.detach_node(stmt, *sub);
        }
        (res, saved)
    }

    fn connect_back(t: &mut IrTree, saved: &[(IrId, bool, IrId)]) {
        for (parent, is_left, child) in saved {
            if *is_left {
                t.update_left(*parent, Some(*child));
            } else {
                t.update_right(*parent, Some(*child));
            }
        }
    }

    /// Apply the post-fix transformation of the oracle to every statement,
    /// for every run. The result is, per run, the list of (equivalent forms,
    /// statement type). Statements with no forms are left out of that run.
    pub fn post_fix_transform(
        &mut self,
        oracle: &dyn Oracle,
        trees: &mut [IrTree],
        types: &[StmtType],
    ) -> Vec<Vec<(Vec<IrTree>, StmtType)>> {
        let runs = oracle.mul_run_num().max(1);
        let mut all_runs = Vec::new();
        for run in 0..runs {
            let mut run_vec = Vec::new();
            for (tree, stmt_type) in trees.iter_mut().zip(types.iter()) {
                let root = tree.root.expect("root");
                let forms = match stmt_type {
                    StmtType::OracleNormal => {
                        oracle.post_fix_transform_normal_stmt(self, tree, root, run)
                    }
                    StmtType::OracleSelect => {
                        oracle.post_fix_transform_select_stmt(self, tree, root, run)
                    }
                    StmtType::NotOracle => vec![tree.extract(root)],
                };
                if !forms.is_empty() {
                    run_vec.push((forms, *stmt_type));
                }
            }
            all_runs.push(run_vec);
        }
        all_runs
    }

    /// Turn the forms of one run into two query strings: with result markers
    /// around each oracle SELECT, and without.
    pub fn ir_to_string(run: &[(Vec<IrTree>, StmtType)]) -> (String, String) {
        let mut marked = String::new();
        let mut unmarked = String::new();
        for (forms, stmt_type) in run {
            let mut count = 0;
            for form in forms {
                let text = form.to_string(form.root.expect("root"));
                if *stmt_type == StmtType::OracleSelect {
                    marked.push_str(&format!("SELECT 'BEGIN VERI {count}'; \n"));
                    marked.push_str(&format!("{text}; \n"));
                    marked.push_str(&format!("SELECT 'END VERI {count}'; \n"));
                    unmarked.push_str(&format!("{text}; \n"));
                    count += 1;
                } else {
                    marked.push_str(&format!("{text}; \n"));
                    unmarked.push_str(&format!("{text}; \n"));
                }
            }
        }
        (marked, unmarked)
    }

    /// Replace a `reloption` node by a new `name = value` node (or a bare
    /// name for RESET). Used by the instantiation step.
    pub(crate) fn new_reloption_node(t: &mut IrTree, name: &str, value: Option<&str>) -> IrId {
        let label = t.leaf(IrType::ReloptionElem, name);
        match value {
            Some(v) => {
                let args = t.leaf(IrType::ReloptionElem, v);
                t.node(
                    IrType::ReloptionElem,
                    Op::new("", "=", ""),
                    Some(label),
                    Some(args),
                )
            }
            None => t.node(
                IrType::ReloptionElem,
                Op::new("", "", ""),
                Some(label),
                None,
            ),
        }
    }

    pub(crate) fn rel_option_type(t: &IrTree, id: IrId) -> RelOptionType {
        t.option_type(id)
    }
}

/// Parse with a trailing semicolon added, like the C++ helper.
pub fn parse_with_semicolon(sql: &str) -> Option<ParsedQuery> {
    let mut s = sql.to_string();
    ensure_semicolon_at_query_end(&mut s);
    parse_query(&s)
}

const SYS_CATALOG_NAMES: &[&str] = &[
    "pg_aggregate",
    "g_am",
    "g_amop",
    "g_amproc",
    "g_attrdef",
    "g_attribute",
    "g_authid",
    "g_auth_members",
    "pg_cast",
    "pg_class",
    "pg_collation",
    "pg_constraint",
    "pg_conversion",
    "pg_database",
    "pg_db_role_setting",
    "pg_default_acl",
    "pg_depend",
    "pg_description",
    "pg_enum",
    "pg_event_trigger",
    "pg_extension",
    "pg_foreign_data_wrapper",
    "pg_foreign_server",
    "pg_foreign_table",
    "pg_index",
    "pg_inherits",
    "pg_init_privs",
    "pg_language",
    "pg_largeobject",
    "pg_largeobject_metadata",
    "pg_namespace",
    "pg_opclass",
    "pg_operator",
    "pg_opfamily",
    "pg_partitioned_table",
    "pg_policy",
    "pg_proc",
    "pg_publication",
    "pg_publication_rel",
    "pg_range",
    "pg_replication_origin",
    "pg_rewrite",
    "pg_seclabel",
    "pg_sequence",
    "pg_shdepend",
    "pg_shdescription",
    "pg_shseclabel",
    "pg_statistic",
    "pg_statistic_ext",
    "pg_statistic_ext_data",
    "pg_subscription",
    "pg_subscription_rel",
    "pg_tablespace",
    "pg_transform",
    "pg_trigger",
    "pg_ts_config",
    "pg_ts_config_map",
    "pg_ts_dict",
    "pg_ts_parser",
    "pg_ts_template",
    "pg_type",
    "pg_user_mapping",
    "System Views",
    "pg_available_extensions",
    "pg_available_extension_versions",
    "pg_backend_memory_contexts",
    "pg_config",
    "pg_cursors",
    "pg_file_settings",
    "pg_group",
    "pg_hba_file_rules",
    "pg_indexes",
    "pg_locks",
    "pg_matviews",
    "pg_policies",
    "pg_prepared_statements",
    "pg_prepared_xacts",
    "pg_publication_tables",
    "pg_replication_origin_status",
    "pg_replication_slots",
    "pg_roles",
    "pg_rules",
    "pg_seclabels",
    "pg_sequences",
    "pg_settings",
    "pg_shadow",
    "pg_shmem_allocations",
    "pg_stats",
    "pg_stats_ext",
    "pg_stats_ext_exprs",
    "pg_tables",
    "pg_timezone_abbrevs",
    "pg_timezone_names",
    "pg_user",
    "pg_user_mappings",
    "pg_views",
];

/// True for a result marker statement (`SELECT 'BEGIN VERI n';` or
/// `SELECT 'END VERI n';`). A saved test case holds these statements. They
/// must not go into the library: a mutation can then put the marker text
/// into an oracle SELECT, and a result row with that text moves the
/// borders of the result blocks.
pub fn is_result_marker(stmt: &str) -> bool {
    let s = stmt.trim().trim_end_matches(';').trim();
    let Some(head) = s.get(..6) else {
        return false;
    };
    if !head.eq_ignore_ascii_case("select") {
        return false;
    }
    let Some(inner) = s[6..]
        .trim()
        .strip_prefix('\'')
        .and_then(|q| q.strip_suffix('\''))
    else {
        return false;
    };
    inner
        .strip_prefix("BEGIN VERI ")
        .or_else(|| inner.strip_prefix("END VERI "))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::AllCompRes;

    struct NoSelect;
    impl Oracle for NoSelect {
        fn oracle_type(&self) -> &'static str {
            "TEST"
        }
        fn template_select_stmt(&self, _m: &mut Mutator) -> String {
            String::new()
        }
        fn is_oracle_select_stmt(&self, _t: &mut IrTree, _stmt: IrId) -> bool {
            false
        }
        fn post_fix_transform_select_stmt(
            &self,
            _m: &mut Mutator,
            t: &mut IrTree,
            stmt: IrId,
            _run: usize,
        ) -> Vec<IrTree> {
            vec![t.extract(stmt)]
        }
        fn compare_results(&self, _res: &mut AllCompRes) {}
    }

    #[test]
    fn finds_result_markers() {
        assert!(is_result_marker("SELECT 'BEGIN VERI 0';"));
        assert!(is_result_marker("select 'END VERI 12'"));
        assert!(!is_result_marker("SELECT 'END VERI 0' AS a FROM t;"));
        assert!(!is_result_marker("SELECT 'END VERI x';"));
        assert!(!is_result_marker("SELECT 1;"));
    }

    #[test]
    fn library_skips_result_markers() {
        let mut m = Mutator::new(1);
        m.add_all_to_library(&NoSelect, "SELECT 'BEGIN VERI 0'; SELECT 'END VERI 0';");
        assert!(m.real_ir_set.values().all(|v| v.is_empty()));
        m.add_all_to_library(&NoSelect, "CREATE TABLE t (c INT);");
        assert!(m.real_ir_set.values().any(|v| !v.is_empty()));
    }
}
