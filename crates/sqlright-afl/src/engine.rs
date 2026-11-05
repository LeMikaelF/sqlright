//! The SQLRight fuzz loop as a test-case generator. Port of `fuzz_one`,
//! `get_oracle_select_stmts`, `execute_cmd_string` and the result handling
//! in `afl-fuzz.cpp`, without the parts AFL++ does itself.
//!
//! One seed (a queue entry) becomes many test cases:
//!
//! 1. Parse the seed and remove its SELECT statements.
//! 2. If it drops more tables than it creates, add CREATE TABLE, INSERT and
//!    CREATE INDEX statements from the library.
//! 3. Make `select_count` oracle-compatible SELECTs (library SELECTs mutated
//!    while the oracle still accepts them).
//! 4. For every IR node, apply the three mutation strategies. Every result
//!    that parses again gets the SELECTs appended, its names filled in by
//!    the instantiation step, and the oracle rewrites applied.
//! 5. Print each result with result markers around the oracle SELECTs.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use sqlright_ir::util::{find_string_in, hash_str, is_str_empty};
use sqlright_ir::IrType;
use sqlright_mutator::ir_wrapper;
use sqlright_mutator::oracle::{
    get_random_mutated_select_stmt, remove_oracle_select_stmt_from_ir, remove_select_stmt_from_ir,
    AllCompRes, OraCompRes, Oracle,
};
use sqlright_mutator::Mutator;
use sqlright_oracle::result::{
    compare_query_result, compare_query_results_cross_run, format_report, split_runs, RUN_MARKER,
};
use sqlright_pg_parser::parse_query;

use crate::config::Config;

/// Functions whose results change between runs. A test case that names one
/// is not checked (the C++ only filtered `random`).
const NON_DETERMINISTIC: &[&str] = &[
    "random",
    "now(",
    "current_timestamp",
    "current_date",
    "current_time",
    "clock_timestamp",
    "statement_timestamp",
    "transaction_timestamp",
    "timeofday",
    "gen_random_uuid",
    "pg_backend_pid",
    "txid_current",
    "pg_sleep",
    "localtime",
    "pg_postmaster_start_time",
];

/// One test case: the text per run (with result markers) and the same text
/// without markers.
#[derive(Debug, Clone)]
pub struct TestCase {
    pub marked_runs: Vec<String>,
    pub unmarked_runs: Vec<String>,
}

impl TestCase {
    /// The size of the text of all runs, with and without markers.
    fn size_in_bytes(&self) -> usize {
        let marked: usize = self.marked_runs.iter().map(String::len).sum();
        let unmarked: usize = self.unmarked_runs.iter().map(String::len).sum();
        marked + unmarked
    }

    /// The bytes AFL++ feeds to the harness: the runs separated by run markers.
    pub fn to_input(&self) -> String {
        let mut out = String::new();
        for (i, run) in self.marked_runs.iter().enumerate() {
            if i > 0 {
                out.push_str(&format!("{RUN_MARKER} {i}\n"));
            }
            out.push_str(run);
        }
        out
    }
}

/// The result of checking one test case.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub result: OraCompRes,
    pub all: AllCompRes,
    /// The report file, when the verdict is a failure that was not seen before.
    pub report_path: Option<std::path::PathBuf>,
}

/// Counters of the fuzz loop, like the C++ status screen.
#[derive(Debug, Clone, Default)]
pub struct EngineStats {
    pub seeds: u64,
    pub total_input_failed: u64,
    pub num_mutate_all: u64,
    pub num_reparse: u64,
    pub num_append: u64,
    pub num_validate: u64,
    pub total_mutate_failed: u64,
    pub total_append_failed: u64,
    pub cases_generated: u64,
    pub cases_checked: u64,
    pub skipped_non_deterministic: u64,
    pub output_mismatch: u64,
    pub oracle_pass: u64,
    pub oracle_fail: u64,
    pub oracle_error: u64,
    pub bug_reports: u64,
}

pub struct Engine {
    pub config: Config,
    pub mutator: Mutator,
    pub oracle: Box<dyn Oracle>,
    pub stats: EngineStats,
    pending: VecDeque<TestCase>,
    last_emitted: Option<TestCase>,
    reported: HashSet<u64>,
}

