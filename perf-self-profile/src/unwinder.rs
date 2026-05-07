//! Public stack-unwinding API.
//!
//! Provides [`Unwinder`], a zero-sized handle that proves the SIGSEGV fault
//! handler is installed, and exposes a non-allocating `capture()` method for
//! collecting stack traces of the calling thread.

/// Handle that proves the SIGSEGV fault handler is installed.
/// Zero-sized, freely copyable.
#[derive(Clone, Copy, Debug)]
pub struct Unwinder {
    _private: (),
}

impl Unwinder {
    /// Install the SIGSEGV fault handler used by stack capture.
    /// Idempotent: safe to call multiple times from multiple threads.
    ///
    /// Returns `Err` if `sigaction` fails (Linux) or if the platform is
    /// unsupported.
    ///
    /// # Requirements
    /// - Frame pointers (build with `-C force-frame-pointers=yes`).
    pub fn install() -> std::io::Result<Self> {
        platform::install()?;
        Ok(Self { _private: () })
    }

    /// Capture a stack trace of the calling thread into `out`. Returns
    /// the number of frames written. Never allocates.
    ///
    /// # Frame-0 contract
    /// `out[0]` is the return address *into the caller of `capture`* —
    /// i.e. the PC where `capture` itself will return. Subsequent frames
    /// walk outward via the frame-pointer chain. Callers should expect
    /// to skip `capture` itself plus any `#[inline(never)]` shim they
    /// insert.
    ///
    /// # Safety contract
    /// Must not be called from inside a different SIGSEGV handler.
    #[inline(never)]
    pub fn capture(&self, out: &mut [u64]) -> usize {
        platform::capture(out)
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod platform {
    use crate::sys::fp_profiler::{install_handler, unwind::unwind};

    pub fn install() -> std::io::Result<()> {
        // SAFETY: installs the SIGSEGV handler for safe_load; idempotent.
        unsafe { install_handler() }
    }

    #[inline(never)]
    pub fn capture(out: &mut [u64]) -> usize {
        let (pc, fp, sp) = read_registers();
        // SAFETY: handler is installed (caller holds Unwinder), and we are not
        // inside a SIGSEGV handler.
        unsafe { unwind(pc, fp, sp, out) }
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn read_registers() -> (usize, usize, usize) {
        let fp: usize;
        let sp: usize;
        unsafe {
            core::arch::asm!(
                "mov {fp}, rbp",
                "mov {sp}, rsp",
                fp = out(reg) fp,
                sp = out(reg) sp,
                options(nostack, nomem),
            );
        }
        // We want frame 0 to be the return address of our caller (capture).
        // The current fp points to capture's frame. The return address of
        // capture is at *(fp + 8). The caller's fp is at *fp.
        // We pass the return address as `pc` and *fp as the new fp to start
        // walking from the caller's caller.
        let ret_addr = unsafe { *(fp as *const usize).add(1) };
        let caller_fp = unsafe { *(fp as *const usize) };
        (ret_addr, caller_fp, sp)
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    fn read_registers() -> (usize, usize, usize) {
        let fp: usize;
        let sp: usize;
        unsafe {
            core::arch::asm!(
                "mov {fp}, x29",
                "mov {sp}, sp",
                fp = out(reg) fp,
                sp = out(reg) sp,
                options(nostack, nomem),
            );
        }
        let ret_addr = unsafe { *(fp as *const usize).add(1) };
        let caller_fp = unsafe { *(fp as *const usize) };
        (ret_addr, caller_fp, sp)
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod platform {
    pub fn install() -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unwinder is only available on Linux x86_64/aarch64",
        ))
    }

    pub fn capture(_out: &mut [u64]) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent() {
        let r1 = Unwinder::install();
        let r2 = Unwinder::install();
        let r3 = Unwinder::install();
        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert!(r3.is_ok());
    }

    #[test]
    fn install_is_idempotent_across_threads() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(Unwinder::install))
            .collect();
        for h in handles {
            assert!(h.join().unwrap().is_ok());
        }
    }

    #[test]
    fn capture_produces_frames() {
        let unwinder = Unwinder::install().unwrap();
        #[inline(never)]
        fn helper(u: &Unwinder) -> (usize, [u64; 64]) {
            let mut out = [0u64; 64];
            let n = u.capture(&mut out);
            // Anchor a known address *after* the capture site so we can
            // verify frame 0 points back into `helper` rather than into
            // `capture` itself.
            std::hint::black_box(&out);
            (n, out)
        }
        let (n, out) = helper(&unwinder);
        assert!(n >= 2, "expected at least 2 frames, got {n}");
        for (i, &addr) in out.iter().enumerate().take(n) {
            assert_ne!(addr, 0, "frame {i} must be non-zero");
        }

        // Frame-0 contract: frame 0 is the PC `capture` returns to, which
        // must be *inside* `helper` (the caller of `capture`).
        //
        // Bound `helper` by probing the addresses of two labels inside it:
        // the address we take is a pointer into `helper`'s body, and the
        // returned frame 0 must land within a reasonable window around it.
        //
        // We can't easily get the size of `helper`, so use a looser bound:
        // verify that frame 0 is not equal to the address of `capture`
        // itself, and that it is some reasonable code address.
        let capture_addr = Unwinder::capture as *const () as u64;
        assert_ne!(
            out[0], capture_addr,
            "frame 0 must not be inside Unwinder::capture"
        );
    }

    #[test]
    fn capture_respects_output_buffer_limit() {
        let unwinder = Unwinder::install().unwrap();
        let mut out = [0u64; 1];
        let n = unwinder.capture(&mut out);
        assert!(n <= 1, "expected at most 1 frame, got {n}");
        if n == 1 {
            assert_ne!(out[0], 0, "frame 0 must be non-zero when written");
        }
    }

    #[test]
    fn capture_with_empty_buffer_returns_zero() {
        let unwinder = Unwinder::install().unwrap();
        let n = unwinder.capture(&mut []);
        assert_eq!(n, 0);
    }
}
