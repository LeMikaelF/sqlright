//! The SQLRight oracles for PostgreSQL-dialect databases.
//!
//! - [`norec::Norec`]: `SELECT COUNT(*) FROM t WHERE p` must count the same
//!   rows as `SELECT SUM((p)::INT) FROM t`.
//! - [`tlp::Tlp`]: `SELECT ... WHERE p` must equal the union of the results
//!   for `p`, `NOT p` and `p IS NULL`.
//! - [`index::Index`]: adding or removing indexes must not change results.
//! - [`eet::Eet`]: a predicate keeps its rows when it is wrapped in a form
//!   the optimizer cannot simplify (`CASE WHEN true_expr THEN p ELSE NULL END`).
//! - [`dqe::Dqe`]: `SELECT`, `UPDATE ... RETURNING *` and
//!   `DELETE ... RETURNING *` with the same WHERE clause touch the same rows.
//! - [`lateral::Lateral`]: a LATERAL join returns the same rows as its
//!   JSON form (`json_to_recordset` over `json_group_array`).
//!
//! [`shape`] has the tests on the shape of a SELECT and the row comparison
//! that the last three share.
//! [`result`] turns the text output of the database into comparison input
//! and asks the oracle for the verdict.

pub mod dqe;
pub mod eet;
pub mod index;
pub mod lateral;
pub mod norec;
pub mod result;
pub mod shape;
pub mod tlp;

pub use sqlright_mutator::oracle::{AllCompRes, CompRes, OraCompRes, Oracle};

/// Build the oracle named on the command line (one of [`ORACLE_NAMES`]).
pub fn oracle_by_name(name: &str) -> Option<Box<dyn Oracle>> {
    match name.to_ascii_uppercase().as_str() {
        "NOREC" => Some(Box::new(norec::Norec)),
        "TLP" => Some(Box::new(tlp::Tlp)),
        "INDEX" => Some(Box::new(index::Index)),
        "EET" => Some(Box::new(eet::Eet)),
        "DQE" => Some(Box::new(dqe::Dqe)),
        "LATERAL" => Some(Box::new(lateral::Lateral)),
        _ => None,
    }
}

/// The names `oracle_by_name` accepts.
pub const ORACLE_NAMES: &[&str] = &["NOREC", "TLP", "INDEX", "EET", "DQE", "LATERAL"];