impl Engine {
    /// Build the engine and fill the library from the seed directories.
    pub fn new(config: Config, rng_seed: u64) -> Result<Engine, String> {
        let oracle = sqlright_oracle::oracle_by_name(&config.oracle)
            .ok_or_else(|| format!("unknown oracle {}", config.oracle))?;
        let seed = config.rng_seed.unwrap_or(rng_seed);
        let mut engine = Engine {
            mutator: Mutator::new(seed),
            oracle,
            stats: EngineStats::default(),
            pending: VecDeque::new(),
            last_emitted: None,
            reported: HashSet::new(),
            config,
        };
        let mut dirs = engine.config.init_lib_dirs.clone();
        if let Some(seed_dir) = engine.config.seed_dir.clone() {
            dirs.push(seed_dir);
        }
        for dir in dirs {
            engine.load_library_dir(&dir);
        }
        std::fs::create_dir_all(&engine.config.bug_dir)
            .map_err(|e| format!("cannot create {}: {e}", engine.config.bug_dir.display()))?;
        Ok(engine)
    }

    /// Add every file of a directory to the mutation library.
    pub fn load_library_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            self.log(&format!("cannot read library directory {}", dir.display()));
            return;
        };
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .collect();
        paths.sort();
        let mut total = 0;
        for path in &paths {
            match self.mutator.init_from_file(self.oracle.as_ref(), path) {
                Ok(n) => total += n,
                Err(e) => self.log(&format!("cannot read {}: {e}", path.display())),
            }
        }
        self.log(&format!(
            "library: {} files, {} statements from {}",
            paths.len(),
            total,
            dir.display()
        ));
    }

    /// Add one queue entry (a saved test case) to the library, like the
    /// SQLite variant does for every interesting input.
    pub fn add_queue_entry(&mut self, text: &str) {
        let seed = without_result_markers(Self::first_run(text));
        let Some(mut q) = parse_query(&seed) else {
            return;
        };
        let stripped = self.mutator.extract_struct(&mut q.tree, q.root);
        self.mutator
            .add_all_to_library(self.oracle.as_ref(), &stripped);
    }

    /// The text of the first run of a test case: a queue entry of a
    /// multi-run oracle holds the schema several times.
    fn first_run(text: &str) -> &str {
        match text.find(RUN_MARKER) {
            Some(pos) => &text[..pos],
            None => text,
        }
    }

    /// Make the test cases of one seed and queue them for `next_case`.
    /// Returns how many there are.
    pub fn prepare(&mut self, seed: &str) -> usize {
        self.pending.clear();
        let cases = self.generate_cases(seed);
        self.pending.extend(cases);
        self.pending.len()
    }

    /// The next queued test case, and remember it for `check_last_output`.
    pub fn next_case(&mut self) -> Option<TestCase> {
        let case = self.pending.pop_front()?;
        self.last_emitted = Some(case.clone());
        Some(case)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// All test cases of one seed (the body of the C++ `fuzz_one`).
    pub fn generate_cases(&mut self, seed: &str) -> Vec<TestCase> {
        let mut cases = Vec::new();
        self.stats.seeds += 1;
        let seed_text = without_result_markers(Self::first_run(seed));
        let Some(mut q) = parse_query(&seed_text) else {
            self.stats.total_input_failed += 1;
            return cases;
        };
        let root = q.root;
        remove_oracle_select_stmt_from_ir(self.oracle.as_ref(), &mut q.tree, root);
        remove_select_stmt_from_ir(&mut q.tree, root);

        // A seed that drops more than it creates gets a schema to work on.
        let stmts = ir_wrapper::get_stmt_ir_vec(&q.tree, root);
        let create_num = stmts
            .iter()
            .filter(|s| q.tree.ty(**s) == IrType::CreateStmt)
            .count();
        let drop_num = stmts
            .iter()
            .filter(|s| q.tree.ty(**s) == IrType::DropStmt)
            .count();
        if drop_num >= create_num {
            self.mutator
                .add_missing_create_table_stmt(&mut q.tree, root);
        }
        let all_nodes = q.tree.all_nodes(root);

        let selects = self.get_oracle_select_stmts(self.config.select_count);
        let mut total_bytes = 0usize;

        for ir_to_mutate in all_nodes {
            if cases.len() >= self.config.max_cases_per_seed
                || total_bytes >= self.config.max_bytes_per_seed
            {
                break;
            }
            self.stats.num_mutate_all += 1;
            let mutated_roots = self.mutator.mutate_all(&mut q.tree, root, ir_to_mutate);
            for mutated in mutated_roots {
                let Some(mroot) = mutated.root else {
                    continue;
                };
                let ir_str = mutated.to_string(mroot);
                if ir_str.is_empty() {
                    self.stats.total_mutate_failed += 1;
                    continue;
                }
                // Only mutations that parse again are kept.
                let Some(mut cur) = parse_query(&ir_str) else {
                    self.stats.total_mutate_failed += 1;
                    continue;
                };
                self.stats.num_reparse += 1;
                self.append_random_stmts(&mut cur);
                for sel in &selects {
                    let Some(sel_root) = sel.root else {
                        continue;
                    };
                    let id = cur.tree.import(sel, sel_root);
                    ir_wrapper::append_stmt_at_end(&mut cur.tree, cur.root, id);
                }
                self.stats.num_append += 1;
                match self.instantiate(&mut cur) {
                    Some(case) => {
                        self.stats.num_validate += 1;
                        self.stats.cases_generated += 1;
                        total_bytes += case.size_in_bytes();
                        cases.push(case);
                    }
                    None => self.stats.total_append_failed += 1,
                }
                if cases.len() >= self.config.max_cases_per_seed
                    || total_bytes >= self.config.max_bytes_per_seed
                {
                    break;
                }
            }
        }
        cases
    }

    /// Oracle-specific statements to add at random positions (INDEX adds
    /// `CREATE INDEX` statements).
    fn append_random_stmts(&mut self, cur: &mut sqlright_pg_parser::ParsedQuery) {
        if self.oracle.oracle_type() != "INDEX" {
            return;
        }
        let count = self
            .mutator
            .rng
            .rand_int(sqlright_oracle::index::Index::RANDOM_APPEND_COUNT + 1);
        for _ in 0..count {
            let Some(stmt) = sqlright_oracle::index::Index::random_append_stmt(&mut self.mutator)
            else {
                continue;
            };
            let Some(stmt_root) = stmt.root else {
                continue;
            };
            // After the first statement at least, so a table can exist.
            let n = ir_wrapper::get_stmt_num(&cur.tree, cur.root);
            let pos = if n == 0 {
                0
            } else {
                1 + self.mutator.rng.rand_int(n)
            };
            let id = cur.tree.import(&stmt, stmt_root);
            ir_wrapper::append_stmt_at_idx(&mut cur.tree, cur.root, id, pos);
        }
    }

    /// Fill in the names, apply the oracle rewrites and print the runs.
    fn instantiate(&mut self, cur: &mut sqlright_pg_parser::ParsedQuery) -> Option<TestCase> {
        self.mutator.pre_validate();
        let (mut trees, types) =
            self.mutator
                .pre_fix_transform(self.oracle.as_ref(), &mut cur.tree, cur.root);
        for tree in trees.iter_mut() {
            let r = tree.root?;
            self.mutator.validate(tree, r);
        }
        let runs = self
            .mutator
            .post_fix_transform(self.oracle.as_ref(), &mut trees, &types);
        let mut marked_runs = Vec::new();
        let mut unmarked_runs = Vec::new();
        for run in &runs {
            let (marked, unmarked) = Mutator::ir_to_string(run);
            marked_runs.push(marked);
            unmarked_runs.push(unmarked);
        }
        if marked_runs.is_empty() || is_str_empty(&marked_runs[0]) {
            return None;
        }
        if unmarked_runs.iter().any(|r| contains_result_marker(r)) {
            return None;
        }
        Some(TestCase {
            marked_runs,
            unmarked_runs,
        })
    }

    /// Up to `max` oracle-compatible SELECTs (the C++ `get_oracle_select_stmts`).
    fn get_oracle_select_stmts(&mut self, max: usize) -> Vec<sqlright_ir::IrTree> {
        let mut out = Vec::new();
        let max_trial = max * 10;
        let mut trial = 0;
        while out.len() < max && trial < max_trial {
            trial += 1;
            if let Some(sel) =
                get_random_mutated_select_stmt(self.oracle.as_ref(), &mut self.mutator)
            {
                out.push(sel);
            }
        }
        out
    }

    /// Compare the harness output of the last emitted test case. Output
    /// that names the hash of another input (a stale file, a crashed or
    /// killed harness) is not compared.
    pub fn check_last_output(&mut self, output: &str) -> Option<Verdict> {
        let case = self.last_emitted.take()?;
        if !output_belongs_to(output, &case.to_input()) {
            self.stats.output_mismatch += 1;
            return None;
        }
        Some(self.check_output(&case, output))
    }

    /// Compare the harness output of a test case and write a report when the
    /// oracle finds a difference.
    pub fn check_output(&mut self, case: &TestCase, output: &str) -> Verdict {
        self.stats.cases_checked += 1;
        let (result, all) = self.compare(case, output);
        let mut report_path = None;
        match result {
            OraCompRes::Pass => self.stats.oracle_pass += 1,
            OraCompRes::Error => {
                if all.cmd_str.is_empty() && all.v_cmd_str.is_empty() {
                    self.stats.skipped_non_deterministic += 1;
                } else {
                    self.stats.oracle_error += 1;
                }
            }
            OraCompRes::Fail => {
                self.stats.oracle_fail += 1;
                report_path = self.write_report(case, &all);
            }
        }
        Verdict {
            result,
            all,
            report_path,
        }
    }

    /// The oracle verdict for the harness output of a test case. A test
    /// case that calls a function with changing results gets `Error` and an
    /// empty comparison.
    pub fn compare(&self, case: &TestCase, output: &str) -> (OraCompRes, AllCompRes) {
        let input = case.to_input();
        if is_non_deterministic(&input) {
            return (OraCompRes::Error, AllCompRes::default());
        }
        let mut all = AllCompRes::default();
        if self.oracle.mul_run_num() <= 1 {
            all.cmd_str = case.marked_runs[0].clone();
            all.res_str = output.to_string();
            compare_query_result(self.oracle.as_ref(), &mut all);
        } else {
            all.v_cmd_str = case.marked_runs.clone();
            all.v_res_str = split_runs(output);
            compare_query_results_cross_run(self.oracle.as_ref(), &mut all);
        }
        let result = all.final_res.unwrap_or(OraCompRes::Error);
        (result, all)
    }

    /// Write `bug_<hash>.txt` (the report) and `bug_<hash>.sql` (the test
    /// case) in the bug directory. The same test case is reported once.
    fn write_report(&mut self, case: &TestCase, all: &AllCompRes) -> Option<std::path::PathBuf> {
        let input = case.to_input();
        let h = hash_str(&input);
        if !self.reported.insert(h) {
            return None;
        }
        let stem = format!("bug_{}_{h:016x}", self.oracle.oracle_type());
        let report = self.config.bug_dir.join(format!("{stem}.txt"));
        let sql = self.config.bug_dir.join(format!("{stem}.sql"));
        if std::fs::write(&sql, &input).is_err() {
            return None;
        }
        let text = format_report(self.oracle.as_ref(), all);
        if std::fs::write(&report, text).is_err() {
            return None;
        }
        self.stats.bug_reports += 1;
        Some(report)
    }

    /// Write the counters to `stats_<pid>.txt` in the bug directory.
    pub fn write_stats(&self) {
        let path = self
            .config
            .bug_dir
            .join(format!("stats_{}.txt", std::process::id()));
        let s = &self.stats;
        let ms = &self.mutator.stats;
        let text = format!(
            "oracle: {}\nseeds: {}\ninput parse failed: {}\nmutate_all calls: {}\nmutations that parse: {}\nmutations that do not parse: {}\nappend failed: {}\ncases generated: {}\ncases checked: {}\nskipped (non-deterministic): {}\noutput of another input: {}\noracle pass: {}\noracle fail: {}\noracle error: {}\nbug reports: {}\nlibrary selects: {}\noracle select mutations: {} ({} ok)\n",
            self.oracle.oracle_type(),
            s.seeds,
            s.total_input_failed,
            s.num_mutate_all,
            s.num_reparse,
            s.total_mutate_failed,
            s.total_append_failed,
            s.cases_generated,
            s.cases_checked,
            s.skipped_non_deterministic,
            s.output_mismatch,
            s.oracle_pass,
            s.oracle_fail,
            s.oracle_error,
            s.bug_reports,
            self.mutator.get_valid_collection_size(),
            ms.num_oracle_select_mutate,
            ms.num_oracle_select_succeed,
        );
        let _ = std::fs::write(path, text);
    }

    pub fn log(&self, msg: &str) {
        if self.config.debug {
            eprintln!("[sqlright] {msg}");
        }
    }
}

