//! Direct libunwind FFI for lock-free stack walking.
//!
//! Bypasses the `backtrace` crate's global mutex by calling `_Unwind_Backtrace`
//! directly. Adapted from backtrace-0.3.76/src/backtrace/libunwind.rs (Apache-2.0/MIT).
//!
//! # Two-phase design
//!
//! Capture and trimming are separated to keep the hot path lock-free:
//!
//! - [`collect_frames_raw`]: called on every yield. Collects all IPs using only
//!   `_Unwind_GetIP` — no global locks.
//! - [`trim_frames`]: called only at emit time (rare, Poisson-sampled). Uses
//!   `_Unwind_FindEnclosingFunction` to locate the root/leaf boundaries and
//!   trims the collected IPs. This calls `dl_iterate_phdr` internally but only
//!   runs on the infrequent emit path.

use core::ffi::c_void;

#[allow(non_camel_case_types, dead_code)]
#[repr(C)]
enum UnwindReasonCode {
    NoReason = 0,
    Failure = 9,
}

#[allow(non_camel_case_types)]
enum UnwindContext {}

type UnwindTraceFn = extern "C" fn(ctx: *mut UnwindContext, arg: *mut c_void) -> UnwindReasonCode;

unsafe extern "C" {
    fn _Unwind_Backtrace(trace: UnwindTraceFn, trace_argument: *mut c_void) -> UnwindReasonCode;
    fn _Unwind_GetIP(ctx: *mut UnwindContext) -> libc::uintptr_t;
    fn _Unwind_FindEnclosingFunction(pc: *mut c_void) -> *mut c_void;
}

// ─── Raw collection (hot path, no locks) ────────────────────────────────────

struct RawCallbackData<'a> {
    frame_ips: &'a mut Vec<u64>,
}

extern "C" fn raw_trace_fn(ctx: *mut UnwindContext, arg: *mut c_void) -> UnwindReasonCode {
    let data = unsafe { &mut *arg.cast::<RawCallbackData<'_>>() };
    let ip = unsafe { _Unwind_GetIP(ctx) } as u64;
    data.frame_ips.push(ip);
    UnwindReasonCode::NoReason
}

/// Collect all instruction pointers on the current call stack.
/// Uses only `_Unwind_GetIP` — no `dl_iterate_phdr`, no global locks.
pub(crate) fn collect_frames_raw(frame_ips: &mut Vec<u64>) {
    let mut data = RawCallbackData { frame_ips };
    unsafe {
        _Unwind_Backtrace(raw_trace_fn, (&raw mut data).cast());
    }
}

// ─── Trimming (emit path, uses FindEnclosingFunction) ───────────────────────

/// Trim a raw IP list to only the frames between `leaf_addr` and `root_addr`.
/// Calls `_Unwind_FindEnclosingFunction` which takes `dl_iterate_phdr`'s lock,
/// but this only runs on the infrequent emit path.
pub(crate) fn trim_frames(
    frame_ips: &[u64],
    root_addr: Option<*const c_void>,
    leaf_addr: *const c_void,
) -> &[u64] {
    // Find the first frame inside the leaf function (start of interesting range).
    let leaf_start = frame_ips.iter().position(|&ip| {
        let sym = unsafe { _Unwind_FindEnclosingFunction(ip as *mut c_void) };
        std::ptr::eq(sym, leaf_addr)
    });

    // Find the first frame inside the root function (end of interesting range).
    let root_end = root_addr.and_then(|root| {
        frame_ips.iter().position(|&ip| {
            let sym = unsafe { _Unwind_FindEnclosingFunction(ip as *mut c_void) };
            std::ptr::eq(sym, root)
        })
    });

    match (leaf_start, root_end) {
        (Some(start), Some(end)) if start + 1 < end => &frame_ips[start + 1..end],
        (Some(start), None) if start + 1 < frame_ips.len() => &frame_ips[start + 1..],
        _ => &[],
    }
}
