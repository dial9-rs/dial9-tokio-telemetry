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

/// Strip pointer authentication (PAC) bits from a return address.
///
/// On ARMv8.3+ with PAC, the kernel signs return addresses in upper bits.
/// Safe to apply unconditionally, on non-PAC systems the upper bits are zero.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn strip_pac(addr: usize) -> usize {
    addr & 0x0000_FFFF_FFFF_FFFF
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn strip_pac(addr: usize) -> usize {
    addr
}

/// Walk the frame-pointer chain starting from the given (pc, fp, sp) triple,
/// usually obtained from a signal handler's ucontext.
///
/// Returns `(frames_written, truncated)`. `truncated` is `true` if the walk
/// stopped because the output buffer (or [`MAX_FRAMES`]) was full *and* at
/// least one additional frame would have been valid. A natural stop (end of
/// chain, faulty load, implausible pointer) produces `truncated = false`.
///
/// # Safety
/// - `install_handler` must have been called.
/// - Should generally be called from a signal handler where the target thread
///   is stopped; walking a running thread's stack races with mutations.
pub unsafe fn unwind(pc: usize, mut fp: usize, sp: usize, out: &mut [u64]) -> (usize, bool) {
    let limit = out.len().min(MAX_FRAMES);
    if limit == 0 {
        // No room even for the interrupted PC. The walk would have produced
        // at least one frame, so this is truncation.
        return (0, true);
    }

    out[0] = pc as u64;
    let mut n = 1;

    let stack_lo = sp;
    let stack_hi = sp.saturating_add(8 * 1024 * 1024);

    loop {
        // Validate current fp before reading from it.
        if fp < stack_lo || fp >= stack_hi {
            return (n, false);
        }
        if fp & (core::mem::size_of::<usize>() - 1) != 0 {
            return (n, false); // misaligned
        }

        let saved_fp = unsafe { load(fp as *const usize) };
        if saved_fp == SAFE_LOAD_FAULT {
            return (n, false);
        }
        let ret_addr_slot = (fp + core::mem::size_of::<usize>()) as *const usize;
        let ret_addr = strip_pac(unsafe { load(ret_addr_slot) });
        if ret_addr == SAFE_LOAD_FAULT {
            return (n, false);
        }

        if !(DEAD_ZONE..=usize::MAX - DEAD_ZONE).contains(&ret_addr) {
            return (n, false);
        }

        // Frame pointer must advance (stacks grow down -> saved_fp > fp)
        // but not by more than MAX_FRAME_SIZE.
        if saved_fp <= fp || saved_fp - fp > MAX_FRAME_SIZE {
            return (n, false);
        }

        // We have a valid next frame. If we ran out of room to record it,
        // the walk is truncated.
        if n >= limit {
            return (n, true);
        }

        out[n] = ret_addr as u64;
        n += 1;
        fp = saved_fp;
    }
}

