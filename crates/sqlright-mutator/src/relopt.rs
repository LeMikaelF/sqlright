//! Random PostgreSQL options: storage parameters, settings, and attribute
//! options. Port of `relopt_generator.cpp`.

use sqlright_ir::RelOptionType;

use crate::rng::Rng;

/// A (name, value) option pair and whether the option is a RESET form, which
/// has no value.
pub struct RelOption {
    pub name: String,
    pub value: String,
    pub is_reset: bool,
}

/// Pick a random option of the given kind. Returns `None` for `Unknown`.
pub fn get_rel_option_pair(rng: &mut Rng, option_type: RelOptionType) -> Option<RelOption> {
    match option_type {
        RelOptionType::StorageParameters => {
            let (name, value) = storage_parameter(rng);
            Some(RelOption {
                name,
                value,
                is_reset: false,
            })
        }
        RelOptionType::SetConfigurationOptions => {
            let (name, mut value) = set_configuration_option(rng);
            if rng.rand_int(2) == 1 {
                value = "DEFAULT".to_string();
            }
            Some(RelOption {
                name,
                value,
                is_reset: false,
            })
        }
        RelOptionType::AlterAttribute => {
            let (name, value) = alter_attribute(rng);
            Some(RelOption {
                name,
                value,
                is_reset: false,
            })
        }
        RelOptionType::AlterAttributeReset => {
            let (name, value) = alter_attribute(rng);
            Some(RelOption {
                name,
                value,
                is_reset: true,
            })
        }
        RelOptionType::Unknown => None,
    }
}

fn float_str(f: f64) -> String {
    // The C++ uses std::to_string(float), which prints six decimals.
    format!("{f:.6}")
}

fn choose_str(rng: &mut Rng, items: &[&str]) -> String {
    rng.choose(items).map(|s| s.to_string()).unwrap_or_default()
}

fn choose_i64(rng: &mut Rng, items: &[i64]) -> String {
    rng.choose(items).copied().unwrap_or(0).to_string()
}

fn choose_f64(rng: &mut Rng, items: &[f64]) -> String {
    float_str(rng.choose(items).copied().unwrap_or(0.0))
}

fn storage_parameter(rng: &mut Rng) -> (String, String) {
    let scale_factors = [0.0, 0.00001, 0.01, 0.1, 0.2, 0.5, 0.8, 0.9, 1.0];
    let (name, value) = match rng.rand_int(13) {
        0 => ("fillfactor", rng.rand_range(10, 100).to_string()),
        1 => ("parallel_workers", rng.rand_int(1024).to_string()),
        2 => ("autovacuum_enabled", rng.rand_int(2).to_string()),
        3 => (
            "autovacuum_vacuum_threshold",
            rng.rand_i64(2147483647).to_string(),
        ),
        4 => ("oids", rng.rand_int(2).to_string()),
        5 => (
            "autovacuum_vacuum_scale_factor",
            choose_f64(rng, &scale_factors),
        ),
        6 => (
            "autovacuum_analyze_threshold",
            rng.rand_i64(i32::MAX as i64).to_string(),
        ),
        7 => (
            "autovacuum_analyze_scale_factor",
            choose_f64(rng, &scale_factors),
        ),
        8 => (
            "autovacuum_vacuum_cost_delay",
            rng.rand_int(100).to_string(),
        ),
        9 => (
            "autovacuum_vacuum_cost_limit",
            rng.rand_range(1, 10000).to_string(),
        ),
        10 => (
            "autovacuum_freeze_min_age",
            rng.rand_range(0, 1000000000).to_string(),
        ),
        11 => (
            "autovacuum_freeze_max_age",
            rng.rand_range(100000, 2000000000).to_string(),
        ),
        _ => (
            "autovacuum_freeze_table_age",
            rng.rand_range(0, 2000000000).to_string(),
        ),
    };
    (name.to_string(), value)
}

