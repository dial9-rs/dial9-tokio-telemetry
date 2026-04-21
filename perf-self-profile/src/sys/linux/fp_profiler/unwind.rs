//! Frame-pointer stack unwinder using `load` (safe_load) for fault-tolerant reads.
//!
//! Walks the rbp/x29 chain. Each frame on a System V AMD64 / AAPCS64 stack
//! looks like:
//!
//!   x86_64:                 aarch64:
//!   +----------------+      +----------------+
//!   | saved rbp      | <-fp | saved fp (x29) |  <- *fp
//!   +----------------+      +----------------+
//!   | return addr    |      | return addr    |  <- *(fp + 8)
//!   +----------------+      +----------------+
//!   | locals...      |      | locals...      |
//!
//! On x86_64, *fp is the caller's saved rbp and *(fp+8) is the return address.
//! On aarch64, *fp is the caller's saved fp (x29) and *(fp+8) is the LR.
//!
//! Either way: dereference fp to get the next fp, and fp+8 to get the return
//! address. Both reads go through safe_access::load so a corrupted chain
//! aborts the walk instead of crashing.

use super::{SAFE_LOAD_FAULT, load};

// Cap on frames per sample; prevents runaway walks on corrupted FP chains.
pub const MAX_FRAMES: usize = 128;

// Minimum distance from address-space edges for a plausible return address.
const DEAD_ZONE: usize = 0x1000;

/// Maximum plausible single-frame advance of the frame pointer (256 KiB).
/// Rejects wild pointers that happen to be above fp but aren't real frames.
const MAX_FRAME_SIZE: usize = 0x40000;

#[derive(Copy, Clone, Debug)]
pub struct Frame {
    pub pc: usize,
}

/// Strip pointer authentication (PAC) bits from a return address.
///
/// On ARMv8.3+ with PAC, the kernel signs return addresses in upper bits.
/// Safe to apply unconditionally, on non-PAC systems the upper bits are zero.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn strip_pac(addr: usize) -> usize {
    addr & 0x0000_FFFF_FFFF_FFFF
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn strip_pac(addr: usize) -> usize {
    addr
}

/// Walk the frame-pointer chain starting from the given (pc, fp, sp) triple,
/// usually obtained from a signal handler's ucontext.
///
/// # Safety
/// - `install_handler` must have been called.
/// - Should generally be called from a signal handler where the target thread
///   is stopped; walking a running thread's stack races with mutations.
pub unsafe fn unwind(pc: usize, mut fp: usize, sp: usize, out: &mut [Frame]) -> usize {
    let limit = out.len().min(MAX_FRAMES);
    if limit == 0 {
        return 0;
    }

    out[0] = Frame { pc };
    let mut n = 1;

    let stack_lo = sp;
    let stack_hi = sp.saturating_add(8 * 1024 * 1024);

    while n < limit {
        if fp < stack_lo || fp >= stack_hi {
            break;
        }
        if fp & (core::mem::size_of::<usize>() - 1) != 0 {
            break; // misaligned
        }

        let saved_fp = unsafe { load(fp as *const usize) };
        if saved_fp == SAFE_LOAD_FAULT {
            break;
        }
        let ret_addr_slot = (fp + core::mem::size_of::<usize>()) as *const usize;
        let ret_addr = strip_pac(unsafe { load(ret_addr_slot) });
        if ret_addr == SAFE_LOAD_FAULT {
            break;
        }

        if !(DEAD_ZONE..=usize::MAX - DEAD_ZONE).contains(&ret_addr) {
            break;
        }

        // Frame pointer must advance (stacks grow down -> saved_fp > fp)
        // but not by more than MAX_FRAME_SIZE.
        if saved_fp <= fp || saved_fp - fp > MAX_FRAME_SIZE {
            break;
        }

        out[n] = Frame { pc: ret_addr };
        n += 1;
        fp = saved_fp;
    }

    n
}

/// Unwind from inside a signal handler given the raw ucontext.
///
/// # Safety
/// `ucontext` must be the pointer the kernel passed to a SA_SIGINFO handler.
pub unsafe fn unwind_from_ucontext(ucontext: *mut libc::c_void, out: &mut [Frame]) -> usize {
    let (pc, fp, sp) = unsafe { read_pc_fp_sp(ucontext) };
    unsafe { unwind(pc, fp, sp, out) }
}

