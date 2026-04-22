//! Userspace frame-pointer profiling infrastructure.
//! x86_64 and aarch64 only.

pub mod ctimer;
pub mod sample_buffer;
pub mod unwind;

/// Sentinel returned when the load faulted.
pub const SAFE_LOAD_FAULT: usize = 0;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "dial9-fp-profiler: unsupported Linux architecture \
     (only x86_64 and aarch64 are supported)"
);

mod supported {
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    use super::SAFE_LOAD_FAULT;

    // Trampoline for a fault-tolerant load (bounded by start/end labels).

    #[cfg(target_arch = "x86_64")]
    core::arch::global_asm!(
        ".globl safe_load_start",
        ".globl safe_load_end",
        ".globl safe_load",
        ".type safe_load, @function",
        "safe_load:",
        "safe_load_start:",
        "    mov (%rdi), %rax",
        "safe_load_end:",
        "    ret",
        options(att_syntax)
    );

    #[cfg(target_arch = "aarch64")]
    core::arch::global_asm!(
        ".globl safe_load_start",
        ".globl safe_load_end",
        ".globl safe_load",
        ".type safe_load, @function",
        "safe_load:",
        "safe_load_start:",
        "    ldr x0, [x0]",
        "safe_load_end:",
        "    ret",
    );

    unsafe extern "C" {
        fn safe_load(ptr: *const usize) -> usize;
        static safe_load_start: u8;
        static safe_load_end: u8;
    }

    /// Dereference `ptr` returning its value, or `SAFE_LOAD_FAULT` if the
    /// read faulted (unmapped page, guard page, etc.).
    ///
    /// # Safety
    /// - The SIGSEGV handler must be installed via [`install_handler`] first.
    /// - Must only be called from contexts where SAFE_LOAD_FAULT is
    ///   distinguishable from a real zero.
    /// - The pointer's alignment must be appropriate for a `usize` load.
    #[inline(always)]
    pub unsafe fn load(ptr: *const usize) -> usize {
        unsafe { safe_load(ptr) }
    }

    static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);
    static OLD_HANDLER: AtomicPtr<libc::sigaction> = AtomicPtr::new(ptr::null_mut());

    /// Install our SIGSEGV handler, chaining to whatever was previously
    /// registered.
    ///
    /// # Safety
    /// Modifies process-global signal state. Call once during initialization.
    pub unsafe fn install_handler() -> Result<(), std::io::Error> {
        if HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
            return Ok(()); // already installed
        }

        let mut new_action: libc::sigaction = unsafe { std::mem::zeroed() };
        new_action.sa_sigaction = sigsegv_handler as *const () as usize;
        // SA_NODEFER: safe_load may fault inside SIGPROF, without this the second
        // SIGSEGV queues instead of fires. Invariant: neither this handler nor any
        // chained handler may fault, or we recurse without bound.
        new_action.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
        unsafe { libc::sigemptyset(&mut new_action.sa_mask) };

        let old_storage = Box::into_raw(Box::new(unsafe { std::mem::zeroed::<libc::sigaction>() }));

        if unsafe { libc::sigaction(libc::SIGSEGV, &new_action, old_storage) } != 0 {
            let err = std::io::Error::last_os_error();
            unsafe { drop(Box::from_raw(old_storage)) };
            HANDLER_INSTALLED.store(false, Ordering::SeqCst);
            return Err(err);
        }

        OLD_HANDLER.store(old_storage, Ordering::SeqCst);
        Ok(())
    }

    /// SIGSEGV handler for `safe_load`: if the faulting PC is within the
    /// `safe_load_start..safe_load_end` instruction range, it skips the faulting
    /// load, and resumes execution.
    /// Otherwise, it chains to the previously installed handler.
    extern "C" fn sigsegv_handler(
        signo: libc::c_int,
        info: *mut libc::siginfo_t,
        ucontext: *mut libc::c_void,
    ) {
        // SAFETY: In a SA_SIGINFO SIGSEGV handler, kernel provides a valid ucontext_t;
        // safe_load_start/end are linker-defined code labels in this module.
        unsafe {
            let pc = get_pc(ucontext);
            let start = &safe_load_start as *const u8 as usize;
            let end = &safe_load_end as *const u8 as usize;

            if pc >= start && pc < end {
                set_pc(ucontext, end);
                set_result_reg(ucontext, SAFE_LOAD_FAULT);
                return;
            }

            // Not ours. Chain to the previous handler.
            let old = OLD_HANDLER.load(Ordering::SeqCst);
            if !old.is_null() {
                let old_ref = &*old;
                if old_ref.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                        std::mem::transmute(old_ref.sa_sigaction);
                    f(signo, info, ucontext);
                    return;
                }
                let h = old_ref.sa_sigaction;
                if h == libc::SIG_DFL {
                    // Restore default handler and re-raise so the kernel
                    // terminates the process as expected for a real SIGSEGV.
                    let mut dfl: libc::sigaction = std::mem::zeroed();
                    dfl.sa_sigaction = libc::SIG_DFL;
                    libc::sigemptyset(&mut dfl.sa_mask);
                    libc::sigaction(libc::SIGSEGV, &dfl, ptr::null_mut());
                    libc::raise(libc::SIGSEGV);
                } else if h != libc::SIG_IGN {
                    // SAFETY: SA_SIGINFO is not set, the old handler uses the 1-arg
                    // `void (*)(int)` signal-handler signature.
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(h);
                    f(signo);
                }
            }
        }
    }

    // Architecture-specific ucontext access

    #[cfg(target_arch = "x86_64")]
    unsafe fn get_pc(uc: *mut libc::c_void) -> usize {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as usize }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn set_pc(uc: *mut libc::c_void, pc: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] = pc as i64 };
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn set_result_reg(uc: *mut libc::c_void, val: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.gregs[libc::REG_RAX as usize] = val as i64 };
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn get_pc(uc: *mut libc::c_void) -> usize {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.pc as usize }
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn set_pc(uc: *mut libc::c_void, pc: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.pc = pc as u64 };
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn set_result_reg(uc: *mut libc::c_void, val: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.regs[0] = val as u64 };
    }
}

pub use supported::{install_handler, load};