fn set_configuration_option(rng: &mut Rng) -> (String, String) {
    let costs = [0.0, 0.00001, 0.05, 0.1, 1.0, 10.0, 10000.0];
    let ages = [0, 5, 10, 100, 500, 2000000000];
    let min_ages = [0, 5, 10, 100, 500, 1000000000];
    let jit = [0, i32::MAX as i64];
    let on_off = |rng: &mut Rng| rng.rand_int(2).to_string();
    let (name, value) = match rng.rand_int(66) {
        0 => (
            "synchronous_commit",
            choose_str(rng, &["remote_apply", "remote_write", "local", "off"]),
        ),
        1 => ("wal_compression", on_off(rng)),
        2 => ("commit_delay", rng.rand_int(100000).to_string()),
        3 | 4 => ("commit_siblings", rng.rand_int(1000).to_string()),
        5 => ("commit_siblings", on_off(rng)),
        6 => ("track_counts", on_off(rng)),
        7 => ("track_io_timing", on_off(rng)),
        8 => (
            "track_functions",
            choose_str(rng, &["'none'", "'pl'", "'all'"]),
        ),
        9 => ("vacuum_freeze_table_age", choose_i64(rng, &ages)),
        10 => ("vacuum_multixact_freeze_table_age", choose_i64(rng, &ages)),
        11 => (
            "vacuum_multixact_freeze_min_age",
            choose_i64(rng, &min_ages),
        ),
        12 => (
            "vacuum_cleanup_index_scale_factor",
            choose_f64(
                rng,
                &[
                    0.0,
                    0.0000001,
                    0.00001,
                    0.01,
                    0.1,
                    1.0,
                    10.0,
                    100.0,
                    100000.0,
                    10000000000.0,
                ],
            ),
        ),
        13 => (
            "gin_fuzzy_search_limit",
            rng.rand_i64(2147483647).to_string(),
        ),
        14 => ("default_with_oids", on_off(rng)),
        15 | 16 => ("synchronize_seqscans", on_off(rng)),
        17 => ("enable_bitmapscan", on_off(rng)),
        18 => ("enable_gathermerge", on_off(rng)),
        19 => ("enable_hashjoin", on_off(rng)),
        20 => ("enable_indexscan", on_off(rng)),
        21 => ("enable_indexonlyscan", on_off(rng)),
        22 => ("enable_material", on_off(rng)),
        23 => ("enable_mergejoin", on_off(rng)),
        24 => ("enable_nestloop", on_off(rng)),
        25 => ("enable_parallel_append", on_off(rng)),
        26 => ("enable_parallel_hash", on_off(rng)),
        27 => ("enable_partition_pruning", on_off(rng)),
        28 => ("enable_partitionwise_join", on_off(rng)),
        29 => ("enable_partitionwise_aggregate", on_off(rng)),
        30 => ("enable_seqscan", on_off(rng)),
        31 => ("enable_sort", on_off(rng)),
        32 => ("enable_tidscan", on_off(rng)),
        33 => ("seq_page_cost", choose_f64(rng, &costs)),
        34 => ("random_page_cost", choose_f64(rng, &costs)),
        35 => ("cpu_tuple_cost", choose_f64(rng, &costs)),
        36 => ("cpu_index_tuple_cost", choose_f64(rng, &costs)),
        37 => ("cpu_operator_cost", choose_f64(rng, &costs)),
        38 => (
            "parallel_setup_cost",
            rng.rand_i64(i32::MAX as i64).to_string(),
        ),
        39 => (
            "parallel_tuple_cost",
            rng.rand_i64(i32::MAX as i64).to_string(),
        ),
        40 => (
            "min_parallel_table_scan_size",
            rng.rand_i64(715827882).to_string(),
        ),
        41 => (
            "min_parallel_index_scan_size",
            rng.rand_i64(715827882).to_string(),
        ),
        42 => (
            "min_parallel_index_scan_size",
            rng.rand_i64(i32::MAX as i64).to_string(),
        ),
        43 => (
            "effective_cache_size",
            rng.rand_i64(i32::MAX as i64).to_string(),
        ),
        44 => ("jit_above_cost", choose_i64(rng, &jit)),
        45 => ("jit_inline_above_cost", choose_i64(rng, &jit)),
        46 => ("jit_optimize_above_cost", choose_i64(rng, &jit)),
        47 => ("geqo", on_off(rng)),
        48 => ("geqo_threshold", rng.rand_range(2, 2147483647).to_string()),
        49 => ("geqo_effort", rng.rand_range(1, 10).to_string()),
        50 => ("geqo_pool_size", rng.rand_range(0, 2147483647).to_string()),
        51 | 52 => (
            "geqo_generations",
            rng.rand_range(0, 2147483647).to_string(),
        ),
        53 | 54 => ("geqo_selection_bias", choose_f64(rng, &[1.5, 1.8, 2.0])),
        55 => ("geqo_seed", choose_f64(rng, &[0.0, 0.5, 1.0])),
        56 => (
            "default_statistics_target",
            rng.rand_range(1, 10000).to_string(),
        ),
        57 => (
            "constraint_exclusion",
            choose_str(rng, &["on", "off", "partition"]),
        ),
        58 => (
            "cursor_tuple_fraction",
            choose_f64(rng, &[0.0, 0.1, 0.000001, 1.0, 0.5, 0.9999999]),
        ),
        59 => (
            "from_collapse_limit",
            rng.rand_range(1, i32::MAX as i64).to_string(),
        ),
        60 => ("jit", on_off(rng)),
        61 => (
            "join_collapse_limit",
            rng.rand_range(1, i32::MAX as i64).to_string(),
        ),
        62 => ("parallel_leader_participation", on_off(rng)),
        63 => (
            "force_parallel_mode",
            choose_str(rng, &["off", "on", "regress"]),
        ),
        64 => (
            "plan_cache_mode",
            choose_str(rng, &["auto", "force_generic_plan", "force_custom_plan"]),
        ),
        _ => ("vacuum_freeze_min_age", choose_i64(rng, &min_ages)),
    };
    (name.to_string(), value)
}

fn alter_attribute(rng: &mut Rng) -> (String, String) {
    let values = [
        "-1", "-0.8", "-0.5", "-0.2", "-0.1", "-0.0001", "0", "0.0001", "0.1", "1",
    ];
    let name = if rng.rand_int(2) == 0 {
        "n_distinct_inherited"
    } else {
        "n_distinct"
    };
    (name.to_string(), choose_str(rng, &values))
}
