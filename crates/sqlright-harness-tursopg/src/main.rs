//! AFL++ target: run a SQLRight test case on a fresh in-memory tursopg
//! database and write the results as text.
//!
//! The test case is a list of SQL statements. The line `-- SQLRIGHT RUN n`
//! starts a new run on a fresh database (multi-run oracles). The output has
//! one line per result row (values separated by spaces, NULLs left out),
//! one `ERROR:  message` line per failed statement, and the run marker line
//! repeated at the start of each run after the first. This is the format
//! the C++ SQLRight read from PostgreSQL, so the oracles compare it the
//! same way.
//!
//! Modes:
//! - `sqlright-harness-tursopg --replay <file>`: print the output to stdout.
//! - no arguments: AFL++ persistent mode. The output goes to the file named
//!   by `SQLRIGHT_OUTPUT` (the custom mutator reads it after each run).

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use turso_core::{DatabaseOpts, OpenFlags, Value};
use turso_pg::Connection;

/// The line that separates runs. Must match `sqlright_oracle::result::RUN_MARKER`.
const RUN_MARKER: &str = "-- SQLRIGHT RUN";

/// Statements that take longer than this are stopped and reported as errors.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(2);

/// The first output line: this marker and the FNV-1a hash of the input.
/// Must match `sqlright_afl::engine::INPUT_MARKER`.
const INPUT_MARKER: &str = "-- SQLRIGHT INPUT";

/// 64-bit FNV-1a. Must match `sqlright_afl::engine::fnv1a`.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--replay" {
        let data = std::fs::read(&args[2]).expect("read the test case");
        let mut out = Vec::new();
        run_case(&data, &mut out);
        std::io::stdout().write_all(&out).expect("write stdout");
        return;
    }
    if args.len() >= 2 && args[1] == "--help" {
        eprintln!("usage: sqlright-harness-tursopg [--replay <file>]");
        return;
    }
    let out_path = std::env::var("SQLRIGHT_OUTPUT").unwrap_or_else(|_| "/tmp/sqlright_output.txt".to_string());
    afl::fuzz!(|data: &[u8]| {
        let mut out = Vec::with_capacity(4096);
        run_case(data, &mut out);
        // The mutator reads this file in its post_run hook.
        if let Err(e) = std::fs::write(&out_path, &out) {
            eprintln!("sqlright-harness-tursopg: cannot write {out_path}: {e}");
        }
    });
}

/// Run every run of the test case, each on a fresh database. The first
/// output line names the hash of the input, so the mutator can tell that
/// the output belongs to the test case it sent.
fn run_case(data: &[u8], out: &mut Vec<u8>) {
    let _ = writeln!(out, "{INPUT_MARKER} {:016x}", fnv1a(data));
    let text = String::from_utf8_lossy(data);
    for (run_idx, run_text) in split_runs(&text).iter().enumerate() {
        if run_idx > 0 {
            let _ = writeln!(out, "{RUN_MARKER} {run_idx}");
        }
        run_statements(run_text, out);
    }
}

/// The runs of a test case: the text between the run marker lines.
fn split_runs(text: &str) -> Vec<String> {
    let mut runs = vec![String::new()];
    for line in text.lines() {
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

fn open_memory_database() -> turso_pg::Result<(Arc<dyn turso_core::IO>, Arc<turso_core::Database>)> {
    let opts = DatabaseOpts::new().with_views(true).with_custom_types(true).with_index_method(true).with_autovacuum(true);
    turso_pg::open_database(":memory:", None, OpenFlags::default(), opts)
}

/// Run the statements of one run, in order. An error does not stop the run.
fn run_statements(sql: &str, out: &mut Vec<u8>) {
    let (_io, db) = match open_memory_database() {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(out, "ERROR:  cannot open the database: {e}");
            return;
        }
    };
    let conn = match Connection::connect(&db) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "ERROR:  cannot connect: {e}");
            return;
        }
    };
    let statements = turso_pg::split_statements(sql).unwrap_or_else(|_| vec![sql.trim().to_string()]);
    for stmt in statements {
        if stmt.trim().is_empty() {
            continue;
        }
        run_one(&conn, &stmt, out);
    }
    let _ = conn.close();
}

/// Run one statement and append its rows or its error.
fn run_one(conn: &Connection, sql: &str, out: &mut Vec<u8>) {
    match conn.query(sql) {
        Ok(Some(mut stmt)) => {
            stmt.set_query_timeout_override(Some(Some(STATEMENT_TIMEOUT)));
            let num_columns = stmt.num_columns();
            let result = stmt.run_with_row_callback(|row| {
                if num_columns > 0 {
                    for value in row.get_values() {
                        if !matches!(value, Value::Null) {
                            let _ = write!(out, "{value} ");
                        }
                    }
                    out.push(b'\n');
                }
                Ok(())
            });
            if let Err(e) = result {
                write_error(out, &e);
            }
        }
        Ok(None) => {}
        Err(e) => write_error(out, &e),
    }
}

fn write_error(out: &mut Vec<u8>, error: &turso_pg::Error) {
    // Errors never contain a newline so that one line is one result row.
    let message = error.message.replace('\n', " ");
    let _ = writeln!(out, "ERROR:  {message}");
}
