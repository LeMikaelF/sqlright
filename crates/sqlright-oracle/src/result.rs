//! From the text output of the database to a verdict. Port of
//! `extract_query_result`, `compare_query_result` and
//! `compare_query_results_cross_run` in `afl-fuzz.cpp`.
//!
//! The database output has one line per row, the values of a row separated
//! by spaces, and one line per error that holds `ERROR`. The oracle SELECTs
//! are wrapped in `SELECT 'BEGIN VERI n'` / `SELECT 'END VERI n'`
//! statements, so their rows sit between `BEGIN VERI n` and `END VERI n`
//! lines.

use sqlright_ir::util::{find_string_in, is_str_empty, trimmed};
use sqlright_mutator::oracle::{AllCompRes, CompRes, OraCompRes, Oracle};

/// The line that separates the runs of a multi-run test case, in the test
/// case and in the output.
pub const RUN_MARKER: &str = "-- SQLRIGHT RUN";

/// The output of a test case split per run.
pub fn split_runs(output: &str) -> Vec<String> {
    let mut runs = vec![String::new()];
    for line in output.lines() {
        if line.starts_with(RUN_MARKER) {
            runs.push(String::new());
            continue;
        }
        let cur = runs.last_mut().unwrap();
        cur.push_str(line);
        cur.push('\n');
    }
    runs
}

/// The results between every `begin`/`end` pair, in order. A block with an
/// error becomes the single word `Error`.
pub fn extract_query_result(res: &str, begin_sign: &str, end_sign: &str) -> Vec<String> {
    let mut out = Vec::new();
    if is_str_empty(res) {
        return out;
    }
    let mut begin_idx = res.find(begin_sign);
    let mut end_idx = res.find(end_sign);
    while let Some(b) = begin_idx {
        let Some(e) = end_idx else {
            break;
        };
        let content_start = b + begin_sign.len();
        let cur = if e >= content_start {
            &res[content_start..e]
        } else {
            ""
        };
        begin_idx = res[content_start..]
            .find(begin_sign)
            .map(|p| p + content_start);
        end_idx = res[e + end_sign.len()..]
            .find(end_sign)
            .map(|p| p + e + end_sign.len());
        if cur.contains("Error") || cur.contains("ERROR") {
            out.push("Error".to_string());
        } else {
            out.push(trimmed(cur));
        }
    }
    out
}

/// Compare the forms of every oracle SELECT inside one run.
pub fn compare_query_result(oracle: &dyn Oracle, all: &mut AllCompRes) {
    all.final_res = Some(OraCompRes::Pass);
    if is_str_empty(&all.res_str) {
        all.final_res = Some(OraCompRes::Error);
        return;
    }
    if !markers_are_paired(&all.res_str) {
        all.final_res = Some(OraCompRes::Error);
        return;
    }
    let res_vecs: Vec<Vec<String>> = (0..4)
        .map(|i| {
            extract_query_result(
                &all.res_str,
                &format!("BEGIN VERI {i}"),
                &format!("END VERI {i}"),
            )
        })
        .collect();
    let max = res_vecs.iter().map(|v| v.len()).max().unwrap_or(0);
    for idx in 0..max {
        let mut comp = CompRes::default();
        if let Some(s) = res_vecs[0].get(idx) {
            comp.res_str_0 = s.clone();
        }
        if let Some(s) = res_vecs[1].get(idx) {
            comp.res_str_1 = s.clone();
        }
        if let Some(s) = res_vecs[2].get(idx) {
            comp.res_str_2 = s.clone();
        }
        if let Some(s) = res_vecs[3].get(idx) {
            comp.res_str_3 = s.clone();
        }
        all.v_res.push(comp);
    }
    oracle.compare_results(all);
}

