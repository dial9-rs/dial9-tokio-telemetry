//! Raw Linux perf_event types, constants, and syscall wrapper.

#![allow(dead_code)]

use libc::{SYS_perf_event_open, c_int, c_ulong, pid_t, syscall};
use std::mem;

// --- Event types ---

pub const PERF_TYPE_HARDWARE: u32 = 0;
pub const PERF_TYPE_SOFTWARE: u32 = 1;

// --- Hardware events ---

pub const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;

// --- Software events ---

pub const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;
pub const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
pub const PERF_COUNT_SW_CONTEXT_SWITCHES: u64 = 3;

// --- sample_type bitmask ---

pub const PERF_SAMPLE_IP: u64 = 1 << 0;
pub const PERF_SAMPLE_TID: u64 = 1 << 1;
pub const PERF_SAMPLE_TIME: u64 = 1 << 2;
pub const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;
pub const PERF_SAMPLE_CPU: u64 = 1 << 7;
pub const PERF_SAMPLE_PERIOD: u64 = 1 << 8;
pub const PERF_SAMPLE_REGS_USER: u64 = 1 << 12;
pub const PERF_SAMPLE_STACK_USER: u64 = 1 << 13;

// --- attr.flags (bitfield) ---
// On little-endian, bit N is simply (1 << N).

pub const PERF_ATTR_FLAG_DISABLED: u64 = 1 << 0;
pub const PERF_ATTR_FLAG_INHERIT: u64 = 1 << 1;
pub const PERF_ATTR_FLAG_EXCLUDE_KERNEL: u64 = 1 << 5;
pub const PERF_ATTR_FLAG_EXCLUDE_HV: u64 = 1 << 6;
pub const PERF_ATTR_FLAG_FREQ: u64 = 1 << 10;
pub const PERF_ATTR_FLAG_SAMPLE_ID_ALL: u64 = 1 << 18;
pub const PERF_ATTR_FLAG_EXCLUDE_CALLCHAIN_USER: u64 = 1 << 23;

// --- perf_event_open flags ---

pub const PERF_FLAG_FD_CLOEXEC: c_ulong = 1 << 3;

// --- ioctl ---

// PERF_EVENT_IOC_ENABLE = _IO('$', 0)
// On x86_64/aarch64 (standard ioctl encoding): direction=IOC_NONE=0, type='$'=0x24, nr=0, size=0
// = (0 << 30) | (0x24 << 8) | (0 << 0) | (0 << 16) = 0x2400
pub const PERF_EVENT_IOC_ENABLE: c_ulong = 0x2400;
pub const PERF_EVENT_IOC_DISABLE: c_ulong = 0x2401;

// --- Record types ---

pub const PERF_RECORD_SAMPLE: u32 = 9;
pub const PERF_RECORD_LOST: u32 = 2;

// --- Kernel address threshold ---
// On x86_64, userspace virtual addresses are below 0x0000_8000_0000_0000.
// Anything at or above this is kernel space, a context marker, or invalid.
pub const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;

// --- Context markers in callchain ---
// These sentinel values separate kernel/user/hypervisor frames in the callchain.

pub const PERF_CONTEXT_HV: u64 = u64::MAX - 31; // -32 as u64
pub const PERF_CONTEXT_KERNEL: u64 = u64::MAX - 127; // -128 as u64
pub const PERF_CONTEXT_USER: u64 = u64::MAX - 511; // -512 as u64
pub const PERF_CONTEXT_GUEST: u64 = u64::MAX - 2047; // -2048 as u64
pub const PERF_CONTEXT_GUEST_KERNEL: u64 = u64::MAX - 2175; // -2176 as u64
pub const PERF_CONTEXT_GUEST_USER: u64 = u64::MAX - 2559; // -2560 as u64
pub const PERF_CONTEXT_MAX: u64 = u64::MAX - 4095; // -4096 as u64

// --- perf_event_attr ---

#[repr(C)]
#[derive(Debug)]
pub struct PerfEventAttr {
    pub type_: u32,
    pub size: u32,
    pub config: u64,
    pub sample_period_or_freq: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub wakeup_events_or_watermark: u32,
    pub bp_type: u32,
    pub bp_addr_or_config1: u64,
    pub bp_len_or_config2: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clock_id: i32,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub __reserved_2: u16,
    pub aux_sample_size: u32,
    pub __reserved_3: u32,
}

impl PerfEventAttr {
    pub fn zeroed() -> Self {
        unsafe { mem::zeroed() }
    }
}

// --- perf_event_mmap_page (ring buffer metadata) ---

#[repr(C)]
pub struct PerfEventMmapPage {
    pub version: u32,
    pub compat_version: u32,
    pub lock: u32,
    pub index: u32,
    pub offset: i64,
    pub time_enabled: u64,
    pub time_running: u64,
    pub capabilities: u64,
    pub pmc_width: u16,
    pub time_shift: u16,
    pub time_mult: u32,
    pub time_offset: u64,
    pub time_zero: u64,
    pub size: u32,
    pub _reserved: [u8; 948], // pad to offset 0x400 = 1024
    pub data_head: u64,
    pub data_tail: u64,
    pub data_offset: u64,
    pub data_size: u64,
    pub aux_head: u64,
    pub aux_tail: u64,
    pub aux_offset: u64,
    pub aux_size: u64,
}

// --- perf_event_header (precedes each record in ring buffer) ---

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PerfEventHeader {
    pub type_: u32,
    pub misc: u16,
    pub size: u16,
}

// --- Syscall wrapper ---

pub fn perf_event_open(
    attr: &PerfEventAttr,
    pid: pid_t,
    cpu: c_int,
    group_fd: c_int,
    flags: c_ulong,
) -> c_int {
    unsafe {
        syscall(
            SYS_perf_event_open,
            attr as *const _ as *const libc::c_void,
            pid,
            cpu,
            group_fd,
            flags,
        ) as c_int
    }
}

pub fn perf_event_ioctl(fd: c_int, request: c_ulong) -> c_int {
    unsafe { libc::ioctl(fd, request as _) }
}
