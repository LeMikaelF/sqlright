//! Dump the IR of every line of a SQL file. The output format is the one of
//! the C++ `ir_dump` tool, so the two parsers can be compared line by line.

use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: pg-ir-dump <sql-file>");
        std::process::exit(1);
    }
    let file = std::fs::File::open(&args[1]).expect("open input file");
    let reader = io::BufReader::new(file);
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = sqlright_ir::util::trimmed(&line);
        if line.is_empty() {
            continue;
        }
        writeln!(out, "=== {line}").unwrap();
        match sqlright_pg_parser::parse_query(&line) {
            Some(q) => {
                write!(out, "{}", q.tree.dump(q.root)).unwrap();
                writeln!(out, "--- {}", q.tree.to_string(q.root)).unwrap();
            }
            None => writeln!(out, "PARSE_FAILED").unwrap(),
        }
    }
}
