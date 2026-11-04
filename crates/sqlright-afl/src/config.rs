//! Configuration of a fuzzing run, read from environment variables because
//! AFL++ loads the mutator without arguments.

use std::path::PathBuf;

/// The variables, with their defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// `SQLRIGHT_ORACLE`: `NOREC`, `TLP` or `INDEX`.
    pub oracle: String,
    /// `SQLRIGHT_INIT_LIB`: directories of SQL files that seed the mutation
    /// library, separated by `:`. The AFL++ input directory is added too
    /// when `SQLRIGHT_SEED_DIR` names it.
    pub init_lib_dirs: Vec<PathBuf>,
    /// `SQLRIGHT_SEED_DIR`: the AFL++ input directory (its files also seed the library).
    pub seed_dir: Option<PathBuf>,
    /// `SQLRIGHT_OUTPUT`: the file the harness writes the results to.
    pub output_path: PathBuf,
    /// `SQLRIGHT_BUG_DIR`: where bug reports go.
    pub bug_dir: PathBuf,
    /// `SQLRIGHT_SELECT_COUNT`: how many oracle SELECTs to append to a test case.
    pub select_count: usize,
    /// `SQLRIGHT_MAX_CASES`: the most test cases made from one seed.
    pub max_cases_per_seed: usize,
    /// `SQLRIGHT_DEBUG`: write a stats file and verbose messages.
    pub debug: bool,
    /// `SQLRIGHT_RNG_SEED`: fixed random seed (default: the seed AFL++ gives).
    pub rng_seed: Option<u64>,
}

impl Config {
    pub fn from_env() -> Config {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Config {
            oracle: var("SQLRIGHT_ORACLE").unwrap_or_else(|| "NOREC".to_string()),
            init_lib_dirs: var("SQLRIGHT_INIT_LIB").map(|v| v.split(':').map(PathBuf::from).collect()).unwrap_or_default(),
            seed_dir: var("SQLRIGHT_SEED_DIR").map(PathBuf::from),
            output_path: PathBuf::from(var("SQLRIGHT_OUTPUT").unwrap_or_else(|| "/tmp/sqlright_output.txt".to_string())),
            bug_dir: PathBuf::from(var("SQLRIGHT_BUG_DIR").unwrap_or_else(|| "sqlright_bugs".to_string())),
            select_count: var("SQLRIGHT_SELECT_COUNT").and_then(|v| v.parse().ok()).unwrap_or(10),
            max_cases_per_seed: var("SQLRIGHT_MAX_CASES").and_then(|v| v.parse().ok()).unwrap_or(2000),
            debug: var("SQLRIGHT_DEBUG").is_some(),
            rng_seed: var("SQLRIGHT_RNG_SEED").and_then(|v| v.parse().ok()),
        }
    }
}
