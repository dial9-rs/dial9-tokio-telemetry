#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use dial9_perf_self_profile::{
    EventSource, PerfSampler, SamplerConfig, SamplingMode, is_ctimer_active,
};

// libc doesn't re-export AUDIT_ARCH_*, see include/uapi/linux/audit.h.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

// Seccomp BPF: return EACCES for perf_event_open, allow everything else.
// PR_SET_NO_NEW_PRIVS lets us install the filter without CAP_SYS_ADMIN.
fn install_seccomp_blocking_perf_event_open() {
    let ld_abs = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    let jeq = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    let ret = (libc::BPF_RET | libc::BPF_K) as u16;

    // seccomp_data offsets: nr @ 0, arch @ 4.
    let filter = [
        stmt(ld_abs, 4),
        jump(jeq, AUDIT_ARCH, 0, 3),
        stmt(ld_abs, 0),
        jump(jeq, libc::SYS_perf_event_open as u32, 0, 1),
        stmt(ret, libc::SECCOMP_RET_ERRNO | libc::EACCES as u32),
        stmt(ret, libc::SECCOMP_RET_ALLOW),
    ];
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut _,
    };

    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                &prog as *const _ as libc::c_ulong,
                0,
                0,
            ),
            0
        );
    }
}

/// Raw self-profile probe. Returns true if the kernel lets us open a
/// minimal perf event on the calling task.
fn perf_event_open_works() -> bool {
    // perf_event_attr: type=PERF_TYPE_SOFTWARE (1), size=128,
    // config=PERF_COUNT_SW_CPU_CLOCK (0). Rest zeroed.
    let mut attr: [u8; 128] = [0; 128];
    attr[0] = 1;
    attr[4..8].copy_from_slice(&128u32.to_le_bytes());
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            attr.as_ptr(),
            0i32,  // pid = self
            0i32,  // cpu = 0
            -1i32, // group_fd
            0u64,  // flags
        )
    };
    if fd >= 0 {
        unsafe { libc::close(fd as i32) };
        true
    } else {
        false
    }
}

#[test]
fn uses_perf_path_when_available() {
    if !perf_event_open_works() {
        eprintln!("skipping: perf_event_open blocked in this env");
        return;
    }

    let _sampler = PerfSampler::start(
        SamplerConfig::default()
            .event_source(EventSource::SwCpuClock)
            .sampling(SamplingMode::FrequencyHz(999)),
    )
    .expect("PerfSampler::start should succeed when perf is available");

    assert!(
        !is_ctimer_active(),
        "dispatcher routed to ctimer despite perf being available"
    );
}

// Fork so the seccomp filter (sticky) stays isolated from the rest of
// the test run.
#[test]
fn falls_back_to_ctimer_when_perf_event_open_blocked() {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

    if pid == 0 {
        install_seccomp_blocking_perf_event_open();
        let sampler = PerfSampler::start(
            SamplerConfig::default()
                .event_source(EventSource::SwCpuClock)
                .sampling(SamplingMode::FrequencyHz(999)),
        );
        let ok = sampler.is_ok() && is_ctimer_active();
        unsafe { libc::_exit(!ok as i32) };
    }

    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status), "child did not exit normally");
    assert_eq!(libc::WEXITSTATUS(status), 0, "fallback did not trigger");
}
