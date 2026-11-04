//! The `sqlright` command. AFL++ does the fuzzing; this command prepares
//! seeds, starts `afl-fuzz` with the SQLRight mutator, and helps with the
//! findings (replay, minimize).

mod harness;
mod minimize;
mod run;
mod seeds;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sqlright_afl::{Config, Engine, TestCase};
use sqlright_mutator::oracle::OraCompRes;
use sqlright_oracle::result::{format_report, split_runs};

#[derive(Parser)]
#[command(name = "sqlright", version, about = "SQLRight: a logic-bug fuzzer for SQL databases, as an AFL++ custom mutator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a SQL file and print the IR of every statement.
    PrintIr {
        /// The SQL file.
        file: PathBuf,
    },
    /// Make test cases from one seed, without AFL++.
    Mutate {
        /// The seed: a SQL file.
        seed: PathBuf,
        /// The oracle: NOREC, TLP or INDEX.
        #[arg(long, default_value = "NOREC")]
        oracle: String,
        /// A directory of SQL files for the mutation library (repeatable).
        #[arg(long)]
        lib: Vec<PathBuf>,
        /// How many test cases to make at most.
        #[arg(long, default_value_t = 20)]
        count: usize,
        /// How many oracle SELECTs to add to each test case.
        #[arg(long, default_value_t = 10)]
        selects: usize,
        /// The random seed.
        #[arg(long)]
        rng_seed: Option<u64>,
        /// Write the test cases as files into this directory instead of printing them.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run a test case on the harness and print the oracle verdict.
    Replay {
        /// The test case: a SQL file with result markers.
        case: PathBuf,
        /// The oracle: NOREC, TLP or INDEX.
        #[arg(long, default_value = "NOREC")]
        oracle: String,
        /// The harness binary.
        #[arg(long)]
        harness: PathBuf,
        /// Print the harness output too.
        #[arg(long)]
        raw: bool,
        /// Kill the harness after this many seconds.
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
    },
    /// Shrink a failing test case while the oracle still fails.
    Minimize {
        /// The test case: a SQL file with result markers.
        case: PathBuf,
        /// The oracle: NOREC, TLP or INDEX.
        #[arg(long, default_value = "NOREC")]
        oracle: String,
        /// The harness binary.
        #[arg(long)]
        harness: PathBuf,
        /// Write the small test case here (default: print it).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Stop after this many harness runs.
        #[arg(long, default_value_t = 400)]
        max_runs: usize,
        /// Kill the harness after this many seconds.
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
    },
    /// Build a seed directory from SQL files and .sqltest files.
    Seeds {
        /// A file, or a directory of .sql and .sqltest files (repeatable).
        #[arg(long, required = true)]
        from: Vec<PathBuf>,
        /// The seed directory to write.
        #[arg(long)]
        out: PathBuf,
        /// Split a file into seeds of at most this many statements.
        #[arg(long, default_value_t = 30)]
        max_stmts: usize,
    },
    /// Start afl-fuzz with the SQLRight mutator on the harness.
    Run(run::RunArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::PrintIr { file } => print_ir(&file),
        Command::Mutate { seed, oracle, lib, count, selects, rng_seed, out } => mutate(&seed, &oracle, &lib, count, selects, rng_seed, out.as_deref()),
        Command::Replay { case, oracle, harness, raw, timeout_secs } => replay(&case, &oracle, &harness, raw, Duration::from_secs(timeout_secs)),
        Command::Minimize { case, oracle, harness, out, max_runs, timeout_secs } => {
            minimize_case(&case, &oracle, &harness, out.as_deref(), max_runs, Duration::from_secs(timeout_secs))
        }
        Command::Seeds { from, out, max_stmts } => seeds::build(&from, &out, max_stmts),
        Command::Run(args) => run::run(args),
    }
}

fn print_ir(file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    match sqlright_pg_parser::parse_query(&text) {
        Some(q) => {
            print!("{}", q.tree.dump(q.root));
            println!("--- {}", q.tree.to_string(q.root));
            Ok(())
        }
        None => {
            // Parse statement by statement so the reader sees which one fails.
            for stmt in seeds::split_sql(&text) {
                println!("=== {stmt}");
                match sqlright_pg_parser::parse_query(&stmt) {
                    Some(q) => print!("{}", q.tree.dump(q.root)),
                    None => println!("PARSE_FAILED"),
                }
            }
            bail!("the file does not parse as a whole");
        }
    }
}