/// The text without the lines that hold result-marker text. A queue entry
/// holds the `SELECT 'BEGIN VERI n'` statements of its test case. If they
/// get into the library, a mutated SELECT can print `BEGIN VERI n` as a
/// row, and that row breaks the result blocks of the output.
pub fn without_result_markers(text: &str) -> String {
    text.lines()
        .filter(|l| !l.contains("BEGIN VERI ") && !l.contains("END VERI "))
        .flat_map(|l| [l, "\n"])
        .collect()
}

/// The first line the harness writes: this marker and the FNV-1a hash of its input.
pub const INPUT_MARKER: &str = "-- SQLRIGHT INPUT";

/// True when the first output line names the hash of `input`. Output
/// without a hash line (another harness) is accepted.
pub fn output_belongs_to(output: &str, input: &str) -> bool {
    let first = output.lines().next().unwrap_or("");
    let Some(rest) = first.strip_prefix(INPUT_MARKER) else {
        return true;
    };
    u64::from_str_radix(rest.trim(), 16)
        .map(|h| h == fnv1a(input.as_bytes()))
        .unwrap_or(false)
}

/// 64-bit FNV-1a, the same function as in the harness.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// True when the test case uses a function whose result changes between
/// runs, so that a difference proves nothing.
pub fn is_non_deterministic(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    NON_DETERMINISTIC.iter().any(|f| find_string_in(&lower, f))
        || input.lines().any(|l| l.trim_start().starts_with('!'))
}

