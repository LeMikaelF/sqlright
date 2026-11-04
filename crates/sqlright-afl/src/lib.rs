//! SQLRight as an AFL++ custom mutator.
//!
//! AFL++ owns the queue, the coverage map and the scheduling. This library
//! owns what SQLRight adds: parsing, IR mutation, instantiation, the oracle
//! rewrites, and the result comparison.
//!
//! - [`engine`]: the fuzz loop of `afl-fuzz.cpp` (`fuzz_one`) as a
//!   generator of test cases from one seed, and the result check.
//! - [`ffi`]: the `afl_custom_*` C functions AFL++ loads from the shared
//!   library (`AFL_CUSTOM_MUTATOR_LIBRARY`).
//! - [`config`]: the environment variables that configure a run.
//!
//! The target is `sqlright-harness-tursopg`. It writes the result rows of
//! every test case to the file named by `SQLRIGHT_OUTPUT`; the mutator
//! reads that file in `afl_custom_post_run` and applies the oracle.

pub mod config;
pub mod engine;
pub mod ffi;

pub use config::Config;
pub use engine::{Engine, TestCase, Verdict};
