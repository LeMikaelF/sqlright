//! The AFL++ custom mutator C interface
//! (<https://aflplus.plus/docs/custom_mutators/>).
//!
//! AFL++ loads this library with `AFL_CUSTOM_MUTATOR_LIBRARY` and calls:
//!
//! - `afl_custom_init` once, with a seed.
//! - `afl_custom_queue_get` when it picks a queue entry.
//! - `afl_custom_fuzz_count` with the entry: we make all test cases of the
//!   entry and return how many there are.
//! - `afl_custom_fuzz` that many times: we return the next test case.
//! - `afl_custom_post_run` after every run of the target: we read the
//!   harness output and apply the oracle.
//! - `afl_custom_queue_new_entry` when it saves a new entry: we add it to
//!   the library.
//!
//! Panics never cross into AFL++: every entry point catches them and aborts.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::config::Config;
use crate::engine::Engine;

/// The state AFL++ hands back to every call.
struct State {
    engine: Engine,
    /// Holds the bytes of the last test case returned by `afl_custom_fuzz`.
    out_buf: Vec<u8>,
    fuzz_calls: u64,
}

fn abort_on_panic<T>(what: &str, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            eprintln!("[sqlright] panic in {what}: {msg}");
            std::process::abort();
        }
    }
}

unsafe fn state<'a>(data: *mut c_void) -> &'a mut State {
    assert!(!data.is_null(), "AFL++ passed a null state");
    &mut *(data as *mut State)
}

unsafe fn bytes<'a>(buf: *const u8, size: usize) -> &'a [u8] {
    if buf.is_null() || size == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(buf, size)
    }
}

unsafe fn c_str_to_path(p: *const c_char) -> Option<std::path::PathBuf> {
    if p.is_null() {
        return None;
    }
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    Some(std::path::PathBuf::from(s))
}

#[no_mangle]
pub extern "C" fn afl_custom_init(_afl: *mut c_void, seed: c_uint) -> *mut c_void {
    abort_on_panic("afl_custom_init", || {
        let config = Config::from_env();
        let engine = match Engine::new(config, seed as u64) {
            Ok(e) => e,
            Err(msg) => {
                eprintln!("[sqlright] cannot start: {msg}");
                std::process::abort();
            }
        };
        engine.log("initialized");
        engine.write_stats();
        let state = Box::new(State {
            engine,
            out_buf: Vec::new(),
            fuzz_calls: 0,
        });
        Box::into_raw(state) as *mut c_void
    })
}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_deinit(data: *mut c_void) {
    abort_on_panic("afl_custom_deinit", || {
        if !data.is_null() {
            let state = Box::from_raw(data as *mut State);
            state.engine.write_stats();
        }
    })
}

/// Tell AFL++ that splicing with other queue entries makes no sense for SQL.
#[no_mangle]
pub extern "C" fn afl_custom_splice_optout(_data: *mut c_void) {}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_queue_get(_data: *mut c_void, _filename: *const c_char) -> u8 {
    1
}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_fuzz_count(
    data: *mut c_void,
    buf: *const u8,
    buf_size: usize,
) -> c_uint {
    abort_on_panic("afl_custom_fuzz_count", || {
        let st = state(data);
        let seed = String::from_utf8_lossy(bytes(buf, buf_size)).into_owned();
        let n = st.engine.prepare(&seed);
        if st.engine.config.debug {
            st.engine
                .log(&format!("{n} test cases for a seed of {buf_size} bytes"));
        }
        n as c_uint
    })
}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_fuzz(
    data: *mut c_void,
    buf: *mut u8,
    _buf_size: usize,
    out_buf: *mut *mut u8,
    _add_buf: *mut u8,
    _add_buf_size: usize,
    max_size: usize,
) -> usize {
    abort_on_panic("afl_custom_fuzz", || {
        let st = state(data);
        st.fuzz_calls += 1;
        if st.fuzz_calls % 200 == 0 {
            st.engine.write_stats();
        }
        loop {
            let Some(case) = st.engine.next_case() else {
                // Nothing left: tell AFL++ to skip this attempt.
                *out_buf = buf;
                return 0;
            };
            let text = case.to_input();
            if text.len() > max_size {
                continue;
            }
            st.out_buf.clear();
            st.out_buf.extend_from_slice(text.as_bytes());
            *out_buf = st.out_buf.as_mut_ptr();
            return st.out_buf.len();
        }
    })
}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_post_run(data: *mut c_void) {
    abort_on_panic("afl_custom_post_run", || {
        let st = state(data);
        let output = std::fs::read_to_string(&st.engine.config.output_path).unwrap_or_default();
        if let Some(verdict) = st.engine.check_last_output(&output) {
            if let Some(path) = verdict.report_path {
                st.engine
                    .log(&format!("oracle mismatch: {}", path.display()));
            }
        }
    })
}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_queue_new_entry(
    data: *mut c_void,
    filename_new_queue: *const c_char,
    _filename_orig_queue: *const c_char,
) -> u8 {
    abort_on_panic("afl_custom_queue_new_entry", || {
        let st = state(data);
        if let Some(path) = c_str_to_path(filename_new_queue) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                st.engine.add_queue_entry(&text);
            }
        }
        0
    })
}

static DESCRIPTION: &[u8] = b"sqlright\0";

#[no_mangle]
pub extern "C" fn afl_custom_describe(
    _data: *mut c_void,
    _max_description_len: usize,
) -> *const c_char {
    DESCRIPTION.as_ptr() as *const c_char
}

/// No trimming: a SQL test case cannot lose bytes and stay valid.
/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_init_trim(
    _data: *mut c_void,
    _buf: *mut u8,
    _buf_size: usize,
) -> c_int {
    0
}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_trim(_data: *mut c_void, out_buf: *mut *mut u8) -> usize {
    if !out_buf.is_null() {
        *out_buf = std::ptr::null_mut();
    }
    0
}

/// # Safety
///
/// Called by AFL++ with the state pointer that `afl_custom_init` returned
/// and with buffers that stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn afl_custom_post_trim(_data: *mut c_void, _success: u8) -> c_int {
    0
}