/// True when the text holds `BEGIN VERI` or `END VERI`. The result
/// extraction finds the result blocks by these words. A string literal
/// with these words in a statement other than a result marker can show in
/// a result row or an error message, and the extraction then splits the
/// output at the wrong place.
fn contains_result_marker(text: &str) -> bool {
    text.contains("BEGIN VERI") || text.contains("END VERI")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_case_with_a_marker_literal_is_not_generated() {
        let mut engine = Engine::new(test_config("NOREC"), 1).unwrap();
        let lib = "CREATE TABLE t (a INT, b TEXT);\nSELECT COUNT(*) FROM t WHERE b = 'BEGIN VERI 1';\nSELECT COUNT(*) FROM t WHERE a > 0;\n";
        for line in lib.lines() {
            engine.mutator.init_from_line(engine.oracle.as_ref(), line);
        }
        let seed = "CREATE TABLE v0 (c1 INT, c2 TEXT);\nINSERT INTO v0 VALUES (1, 'BEGIN VERI 1');\nINSERT INTO v0 VALUES (2, 'END VERI 0');\n";
        let cases = engine.generate_cases(seed);
        for c in &cases {
            assert!(
                !contains_result_marker(&c.unmarked_runs[0]),
                "{}",
                c.unmarked_runs[0]
            );
        }
        assert!(contains_result_marker("SELECT format('p', 'BEGIN VERI 1')"));
        assert!(!contains_result_marker("SELECT 'BEGIN'"));
    }

    fn test_config(oracle: &str) -> Config {
        let dir = std::env::temp_dir().join(format!(
            "sqlright-engine-test-{}-{}",
            std::process::id(),
            oracle
        ));
        Config {
            oracle: oracle.to_string(),
            init_lib_dirs: Vec::new(),
            seed_dir: None,
            output_path: dir.join("out.txt"),
            bug_dir: dir,
            select_count: 3,
            max_cases_per_seed: 50,
            max_bytes_per_seed: usize::MAX,
            debug: false,
            rng_seed: Some(42),
        }
    }

    #[test]
    fn generates_cases_from_a_seed() {
        let mut engine = Engine::new(test_config("NOREC"), 1).unwrap();
        // A small library: a schema and some SELECTs the oracle accepts.
        let lib = "CREATE TABLE t (a INT, b INT);\nINSERT INTO t VALUES (1, 2);\nCREATE INDEX i ON t (a);\nSELECT COUNT(*) FROM t WHERE a > 0;\nSELECT COUNT(*) FROM t WHERE b = 2 AND a < 10;\n";
        for line in lib.lines() {
            engine.mutator.init_from_line(engine.oracle.as_ref(), line);
        }
        let seed = "CREATE TABLE v0 (c1 INT, c2 TEXT);\nINSERT INTO v0 VALUES (1, 'x');\nSELECT COUNT(*) FROM v0 WHERE c1 = 1;\n";
        let cases = engine.generate_cases(seed);
        assert!(!cases.is_empty(), "no cases generated");
        let case = &cases[0];
        assert!(
            case.marked_runs[0].contains("BEGIN VERI 0"),
            "{}",
            case.marked_runs[0]
        );
        assert!(
            case.marked_runs[0].contains("CREATE TABLE v0"),
            "{}",
            case.marked_runs[0]
        );
        // Every generated case parses as a whole.
        for c in &cases {
            assert!(
                parse_query(&c.marked_runs[0]).is_some(),
                "{}",
                c.marked_runs[0]
            );
        }
    }

    #[test]
    fn stops_making_cases_when_their_text_is_too_big() {
        let lib = "CREATE TABLE t (a INT, b INT);\nINSERT INTO t VALUES (1, 2);\nSELECT COUNT(*) FROM t WHERE a > 0;\nSELECT COUNT(*) FROM t WHERE b = 2 AND a < 10;\n";
        let seed = "CREATE TABLE v0 (c1 INT, c2 TEXT);\nINSERT INTO v0 VALUES (1, 'x');\nSELECT COUNT(*) FROM v0 WHERE c1 = 1;\n";
        let make = |max_bytes: usize| {
            let mut config = test_config("NOREC");
            config.max_bytes_per_seed = max_bytes;
            let mut engine = Engine::new(config, 1).unwrap();
            for line in lib.lines() {
                engine.mutator.init_from_line(engine.oracle.as_ref(), line);
            }
            engine.generate_cases(seed)
        };
        let all = make(usize::MAX);
        assert!(all.len() > 3, "{}", all.len());
        let few = make(1);
        assert_eq!(few.len(), 1);
    }

    #[test]
    fn result_markers_of_a_queue_entry_do_not_get_into_the_library() {
        let entry = "CREATE TABLE v0 (c1 INT); \nSELECT 'BEGIN VERI 0'; \nSELECT 'BEGIN VERI 0' FROM v0 WHERE c1 = 1; \nSELECT c1 FROM v0 WHERE c1 = 2; \nSELECT 'END VERI 0'; \n";
        assert_eq!(
            without_result_markers(entry),
            "CREATE TABLE v0 (c1 INT); \nSELECT c1 FROM v0 WHERE c1 = 2; \n"
        );
        let mut engine = Engine::new(test_config("TLP"), 1).unwrap();
        engine.add_queue_entry(entry);
        let selects = &engine.mutator.all_valid_pstr_vec;
        assert!(selects.iter().any(|s| s.contains("= 2")), "{selects:?}");
        assert!(!selects.iter().any(|s| s.contains("VERI")), "{selects:?}");
    }

    #[test]
    fn output_of_another_input_is_not_compared() {
        let input = "SELECT 1;\n";
        let good = format!("{INPUT_MARKER} {:016x}\n1 \n", fnv1a(input.as_bytes()));
        assert!(output_belongs_to(&good, input));
        assert!(!output_belongs_to(
            &format!("{INPUT_MARKER} 0000000000000000\n"),
            input
        ));
        assert!(
            output_belongs_to("1 \n", input),
            "output without a hash line is accepted"
        );
    }

    #[test]
    fn reports_a_mismatch() {
        let mut engine = Engine::new(test_config("NOREC"), 1).unwrap();
        let case = TestCase {
            marked_runs: vec!["CREATE TABLE v0 (c1 INT); \nSELECT 'BEGIN VERI 0'; \nSELECT COUNT(*) FROM v0 WHERE c1 = 1; \nSELECT 'END VERI 0'; \nSELECT 'BEGIN VERI 1'; \nSELECT SUM(x) FROM (SELECT (c1 = 1)::INT AS x FROM v0) AS r; \nSELECT 'END VERI 1'; \n".to_string()],
            unmarked_runs: vec![String::new()],
        };
        let output = "BEGIN VERI 0 \n2 \nEND VERI 0 \nBEGIN VERI 1 \n1 \nEND VERI 1 \n";
        let verdict = engine.check_output(&case, output);
        assert_eq!(verdict.result, OraCompRes::Fail);
        assert!(verdict.report_path.is_some());
        let again = engine.check_output(&case, output);
        assert!(
            again.report_path.is_none(),
            "the same case is reported once"
        );
        let ok = engine.check_output(
            &case,
            "BEGIN VERI 0 \n2 \nEND VERI 0 \nBEGIN VERI 1 \n2 \nEND VERI 1 \n",
        );
        assert_eq!(ok.result, OraCompRes::Pass);
    }
}
