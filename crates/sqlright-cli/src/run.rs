//! Start one or more `afl-fuzz` processes with the SQLRight mutator.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::Args;

#[derive(Args, Debug)]
pub struct RunArgs {
    /// The oracle: NOREC, TLP or INDEX.
    #[arg(long, default_value = "NOREC")]
    pub oracle: String,
    /// The harness binary, built with `cargo afl build --release`.
    #[arg(long)]
    pub harness: PathBuf,
    /// The seed directory (afl-fuzz `-i`).
    #[arg(long = "in")]
    pub input: PathBuf,
    /// The output directory (afl-fuzz `-o`). Bug reports go to `<out>/bugs`.
    #[arg(long)]
    pub out: PathBuf,
    /// A directory of SQL files for the mutation library (repeatable).
    /// The seed directory is always part of the library.
    #[arg(long)]
    pub lib: Vec<PathBuf>,
    /// How many afl-fuzz processes to start (one main, the rest secondary).
    #[arg(long, default_value_t = 1)]
    pub jobs: usize,
    /// Stop after this many seconds (afl-fuzz `-V`).
    #[arg(long)]
    pub time: Option<u64>,
    /// The afl-fuzz binary (default: `afl-fuzz` in PATH, then the one cargo-afl installs).
    #[arg(long)]
    pub afl_fuzz: Option<PathBuf>,
    /// The mutator library (default: `libsqlright_afl.so` next to this binary).
    #[arg(long)]
    pub mutator: Option<PathBuf>,
    /// The time limit for one test case, in milliseconds (afl-fuzz `-t`).
    #[arg(long, default_value_t = 20000)]
    pub timeout_ms: u64,
    /// The memory limit of the harness, in MB (afl-fuzz `-m`). 0 means no limit.
    #[arg(long, default_value_t = 4096)]
    pub mem_mb: u64,
    /// How many oracle SELECTs to add to each test case.
    #[arg(long, default_value_t = 10)]
    pub selects: usize,
    /// Show the afl-fuzz status screen (only with one job).
    #[arg(long)]
    pub ui: bool,
    /// Print the commands and do not start them.
    #[arg(long)]
    pub dry_run: bool,
    /// More arguments for afl-fuzz, after `--`.
    #[arg(last = true)]
    pub afl_args: Vec<String>,
}

pub fn run(args: RunArgs) -> Result<()> {
    if !sqlright_oracle::ORACLE_NAMES.contains(&args.oracle.to_ascii_uppercase().as_str()) {
        bail!(
            "unknown oracle {}; use one of {}",
            args.oracle,
            sqlright_oracle::ORACLE_NAMES.join(", ")
        );
    }
    let harness = absolute(&args.harness)?;
    if !harness.is_file() {
        bail!("the harness {} does not exist; build it with `cargo afl build --release` in crates/sqlright-harness-tursopg", harness.display());
    }
    if !args.dry_run {
        check_harness(&harness)?;
    }
    let afl_fuzz = find_afl_fuzz(args.afl_fuzz.as_deref())?;
    let mutator = find_mutator(args.mutator.as_deref())?;
    let input = absolute(&args.input)?;
    let out = absolute(&args.out)?;
    let bug_dir = out.join("bugs");
    let log_dir = out.join("logs");
    if !args.dry_run {
        std::fs::create_dir_all(&bug_dir)?;
        std::fs::create_dir_all(&log_dir)?;
    }
    let mut libs = Vec::new();
    for l in &args.lib {
        libs.push(absolute(l)?.to_string_lossy().into_owned());
    }
    let jobs = args.jobs.max(1);
    let show_ui = args.ui && jobs == 1 && std::io::stdout().is_terminal();

    let mut children = Vec::new();
    for j in 0..jobs {
        let name = if j == 0 {
            "main".to_string()
        } else {
            format!("s{j}")
        };
        let mut cmd = Command::new(&afl_fuzz);
        cmd.arg("-i").arg(&input).arg("-o").arg(&out);
        cmd.arg("-t").arg(args.timeout_ms.to_string());
        if args.mem_mb == 0 {
            cmd.arg("-m").arg("none");
        } else {
            cmd.arg("-m").arg(args.mem_mb.to_string());
        }
        if let Some(t) = args.time {
            cmd.arg("-V").arg(t.to_string());
        }
        if jobs > 1 {
            cmd.arg(if j == 0 { "-M" } else { "-S" }).arg(&name);
        }
        cmd.args(&args.afl_args);
        cmd.arg("--").arg(&harness);
        cmd.env("AFL_CUSTOM_MUTATOR_LIBRARY", &mutator);
        cmd.env("AFL_CUSTOM_MUTATOR_ONLY", "1");
        cmd.env("AFL_DISABLE_TRIM", "1");
        cmd.env("AFL_SKIP_CPUFREQ", "1");
        cmd.env("AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES", "1");
        cmd.env("AFL_NO_AFFINITY", "1");
        cmd.env("AFL_FAST_CAL", "1");
        // Calibrate a seed when it is first picked, not all seeds at start.
        cmd.env("AFL_NO_STARTUP_CALIBRATION", "1");
        cmd.env("AFL_NO_WARN_INSTABILITY", "1");
        cmd.env("AFL_IGNORE_SEED_PROBLEMS", "1");
        // AFL++ 4.40 runs its byte-level "frameshift" analysis even in
        // custom-mutator-only mode; it only makes broken SQL.
        cmd.env("AFL_FRAMESHIFT_DISABLE", "1");
        if !show_ui {
            cmd.env("AFL_NO_UI", "1");
        }
        cmd.env("SQLRIGHT_ORACLE", args.oracle.to_ascii_uppercase());
        cmd.env("SQLRIGHT_SEED_DIR", &input);
        cmd.env("SQLRIGHT_INIT_LIB", libs.join(":"));
        cmd.env("SQLRIGHT_OUTPUT", out.join(format!("output_{name}.txt")));
        cmd.env("SQLRIGHT_BUG_DIR", &bug_dir);
        cmd.env("SQLRIGHT_SELECT_COUNT", args.selects.to_string());
        if args.dry_run || jobs > 1 || !show_ui {
            println!("{}", describe(&cmd));
        }
        if args.dry_run {
            continue;
        }
        if !show_ui {
            let log = std::fs::File::create(log_dir.join(format!("{name}.log")))?;
            cmd.stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("cannot start {}", afl_fuzz.display()))?;
        children.push((name, child));
        if jobs > 1 {
            // The main instance must make the output directory first.
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
    if args.dry_run {
        return Ok(());
    }
    println!(
        "{jobs} afl-fuzz process(es) started; logs in {}, bug reports in {}",
        log_dir.display(),
        bug_dir.display()
    );
    let mut failed = false;
    for (name, mut child) in children {
        let status = child.wait()?;
        if !status.success() {
            failed = true;
            eprintln!(
                "afl-fuzz {name} stopped with {status} (see {})",
                log_dir.join(format!("{name}.log")).display()
            );
        }
    }
    let reports = std::fs::read_dir(&bug_dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| {
                    e.path().extension().map(|x| x == "txt").unwrap_or(false)
                        && e.file_name().to_string_lossy().starts_with("bug_")
                })
                .count()
        })
        .unwrap_or(0);
    println!("{reports} bug report(s) in {}", bug_dir.display());
    if failed {
        bail!("an afl-fuzz process failed");
    }
    Ok(())
}