/// An engine for the command line: the bug directory is a temporary
/// directory because the commands print their findings instead.
fn make_engine(oracle: &str, libs: &[PathBuf], rng_seed: Option<u64>, select_count: usize) -> Result<Engine> {
    let scratch = std::env::temp_dir().join(format!("sqlright-cli-{}", std::process::id()));
    let config = Config {
        oracle: oracle.to_string(),
        init_lib_dirs: libs.to_vec(),
        seed_dir: None,
        output_path: scratch.join("output.txt"),
        bug_dir: scratch,
        select_count,
        max_cases_per_seed: usize::MAX,
        debug: std::env::var_os("SQLRIGHT_DEBUG").is_some(),
        rng_seed,
    };
    Engine::new(config, 1).map_err(|e| anyhow::anyhow!(e))
}

/// A test case from the text of a case file: the runs between the run markers.
fn case_from_text(text: &str) -> TestCase {
    let runs = split_runs(text);
    TestCase { marked_runs: runs.clone(), unmarked_runs: runs }
}

fn mutate(seed: &Path, oracle: &str, libs: &[PathBuf], count: usize, selects: usize, rng_seed: Option<u64>, out: Option<&Path>) -> Result<()> {
    let text = std::fs::read_to_string(seed).with_context(|| format!("cannot read {}", seed.display()))?;
    let mut engine = make_engine(oracle, libs, rng_seed, selects)?;
    engine.config.max_cases_per_seed = count;
    if libs.is_empty() {
        // Without a library, the seed itself is the library.
        for line in seeds::split_sql(&text) {
            engine.mutator.init_from_line(engine.oracle.as_ref(), &line);
        }
    }
    let cases = engine.generate_cases(&text);
    if cases.is_empty() {
        bail!("no test case could be made from {} (does it parse? run print-ir)", seed.display());
    }
    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        for (i, case) in cases.iter().enumerate() {
            std::fs::write(dir.join(format!("case_{i:05}.sql")), case.to_input())?;
        }
        eprintln!("{} test cases written to {}", cases.len(), dir.display());
    } else {
        for (i, case) in cases.iter().enumerate() {
            println!("-- case {i}");
            print!("{}", case.to_input());
            println!();
        }
    }
    Ok(())
}

fn replay(case: &Path, oracle: &str, harness_bin: &Path, raw: bool, timeout: Duration) -> Result<()> {
    let text = std::fs::read_to_string(case).with_context(|| format!("cannot read {}", case.display()))?;
    let engine = make_engine(oracle, &[], None, 0)?;
    let tc = case_from_text(&text);
    let output = harness::run(harness_bin, &text, timeout)?;
    if raw {
        println!("--- harness output");
        print!("{output}");
        println!("--- end of harness output");
    }
    let (verdict, all) = engine.compare(&tc, &output);
    print!("{}", format_report(engine.oracle.as_ref(), &all));
    println!("verdict: {}", verdict_name(verdict));
    Ok(())
}

fn verdict_name(v: OraCompRes) -> &'static str {
    match v {
        OraCompRes::Pass => "PASS (the results agree)",
        OraCompRes::Fail => "FAIL (the results differ: a possible logic bug)",
        OraCompRes::Error => "ERROR (a statement failed or the case is not deterministic; nothing compared)",
    }
}

fn minimize_case(case: &Path, oracle: &str, harness_bin: &Path, out: Option<&Path>, max_runs: usize, timeout: Duration) -> Result<()> {
    let text = std::fs::read_to_string(case).with_context(|| format!("cannot read {}", case.display()))?;
    let engine = make_engine(oracle, &[], None, 0)?;
    let small = minimize::minimize(&engine, harness_bin, &text, max_runs, timeout)?;
    match out {
        Some(path) => {
            std::fs::write(path, &small).with_context(|| format!("cannot write {}", path.display()))?;
            eprintln!("written to {}", path.display());
        }
        None => print!("{small}"),
    }
    Ok(())
}
