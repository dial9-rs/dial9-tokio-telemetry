//! Direct libunwind FFI for lock-free stack walking.
//!
//! Bypasses the `backtrace` crate's global mutex by calling `_Unwind_Backtrace`
//! directly. Adapted from backtrace-0.3.76/src/backtrace/libunwind.rs (Apache-2.0/MIT).

use core::ffi::c_void;

#[allow(non_camel_case_types)]
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

struct CallbackData<'a> {
    frame_ips: &'a mut Vec<u64>,
    above_leaf: bool,
    root_addr: Option<*const c_void>,
    leaf_addr: *const c_void,
}

extern "C" fn trace_fn(ctx: *mut UnwindContext, arg: *mut c_void) -> UnwindReasonCode {
    let data = unsafe { &mut *arg.cast::<CallbackData<'_>>() };
    let ip = unsafe { _Unwind_GetIP(ctx) } as *mut c_void;
    let symbol_address = unsafe { _Unwind_FindEnclosingFunction(ip) };

    let below_root = data
        .root_addr
        .is_none_or(|root| !std::ptr::eq(symbol_address, root));

    if data.above_leaf && below_root {
        data.frame_ips.push(ip as u64);
    }

    if std::ptr::eq(symbol_address, data.leaf_addr) {
        data.above_leaf = true;
    }

    if below_root {
        UnwindReasonCode::NoReason
    } else {
        UnwindReasonCode::Failure
    }
}

/// Walk the call stack, collecting instruction pointers between `leaf_addr` and
/// `root_addr`. This calls `_Unwind_Backtrace` directly without any global lock.
pub(crate) fn collect_frames(
    frame_ips: &mut Vec<u64>,
    root_addr: Option<*const c_void>,
    leaf_addr: *const c_void,
) {
    let mut data = CallbackData {
        frame_ips,
        above_leaf: false,
        root_addr,
        leaf_addr,
    };
    unsafe {
        _Unwind_Backtrace(trace_fn, (&raw mut data).cast());
    }
}
