//! The SQLRight mutation engine.
//!
//! This crate is a port of `mutate.cpp`, `ir_wrapper.cpp` and
//! `relopt_generator.cpp` from the C++ SQLRight, plus the oracle interface
//! (`postgres_oracle.h`). The fuzz loop that drives it lives in the
//! `sqlright-afl` crate.
//!
//! Modules:
//! - [`ir_wrapper`]: read and edit statement lists and search IR trees.
//! - [`mutator`]: the [`Mutator`]: libraries of seen IR nodes, the three
//!   mutation strategies, and the pipeline that turns a mutated tree into
//!   query strings.
//! - [`fix`]: context-based instantiation. It fills in table, column, index
//!   and alias names so that each statement fits the statements before it.
//! - [`oracle`]: the [`Oracle`] trait and the shared result types.
//! - [`relopt`]: random PostgreSQL storage parameters and settings.
//! - [`rng`]: the random number helpers of the C++ `utils.h`.

pub mod fix;
pub mod ir_wrapper;
pub mod mutator;
pub mod oracle;
pub mod relopt;
pub mod rng;

pub use mutator::{Mutator, StmtType};
pub use oracle::{AllCompRes, CompRes, OraCompRes, Oracle};
pub use rng::Rng;