/// Compare the first oracle SELECT form across the runs of a multi-run test
/// case (`v_res_str` holds one output per run).
pub fn compare_query_results_cross_run(oracle: &dyn Oracle, all: &mut AllCompRes) {
    all.final_res = Some(OraCompRes::Pass);
    let mut per_run: Vec<Vec<String>> = Vec::new();
    for res_str in &all.v_res_str {
        if is_str_empty(res_str) || !markers_are_paired(res_str) {
            all.final_res = Some(OraCompRes::Error);
            return;
        }
        per_run.push(extract_query_result(res_str, "BEGIN VERI 0", "END VERI 0"));
    }
    let Some(first) = per_run.first() else {
        all.final_res = Some(OraCompRes::Error);
        return;
    };
    for j in 0..first.len() {
        let mut comp = CompRes::default();
        for run in &per_run {
            match run.get(j) {
                Some(s) => comp.v_res_str.push(s.clone()),
                None => comp.comp_res = OraCompRes::Error,
            }
        }
        all.v_res.push(comp);
    }
    oracle.compare_results(all);
}

/// True when the result markers of an output are in pairs: each
/// `BEGIN VERI n` line has its `END VERI n` line after it, and no other
/// line holds the marker text. A result row with the marker text (a string
/// literal like `'END VERI 0'` in an oracle SELECT) moves the borders of
/// the blocks. Then the blocks of two forms do not belong together.
pub fn markers_are_paired(res: &str) -> bool {
    let mut open: Option<&str> = None;
    for line in res.lines() {
        if !line.contains("BEGIN VERI") && !line.contains("END VERI") {
            continue;
        }
        let line = line.trim();
        if let Some(n) = line.strip_prefix("BEGIN VERI ") {
            if open.is_some() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            open = Some(n);
        } else if let Some(n) = line.strip_prefix("END VERI ") {
            if open != Some(n) {
                return false;
            }
            open = None;
        } else {
            return false;
        }
    }
    open.is_none()
}

/// True when a result block is an error.
pub fn is_error_result(s: &str) -> bool {
    find_string_in(s, "ERROR")
}

/// Split the rows of a result block and count the non-empty ones. The C++
/// counts newlines and removes empty lines (rows of NULLs).
pub fn count_rows(block: &str) -> usize {
    block.split('\n').filter(|l| !is_str_empty(l)).count()
}

