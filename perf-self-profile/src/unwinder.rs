//! Framepointer-based stack unwinding
//!
//! This implementation uses a SIGSEV fault handler to allow safe walking of stacks. Without
//! this, there is no way to perform framepointer unwinding without risking segfaults when walking
//! stacks where framepointers are not enabled.
//!
//! Because of this, the unwinder must be "[`install`ed](Unwinder::install)" before
//! you can use it to [`capture`](Unwinder::capture) a stack.
//!
//! The unwinded stacks are only addresses. You must use a symbolizer separately to
//! convert the addresses into function names.

/// Result of a [`Unwinder::capture`] call.
///
/// The captured program counters are written into the output buffer supplied
/// to `capture`; this struct describes the metadata of the capture.
///
/// `#[non_exhaustive]` so new fields can be added without breaking callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaptureResult {
    /// Number of frames written into the caller's output buffer.
    ///
    /// Frames `out[0..frames_written]` are valid.
    pub frames_written: usize,
    /// `true` if the walk stopped because the output buffer (or the
    /// internal `MAX_FRAMES` cap of 128) was full while at least one
    /// additional frame was still walkable. When `true`, the outer
    /// frames of the stack (closer to `main`) have been dropped.
    pub truncated: bool,
}

/// Handle that proves the SIGSEGV fault handler is installed.
///
/// This type is zero-sized and can be freely copied and cloned.
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

    /// Verify that our SIGSEGV handler is still the active handler for
    /// SIGSEGV on this process. Returns `true` if the handler we installed
    /// is still registered.
    ///
    /// Another library or the runtime may install its own SIGSEGV handler
    /// after [`install`](Self::install) is called. If that handler does not
    /// chain to ours, [`capture`](Self::capture) may crash on a bad frame
    /// pointer instead of aborting the walk safely. Callers who need
    /// defence against this can call `verify_handler` periodically or
    /// before safety-critical captures.
    ///
    /// Performs one `sigaction` syscall. Not suitable for per-sample hot
    /// paths.
    pub fn verify_handler(&self) -> bool {
        platform::verify_handler()
    }

    /// Capture a stack trace of the calling thread into `out`. Returns a
    /// [`CaptureResult`] describing the number of frames written and
    /// whether the walk was truncated. Never allocates.
    ///
    /// # Frame-0 contract
    /// `out[0]` is the return address of `capture` itself — i.e. a PC
    /// *inside the caller of `capture`*. Subsequent frames walk outward
    /// via the frame-pointer chain. Any `#[inline(never)]` shim inserted
    /// between the user's code and `capture` will appear as an extra
    /// frame; plain function calls with frame pointers enabled behave as
    /// expected.
    ///
    /// # Buffer and truncation
    /// At most `out.len().min(MAX_FRAMES)` frames are written (where
    /// `MAX_FRAMES = 128`). If the real stack is deeper, innermost frames
    /// are kept and outer frames are dropped; `CaptureResult::truncated`
    /// is set to `true`.
    ///
    /// # Safety
    /// - [`install`](Self::install) must have succeeded and the SIGSEGV
    ///   handler it registered must still be active. If another library
    ///   has replaced the SIGSEGV handler without chaining to ours, a
    ///   faulty frame-pointer chain can crash the process instead of
    ///   being caught. Use [`verify_handler`](Self::verify_handler) if
    ///   you need to defend against third-party signal handler
    ///   installation.
    /// - Must not be called from inside a signal handler for SIGSEGV
    ///   (that would recurse into our own handler without bound).
    /// - The calling thread's stack must be valid for frame-pointer walking
    ///   (binary compiled with `-C force-frame-pointers=yes`, no code
    ///   currently executing in a prologue/epilogue window where `rbp`
    ///   does not point at a saved-fp slot).
    #[inline(never)]
    pub unsafe fn capture(&self, out: &mut [u64]) -> CaptureResult {
        // Debug-only check that our SIGSEGV handler is still the active
        // one. In release builds this is skipped to keep `capture` syscall-free
        // on the hot path; callers who need this at runtime should use
        // [`verify_handler`](Self::verify_handler) explicitly.
        debug_assert!(
            self.verify_handler(),
            "Unwinder::capture called but our SIGSEGV handler is no longer active; \
             something replaced it without chaining. See Unwinder::verify_handler."
        );
        // SAFETY: forwarding Unwinder::capture's own safety contract to
        // platform::capture (handler installed, not in a SIGSEGV handler,
        // frame pointers enabled).
        unsafe { platform::capture(out) }
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod platform {
    use super::CaptureResult;
    use crate::sys::fp_profiler::{handler_is_installed, install_handler, unwind::unwind};

    pub fn install() -> std::io::Result<()> {
        // SAFETY: installs the SIGSEGV handler for safe_load; idempotent.
        unsafe { install_handler() }
    }

    pub fn verify_handler() -> bool {
        handler_is_installed()
    }

    /// Called from [`Unwinder::capture`] (which is `#[inline(never)]`), so the
    /// "current frame" observed here is `Unwinder::capture`'s frame. This
    /// function is `#[inline(always)]` specifically so it does *not* introduce
    /// another frame — see the `frame_zero_points_into_caller_of_capture`
    /// test.
    ///
    /// # Safety
    /// Same obligations as [`Unwinder::capture`]: handler installed,
    /// not inside a SIGSEGV handler, frame pointers enabled.
    #[inline(always)]
    pub unsafe fn capture(out: &mut [u64]) -> CaptureResult {
        // SAFETY: called from Unwinder::capture which forwards the full
        // safety contract (handler installed, frame pointers enabled, not
        // inside a SIGSEGV handler, not in a prologue/epilogue window).
        let (pc, fp, sp) = unsafe { read_caller_regs() };
        // SAFETY: handler is installed (caller holds Unwinder), and we are
        // not inside a SIGSEGV handler (see Unwinder::capture safety
        // contract).
        let (frames_written, truncated) = unsafe { unwind(pc, fp, sp, out) };
        CaptureResult {
            frames_written,
            truncated,
        }
    }

    /// Read `(pc, fp, sp)` such that `pc` is the PC to use for frame 0 and
    /// `fp` is the saved frame pointer of the frame *above* the current one.
    ///
    /// Because this is `#[inline(always)]`, the `rbp`/`x29` read observes
    /// `Unwinder::capture`'s frame (its one and only non-inlined ancestor).
    /// - `*(rbp + 8)` on x86_64 / LR save slot on aarch64 is
    ///   `Unwinder::capture`'s return address → goes to `out[0]`.
    /// - `*rbp` / `*x29` is the saved fp of the caller of `Unwinder::capture`
    ///   → where we start walking the chain.
    ///
    /// # Safety
    /// - Must be called with `#[inline(always)]` preserved so the read
    ///   observes `Unwinder::capture`'s frame, not this helper's. If ever
    ///   actually inlined into a different caller or promoted to a
    ///   standalone frame, the returned `fp`/return-address semantics
    ///   change and the frame-0 contract breaks.
    /// - Must only be called after [`install_handler`] has succeeded.
    ///   Reading `*fp` and `*(fp+8)` is a raw dereference of the stack;
    ///   the SIGSEGV handler installed by `install_handler` does *not*
    ///   cover these reads (it only covers `safe_load`). The reads are
    ///   safe only because — on a thread built with
    ///   `-C force-frame-pointers=yes` — the kernel/ABI guarantees that
    ///   `rbp`/`x29` always points at a valid `[saved_fp, ret_addr]`
    ///   pair for the currently executing function.
    /// - The calling binary must be compiled with
    ///   `-C force-frame-pointers=yes`. Without frame pointers, `rbp`
    ///   may be used as a general-purpose register and the raw reads
    ///   will dereference arbitrary memory.
    /// - Must not be called during function prologue/epilogue or other
    ///   windows where `rbp`/`x29` does not yet (or no longer) points at
    ///   a saved-fp slot. For the intended call site inside
    ///   `Unwinder::capture`'s body this is always satisfied; calling
    ///   from hand-written asm shims, signal-trampoline code, or from
    ///   within another `naked` function is not supported.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn read_caller_regs() -> (usize, usize, usize) {
        let fp: usize;
        let sp: usize;
        // SAFETY: Reading `rbp`/`rsp` with `nostack, nomem` has no memory
        // side effects and cannot invalidate Rust's stack invariants; we
        // do not modify either register.
        unsafe {
            core::arch::asm!(
                "mov {fp}, rbp",
                "mov {sp}, rsp",
                fp = out(reg) fp,
                sp = out(reg) sp,
                options(nostack, nomem),
            );
        }
        // SAFETY: `fp` is `Unwinder::capture`'s frame pointer (see top-level
        // # Safety note). On x86_64 System V, with frame pointers enabled,
        // a compiler-generated frame begins with
        //   [saved_rbp : usize, return_addr : usize, ...]
        // so `*fp` and `*(fp + 8)` are guaranteed to be valid reads of
        // currently-live stack memory for this frame.
        let ret_addr = unsafe { *(fp as *const usize).add(1) };
        let caller_fp = unsafe { *(fp as *const usize) };
        (ret_addr, caller_fp, sp)
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn read_caller_regs() -> (usize, usize, usize) {
        let fp: usize;
        let sp: usize;
        // SAFETY: Reading `x29`/`sp` with `nostack, nomem` has no memory
        // side effects and cannot invalidate Rust's stack invariants; we
        // do not modify either register.
        unsafe {
            core::arch::asm!(
                "mov {fp}, x29",
                "mov {sp}, sp",
                fp = out(reg) fp,
                sp = out(reg) sp,
                options(nostack, nomem),
            );
        }
        // SAFETY: Same layout as x86_64 under AAPCS64 with frame pointers:
        //   [saved_fp (x29) : u64, saved_lr : u64, ...]
        // so `*fp` and `*(fp + 8)` are valid reads of live stack memory.
        let ret_addr = unsafe { *(fp as *const usize).add(1) };
        // Strip pointer authentication bits (ARMv8.3-A PAC). The saved LR may
        // be signed when compiled with `-Z branch-protection=pac-ret`.
        // See: https://www.kernel.org/doc/html/latest/arch/arm64/pointer-authentication.html
        let ret_addr = ret_addr & 0x0000_FFFF_FFFF_FFFF;
        let caller_fp = unsafe { *(fp as *const usize) };
        (ret_addr, caller_fp, sp)
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod platform {
    use super::CaptureResult;

    pub fn install() -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unwinder is only available on Linux x86_64/aarch64",
        ))
    }

    pub fn verify_handler() -> bool {
        false
    }

    pub unsafe fn capture(_out: &mut [u64]) -> CaptureResult {
        CaptureResult {
            frames_written: 0,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    mod linux {
        use super::super::*;

        /// Skip the test if something (e.g. ASAN) replaced our SIGSEGV handler
        /// after install. Returns the Unwinder on success.
        fn install_or_skip() -> Option<Unwinder> {
            let u = Unwinder::install().unwrap();
            if !u.verify_handler() {
                eprintln!("skipping: SIGSEGV handler was replaced (sanitizer?)");
                return None;
            }
            Some(u)
        }

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
        fn verify_handler_true_after_install() {
            let Some(u) = install_or_skip() else {
                return;
            };
            assert!(u.verify_handler(), "handler should be active after install");
        }

        #[test]
        fn capture_produces_frames() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            #[inline(never)]
            fn helper(u: &Unwinder) -> (CaptureResult, [u64; 64]) {
                let mut out = [0u64; 64];
                // SAFETY: handler installed via Unwinder::install above; test thread
                // is not inside a signal handler.
                let result = unsafe { u.capture(&mut out) };
                std::hint::black_box(&out);
                (result, out)
            }
            let (result, out) = helper(&unwinder);
            assert!(
                result.frames_written >= 2,
                "expected at least 2 frames, got {}",
                result.frames_written
            );
            for (i, &addr) in out.iter().enumerate().take(result.frames_written) {
                assert_ne!(addr, 0, "frame {i} must be non-zero");
            }
        }

        /// Tighter version of the frame-0 contract test: verify that frame 0
        /// lands inside `helper` (the caller of `capture`) rather than inside
        /// `Unwinder::capture` itself. This catches the bug where the old
        /// double-`#[inline(never)]` layering made frame 0 point at an
        /// instruction inside `Unwinder::capture`'s body.
        #[test]
        fn frame_zero_points_into_caller_of_capture() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };

            #[inline(never)]
            fn helper(u: &Unwinder) -> u64 {
                let mut out = [0u64; 64];
                // SAFETY: same as capture_produces_frames.
                let result = unsafe { u.capture(&mut out) };
                std::hint::black_box(&out);
                assert!(result.frames_written >= 1);
                out[0]
            }

            let frame0 = helper(&unwinder);
            let helper_start = helper as *const () as u64;
            let capture_fn = Unwinder::capture as *const () as u64;

            // Frame 0 must NOT be inside Unwinder::capture's body. Allow a
            // generous 4 KiB window around its entry in case of debug bloat.
            let capture_window = 4096u64;
            assert!(
                !(frame0 >= capture_fn && frame0 < capture_fn + capture_window),
                "frame 0 {:#x} must not be inside Unwinder::capture [{:#x}..{:#x})",
                frame0,
                capture_fn,
                capture_fn + capture_window,
            );

            // Stronger: frame 0 should land inside `helper`'s body. Debug
            // builds can be large, so use a 64 KiB window as an upper bound.
            let helper_window = 64 * 1024u64;
            assert!(
                frame0 >= helper_start && frame0 < helper_start + helper_window,
                "frame 0 {:#x} should be inside helper [{:#x}..{:#x})",
                frame0,
                helper_start,
                helper_start + helper_window,
            );
        }

        #[test]
        fn capture_respects_output_buffer_limit() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            let mut out = [0u64; 1];
            // SAFETY: handler installed; test context is not a signal handler.
            let result = unsafe { unwinder.capture(&mut out) };
            assert!(
                result.frames_written <= 1,
                "expected at most 1 frame, got {}",
                result.frames_written
            );
            if result.frames_written == 1 {
                assert_ne!(out[0], 0, "frame 0 must be non-zero when written");
            }
        }

        #[test]
        fn capture_reports_truncation_with_tiny_buffer() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            // Build a small but real call chain so a 1-slot buffer is bound
            // to truncate.
            #[inline(never)]
            fn depth_2(u: &Unwinder) -> CaptureResult {
                let mut out = [0u64; 1];
                // SAFETY: handler installed above.
                let r = unsafe { u.capture(&mut out) };
                std::hint::black_box(&out);
                r
            }
            #[inline(never)]
            fn depth_1(u: &Unwinder) -> CaptureResult {
                std::hint::black_box(depth_2(u))
            }
            let result = depth_1(&unwinder);
            assert_eq!(result.frames_written, 1);
            assert!(
                result.truncated,
                "a 1-slot buffer with a multi-frame stack must report truncated"
            );
        }

        #[test]
        fn capture_with_empty_buffer_reports_truncation() {
            let Some(unwinder) = install_or_skip() else {
                return;
            };
            // SAFETY: handler installed above.
            let result = unsafe { unwinder.capture(&mut []) };
            assert_eq!(result.frames_written, 0);
            assert!(result.truncated);
        }

        /// In debug builds, `capture` panics via `debug_assert!` when its SIGSEGV
        /// handler has been replaced out from under it. This protects against
        /// third-party signal handlers silently breaking stack-walk fault
        /// recovery.
        ///
        /// Nextest runs each test in its own process, so replacing the SIGSEGV
        /// handler here cannot affect sibling tests.
        #[cfg(debug_assertions)]
        #[test]
        fn capture_debug_asserts_when_handler_replaced() {
            use std::panic::{AssertUnwindSafe, catch_unwind};

            let Some(unwinder) = install_or_skip() else {
                return;
            };
            assert!(unwinder.verify_handler());

            // Replace SIGSEGV with SIG_IGN so verify_handler() returns false.
            // SAFETY: we restore our handler below on all exit paths via the
            // guard; nextest's process isolation limits blast radius even if
            // restoration is skipped.
            let mut new_action: libc::sigaction = unsafe { std::mem::zeroed() };
            new_action.sa_sigaction = libc::SIG_IGN;
            unsafe { libc::sigemptyset(&mut new_action.sa_mask) };
            let rc = unsafe { libc::sigaction(libc::SIGSEGV, &new_action, std::ptr::null_mut()) };
            assert_eq!(rc, 0, "failed to replace SIGSEGV handler");

            struct RestoreGuard;
            impl Drop for RestoreGuard {
                fn drop(&mut self) {
                    // Best-effort restore of our handler. `Unwinder::install`
                    // is a no-op after the first successful call (idempotent
                    // flag), so use `sigaction` directly. Nextest's
                    // per-test process isolation means even if this fails,
                    // other tests are unaffected.
                    // SAFETY: process-global signal state; we rewrite
                    // SIGSEGV back to SIG_DFL so any subsequent fault during
                    // teardown terminates cleanly rather than being ignored.
                    let mut dfl: libc::sigaction = unsafe { std::mem::zeroed() };
                    dfl.sa_sigaction = libc::SIG_DFL;
                    unsafe { libc::sigemptyset(&mut dfl.sa_mask) };
                    let _ = unsafe { libc::sigaction(libc::SIGSEGV, &dfl, std::ptr::null_mut()) };
                }
            }
            let _guard = RestoreGuard;

            assert!(
                !unwinder.verify_handler(),
                "precondition: handler should appear replaced"
            );

            // capture() must panic via debug_assert before doing any stack
            // walking.
            let mut out = [0u64; 16];
            let result = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: we intentionally violate the "handler installed"
                // precondition to verify the debug_assert fires. The panic
                // from debug_assert aborts before the frame walk runs.
                let _ = unsafe { unwinder.capture(&mut out) };
            }));
            assert!(
                result.is_err(),
                "capture should panic via debug_assert when handler is replaced"
            );
        }
    }
}