#[cfg(target_arch = "x86_64")]
unsafe fn read_pc_fp_sp(uc: *mut libc::c_void) -> (usize, usize, usize) {
    let uc = uc as *mut libc::ucontext_t;
    unsafe {
        let g = &(*uc).uc_mcontext.gregs;
        (
            g[libc::REG_RIP as usize] as usize,
            g[libc::REG_RBP as usize] as usize,
            g[libc::REG_RSP as usize] as usize,
        )
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn read_pc_fp_sp(uc: *mut libc::c_void) -> (usize, usize, usize) {
    let uc = uc as *mut libc::ucontext_t;
    unsafe {
        let m = &(*uc).uc_mcontext;
        (
            m.pc as usize,
            m.regs[29] as usize, // x29 is the frame pointer
            m.sp as usize,
        )
    }
}

#[cfg(test)]
#[allow(unused_assignments)] // values read through safe_load asm, invisible to compiler
mod tests {
    use super::*;

    fn install() {
        unsafe { crate::sys::fp_profiler::install_handler().unwrap() };
    }

    #[test]
    fn walks_valid_frame_chain() {
        install();
        let sz = std::mem::size_of::<usize>();
        let mut stack = [0usize; 8];
        let base = stack.as_mut_ptr() as usize;

        // 2-frame chain: frame A (0,1) -> frame B (2,3), then stop at frame C (4)
        // because saved_fp == fp.
        stack[0] = base + 2 * sz;
        stack[1] = 0x40_1000;
        stack[2] = base + 4 * sz;
        stack[3] = 0x40_2000;
        stack[4] = base + 4 * sz;

        let mut out = [Frame { pc: 0 }; MAX_FRAMES];
        let n = unsafe { unwind(0x40_0000, base, base, &mut out) };

        assert_eq!(n, 3);
        assert_eq!(out[0].pc, 0x40_0000); // interrupted PC
        assert_eq!(out[1].pc, 0x40_1000);
        assert_eq!(out[2].pc, 0x40_2000);
    }

    #[test]
    fn frame_zero_is_always_the_interrupted_pc() {
        install();
        let mut out = [Frame { pc: 0 }; MAX_FRAMES];
        let n = unsafe { unwind(0xDEAD, 0, 0x1000, &mut out) };
        assert_eq!(n, 1);
        assert_eq!(out[0].pc, 0xDEAD);
    }

    #[test]
    fn stops_at_misaligned_fp() {
        install();
        let mut out = [Frame { pc: 0 }; MAX_FRAMES];
        let sp = 0x7fff_0000_0000usize;
        let n = unsafe { unwind(0x40_0000, sp + 1, sp, &mut out) };
        assert_eq!(n, 1);
    }

    #[test]
    fn stops_when_fp_below_stack() {
        install();
        let mut out = [Frame { pc: 0 }; MAX_FRAMES];
        let sp = 0x7fff_0000_0000usize;
        let n = unsafe { unwind(0x40_0000, sp.wrapping_sub(8), sp, &mut out) };
        assert_eq!(n, 1);
    }

    #[test]
    fn stops_at_dead_zone_return_addr() {
        install();
        let sz = std::mem::size_of::<usize>();
        let mut stack = [0usize; 4];
        let base = stack.as_mut_ptr() as usize;

        stack[0] = base + 2 * sz; // saved_fp advances
        stack[1] = 0x100; // return addr in dead zone (< 0x1000)

        let mut out = [Frame { pc: 0 }; MAX_FRAMES];
        let n = unsafe { unwind(0x40_0000, base, base, &mut out) };
        assert_eq!(n, 1);
    }

    #[test]
    fn stops_when_frame_size_exceeds_max() {
        install();
        let mut stack = [0usize; 4];
        let base = stack.as_mut_ptr() as usize;

        stack[0] = base + MAX_FRAME_SIZE + 8; // jump exceeds MAX_FRAME_SIZE

        let mut out = [Frame { pc: 0 }; MAX_FRAMES];
        let n = unsafe { unwind(0x40_0000, base, base, &mut out) };
        assert_eq!(n, 1);
    }

    #[test]
    fn stops_when_fp_doesnt_advance() {
        install();
        let mut stack = [0usize; 4];
        let base = stack.as_mut_ptr() as usize;

        stack[0] = base; // saved_fp == fp, doesn't advance

        let mut out = [Frame { pc: 0 }; MAX_FRAMES];
        let n = unsafe { unwind(0x40_0000, base, base, &mut out) };
        assert_eq!(n, 1);
    }

    #[test]
    fn respects_output_buffer_limit() {
        install();
        let mut out = [Frame { pc: 0 }; 1];
        let n = unsafe { unwind(0x40_0000, 0, 0x1000, &mut out) };
        assert_eq!(n, 1);
        assert_eq!(out[0].pc, 0x40_0000);
    }
}