/// Human-readable report of one checked test case.
pub fn format_report(oracle: &dyn Oracle, all: &AllCompRes) -> String {
    let mut out = String::new();
    let verdict = all.final_res.map(|r| r.as_i32()).unwrap_or(-1);
    if oracle.mul_run_num() <= 1 {
        out.push_str("Query: \n");
        out.push_str(&all.cmd_str);
        out.push_str("\nResult string: \n");
        out.push_str(&all.res_str);
        out.push_str(&format!("\n\nFinal_res: {verdict}\nDetailed result: \n"));
        for (iter, res) in all.v_res.iter().enumerate() {
            out.push_str(&format!(
                "\n\nResult NUM: {iter} \nRESULT FLAGS: {}\n",
                res.comp_res.as_i32()
            ));
            out.push_str(&format!(
                "First stmt res is (str): {}\nFirst stmt res is (int): {}\n",
                res.res_str_0, res.res_int_0
            ));
            out.push_str(&format!(
                "Second stmt res is (str): {}\nSecond stmt is (int): {}\n",
                res.res_str_1, res.res_int_1
            ));
            out.push_str(&format!(
                "Third stmt res is (str): {}\nThird stmt is (int): {}\n",
                res.res_str_2, res.res_int_2
            ));
            out.push_str(&format!(
                "Fourth stmt res is (str): {}\nFourth stmt is (int): {}\n",
                res.res_str_3, res.res_int_3
            ));
        }
    } else {
        out.push_str("Multiple runs: \n");
        for (i, cmd) in all.v_cmd_str.iter().enumerate() {
            out.push_str(&format!("Query: {i}: \n{cmd}\nResult string: \n"));
            if let Some(r) = all.v_res_str.get(i) {
                out.push_str(r);
            }
            out.push('\n');
        }
        out.push_str(&format!("\nFinal_res: {verdict}\nDetailed result: \n"));
        for (iter, res) in all.v_res.iter().enumerate() {
            out.push_str(&format!(
                "\n\nResult NUM: {iter} \nRESULT FLAGS: {}\n",
                res.comp_res.as_i32()
            ));
            for (j, s) in res.v_res_str.iter().enumerate() {
                out.push_str(&format!("Str: {s} \n"));
                if let Some(i) = res.v_res_int.get(j) {
                    out.push_str(&format!("INT: {i} \n"));
                }
            }
        }
    }
    out.push_str(&format!("Compare_No_Rec_result_int: \n{verdict}\n\n\n\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_blocks() {
        let out = "BEGIN VERI 0 \n3 \nEND VERI 0 \nBEGIN VERI 1 \n3 \nEND VERI 1 \nBEGIN VERI 0 \nERROR: boom\nEND VERI 0 \n";
        // Like the C++, the block keeps its newlines; only spaces are trimmed.
        assert_eq!(
            extract_query_result(out, "BEGIN VERI 0", "END VERI 0"),
            vec!["\n3 \n", "Error"]
        );
        assert_eq!(
            extract_query_result(out, "BEGIN VERI 1", "END VERI 1"),
            vec!["\n3 \n"]
        );
    }

    #[test]
    fn accepts_paired_markers() {
        let out = "-- SQLRIGHT INPUT 0123\nBEGIN VERI 0 \n3 \nEND VERI 0 \nBEGIN VERI 1 \n3 \nEND VERI 1 \n";
        assert!(markers_are_paired(out));
        assert!(markers_are_paired(""));
    }

    #[test]
    fn rejects_a_row_with_the_marker_text() {
        // `SELECT 'END VERI 0' AS a FROM t` on a table with two rows.
        let out = "BEGIN VERI 0 \nEND VERI 0 \nEND VERI 0 \nEND VERI 0 \nBEGIN VERI 1 \n2 \nEND VERI 1 \n";
        assert!(!markers_are_paired(out));
        let out = "BEGIN VERI 0 \nBEGIN VERI 1 x \nEND VERI 0 \n";
        assert!(!markers_are_paired(out));
        let out = "BEGIN VERI 0 \n1 \n";
        assert!(!markers_are_paired(out));
    }

    #[test]
    fn a_row_with_the_marker_text_gives_an_error() {
        struct AlwaysFail;
        impl Oracle for AlwaysFail {
            fn oracle_type(&self) -> &'static str {
                "TEST"
            }
            fn template_select_stmt(&self, _m: &mut sqlright_mutator::Mutator) -> String {
                String::new()
            }
            fn is_oracle_select_stmt(
                &self,
                _t: &mut sqlright_ir::IrTree,
                _stmt: sqlright_ir::IrId,
            ) -> bool {
                false
            }
            fn post_fix_transform_select_stmt(
                &self,
                _m: &mut sqlright_mutator::Mutator,
                _t: &mut sqlright_ir::IrTree,
                _stmt: sqlright_ir::IrId,
                _run: usize,
            ) -> Vec<sqlright_ir::IrTree> {
                Vec::new()
            }
            fn compare_results(&self, res: &mut AllCompRes) {
                res.final_res = Some(OraCompRes::Fail);
            }
        }
        let mut all = AllCompRes {
            res_str: "BEGIN VERI 0 \nEND VERI 0 \nEND VERI 0 \nBEGIN VERI 1 \n1 \nEND VERI 1 \n".into(),
            ..Default::default()
        };
        compare_query_result(&AlwaysFail, &mut all);
        assert_eq!(all.final_res, Some(OraCompRes::Error));
        let mut all = AllCompRes {
            v_res_str: vec!["BEGIN VERI 0 \n1 \nEND VERI 0 \n".into(), "BEGIN VERI 0 \nEND VERI 0 \nEND VERI 0 \n".into()],
            ..Default::default()
        };
        compare_query_results_cross_run(&AlwaysFail, &mut all);
        assert_eq!(all.final_res, Some(OraCompRes::Error));
    }

    #[test]
    fn splits_runs() {
        let runs = split_runs("a\n-- SQLRIGHT RUN 1\nb\nc\n");
        assert_eq!(runs, vec!["a\n", "b\nc\n"]);
    }
}