/// Unwind from inside a signal handler given the raw ucontext.
///
/// Returns `(frames_written, truncated)`; see [`unwind`] for details.
///
/// # Safety
/// `ucontext` must be the pointer the kernel passed to a SA_SIGINFO handler.
pub unsafe fn unwind_from_ucontext(ucontext: *mut libc::c_void, out: &mut [u64]) -> (usize, bool) {
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

        let mut out = [0u64; MAX_FRAMES];
        let (n, truncated) = unsafe { unwind(0x40_0000, base, base, &mut out) };

        assert_eq!(n, 3);
        assert!(!truncated);
        assert_eq!(out[0], 0x40_0000); // interrupted PC
        assert_eq!(out[1], 0x40_1000);
        assert_eq!(out[2], 0x40_2000);
    }

    #[test]
    fn frame_zero_is_always_the_interrupted_pc() {
        install();
        let mut out = [0u64; MAX_FRAMES];
        let (n, truncated) = unsafe { unwind(0xDEAD, 0, 0x1000, &mut out) };
        assert_eq!(n, 1);
        assert!(!truncated);
        assert_eq!(out[0], 0xDEAD);
    }

    #[test]
    fn stops_at_misaligned_fp() {
        install();
        let mut out = [0u64; MAX_FRAMES];
        let sp = 0x7fff_0000_0000usize;
        let (n, truncated) = unsafe { unwind(0x40_0000, sp + 1, sp, &mut out) };
        assert_eq!(n, 1);
        assert!(!truncated);
    }

    #[test]
    fn stops_when_fp_below_stack() {
        install();
        let mut out = [0u64; MAX_FRAMES];
        let sp = 0x7fff_0000_0000usize;
        let (n, truncated) = unsafe { unwind(0x40_0000, sp.wrapping_sub(8), sp, &mut out) };
        assert_eq!(n, 1);
        assert!(!truncated);
    }

    #[test]
    fn stops_at_dead_zone_return_addr() {
        install();
        let sz = std::mem::size_of::<usize>();
        let mut stack = [0usize; 4];
        let base = stack.as_mut_ptr() as usize;

        stack[0] = base + 2 * sz; // saved_fp advances
        stack[1] = 0x100; // return addr in dead zone (< 0x1000)

        let mut out = [0u64; MAX_FRAMES];
        let (n, truncated) = unsafe { unwind(0x40_0000, base, base, &mut out) };
        assert_eq!(n, 1);
        assert!(!truncated);
    }

    #[test]
    fn stops_when_frame_size_exceeds_max() {
        install();
        let mut stack = [0usize; 4];
        let base = stack.as_mut_ptr() as usize;

        stack[0] = base + MAX_FRAME_SIZE + 8; // jump exceeds MAX_FRAME_SIZE

        let mut out = [0u64; MAX_FRAMES];
        let (n, truncated) = unsafe { unwind(0x40_0000, base, base, &mut out) };
        assert_eq!(n, 1);
        assert!(!truncated);
    }

    #[test]
    fn stops_when_fp_doesnt_advance() {
        install();
        let mut stack = [0usize; 4];
        let base = stack.as_mut_ptr() as usize;

        stack[0] = base; // saved_fp == fp, doesn't advance

        let mut out = [0u64; MAX_FRAMES];
        let (n, truncated) = unsafe { unwind(0x40_0000, base, base, &mut out) };
        assert_eq!(n, 1);
        assert!(!truncated);
    }

    #[test]
    fn respects_output_buffer_limit() {
        install();
        let mut out = [0u64; 1];
        let (n, _truncated) = unsafe { unwind(0x40_0000, 0, 0x1000, &mut out) };
        assert_eq!(n, 1);
        assert_eq!(out[0], 0x40_0000);
    }

    #[test]
    fn reports_truncation_when_buffer_fills_before_chain_ends() {
        install();
        let sz = std::mem::size_of::<usize>();
        let mut stack = [0usize; 8];
        let base = stack.as_mut_ptr() as usize;

        // Chain of 3 valid frames (same structure as `walks_valid_frame_chain`).
        stack[0] = base + 2 * sz;
        stack[1] = 0x40_1000;
        stack[2] = base + 4 * sz;
        stack[3] = 0x40_2000;
        stack[4] = base + 4 * sz; // terminates naturally here

        // Buffer fits only pc + 1 frame, so the 3rd frame is dropped.
        let mut out = [0u64; 2];
        let (n, truncated) = unsafe { unwind(0x40_0000, base, base, &mut out) };
        assert_eq!(n, 2);
        assert!(
            truncated,
            "should report truncation when output buffer fills"
        );
        assert_eq!(out[0], 0x40_0000);
        assert_eq!(out[1], 0x40_1000);
    }

    #[test]
    fn empty_buffer_reports_truncation() {
        install();
        let mut out: [u64; 0] = [];
        let (n, truncated) = unsafe { unwind(0x40_0000, 0, 0x1000, &mut out) };
        assert_eq!(n, 0);
        assert!(truncated, "empty buffer can always hold more");
    }

    fn page_size() -> usize {
        let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(ps > 0, "sysconf(_SC_PAGESIZE) must return a positive value");
        ps as usize
    }

    #[test]
    fn stops_when_saved_fp_load_faults() {
        install();
        let ps = page_size();
        // Guard page, any load faults.
        let guard = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ps,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(guard, libc::MAP_FAILED);
        let fp = guard as usize;

        // sp = fp keeps fp in range, page-aligned.
        let mut out = [0u64; MAX_FRAMES];
        let (n, truncated) = unsafe { unwind(0x40_0000, fp, fp, &mut out) };

        // Unmap before the asserts so a panic doesn't leak the mapping.
        assert_eq!(unsafe { libc::munmap(guard, ps) }, 0);

        assert_eq!(n, 1, "unwind must stop when saved_fp load faults");
        assert!(!truncated);
        assert_eq!(out[0], 0x40_0000);
    }

    #[test]
    fn stops_when_ret_addr_load_faults() {
        install();
        let ps = page_size();
        // Two pages: first writable, second PROT_NONE. fp at last usize of
        // first page => *fp succeeds, *(fp+8) crosses into guard and faults.
        let region = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(region, libc::MAP_FAILED);
        let guard = unsafe { (region as *mut u8).add(ps) };
        assert_eq!(
            unsafe { libc::mprotect(guard as *mut _, ps, libc::PROT_NONE) },
            0,
        );

        let sz = std::mem::size_of::<usize>();
        let fp = unsafe { (region as *mut u8).add(ps - sz) } as usize;
        // Ensure saved_fp load succeeds so the walk reaches ret_addr load.
        unsafe { (fp as *mut usize).write(fp + 16) };

        let mut out = [0u64; MAX_FRAMES];
        let (n, truncated) = unsafe { unwind(0x40_0000, fp, fp, &mut out) };

        // Unmap before the asserts so a panic doesn't leak the mapping.
        assert_eq!(unsafe { libc::munmap(region, 2 * ps) }, 0);

        assert_eq!(n, 1, "unwind must stop when ret_addr load faults");
        assert!(!truncated);
        assert_eq!(out[0], 0x40_0000);
    }
}