/// Run the harness once on `SELECT 1` and fail if it does not print a
/// result. `cargo afl build` compiles for the CPU of the build machine
/// (`-C target-cpu=native`), so a harness binary can stop with an illegal
/// instruction on another machine. afl-fuzz then saves every input as a
/// crash and sees no coverage.
fn check_harness(harness: &Path) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("sqlright-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let case_path = dir.join("check.sql");
    std::fs::write(&case_path, "SELECT 1;\n")?;
    let result = Command::new(harness)
        .arg("--replay")
        .arg(&case_path)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot start the harness {}", harness.display()));
    let _ = std::fs::remove_dir_all(&dir);
    let output = result?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !stdout.lines().any(|l| l.trim() == "1") {
        bail!(
            "the harness {} does not run `SELECT 1` ({}). If the status is an illegal instruction, \
             the binary was built for another CPU: rebuild it with \
             `RUSTFLAGS=\"-C target-cpu=x86-64-v3\" cargo afl build --release`",
            harness.display(),
            output.status
        );
    }
    Ok(())
}

fn describe(cmd: &Command) -> String {
    let mut s = String::new();
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            s.push_str(&format!("{}={} ", k.to_string_lossy(), v.to_string_lossy()));
        }
    }
    s.push_str(&cmd.get_program().to_string_lossy());
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn find_afl_fuzz(given: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = given {
        return absolute(p);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("afl-fuzz");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let base = PathBuf::from(home).join(".local/share/afl.rs");
        let direct = base.join("AFLplusplus/afl-fuzz");
        if direct.is_file() {
            return Ok(direct);
        }
        if let Ok(entries) = std::fs::read_dir(&base) {
            for rustc_dir in entries.flatten() {
                if let Ok(versions) = std::fs::read_dir(rustc_dir.path()) {
                    for v in versions.flatten() {
                        let candidate = v.path().join("afl/bin/afl-fuzz");
                        if candidate.is_file() {
                            return Ok(candidate);
                        }
                    }
                }
            }
        }
    }
    bail!("afl-fuzz not found; install AFL++ or cargo-afl (`cargo install cargo-afl && cargo afl config --build`), or pass --afl-fuzz")
}

fn find_mutator(given: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = given {
        return absolute(p);
    }
    let exe = std::env::current_exe()?;
    let candidates = [
        exe.parent().map(|d| d.join("libsqlright_afl.so")),
        Some(PathBuf::from("target/release/libsqlright_afl.so")),
    ];
    for c in candidates.into_iter().flatten() {
        if c.is_file() {
            return absolute(&c);
        }
    }
    bail!("libsqlright_afl.so not found next to the sqlright binary; build it with `cargo build --release` or pass --mutator")
}
