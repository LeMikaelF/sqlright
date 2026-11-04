//! The SQLRight oracles for PostgreSQL-dialect databases.
//!
//! - [`norec::Norec`]: `SELECT COUNT(*) FROM t WHERE p` must count the same
//!   rows as `SELECT SUM((p)::INT) FROM t`.
//! - [`tlp::Tlp`]: `SELECT ... WHERE p` must equal the union of the results
//!   for `p`, `NOT p` and `p IS NULL`.
//! - [`index::Index`]: adding or removing indexes must not change results.
//!
//! [`result`] turns the text output of the database into comparison input
//! and asks the oracle for the verdict.

pub mod index;
pub mod norec;
pub mod result;
pub mod tlp;

pub use sqlright_mutator::oracle::{AllCompRes, CompRes, OraCompRes, Oracle};

/// Build the oracle named on the command line (`NOREC`, `TLP`, `INDEX`).
pub fn oracle_by_name(name: &str) -> Option<Box<dyn Oracle>> {
    match name.to_ascii_uppercase().as_str() {
        "NOREC" => Some(Box::new(norec::Norec)),
        "TLP" => Some(Box::new(tlp::Tlp)),
        "INDEX" => Some(Box::new(index::Index)),
        _ => None,
    }
}

/// The names `oracle_by_name` accepts.
pub const ORACLE_NAMES: &[&str] = &["NOREC", "TLP", "INDEX"];
