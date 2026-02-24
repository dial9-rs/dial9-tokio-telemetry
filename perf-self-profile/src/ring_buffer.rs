//! Lock-free ring buffer consumer for the perf mmap'd region.
//!
//! The kernel writes records into a circular buffer. We read `data_head` (volatile),
//! parse records, then advance `data_tail` to tell the kernel we're done.

use std::mem;
use std::ptr;
use std::sync::atomic::{fence, Ordering};

use crate::sys::{PerfEventHeader, PerfEventMmapPage};

/// A mapped perf ring buffer.
pub struct RingBuffer {
    /// Pointer to the mmap'd region (metadata page + data pages).
    base: *mut u8,
    /// Size of the data region only (excluding the metadata page).
    data_size: u64,
    /// Total mmap size (metadata page + data pages), for munmap.
    mmap_size: usize,
    /// Our current read position.
    position: u64,
}

// Safety: The ring buffer pointer is only accessed by the owner thread,
// and the kernel side is synchronized via data_head/data_tail with fences.
unsafe impl Send for RingBuffer {}

impl RingBuffer {
    /// Create a new RingBuffer from an already-mmap'd pointer.
    ///
    /// # Safety
    /// `base` must be a valid mmap'd perf event region with `mmap_size` bytes.
    pub unsafe fn new(base: *mut u8, data_size: u64, mmap_size: usize) -> Self {
        RingBuffer {
            base,
            data_size,
            mmap_size,
            position: 0,
        }
    }

    /// Returns true if there are unread records in the buffer.
    pub fn has_data(&self) -> bool {
        let head = self.read_head();
        head != self.position
    }

    /// Iterate over all pending records. Each record is provided as a `RawRecord`.
    /// After the callback returns for each record, the ring buffer tail is advanced.
    pub fn for_each_record<F>(&mut self, mut f: F)
    where
        F: FnMut(RawRecord<'_>),
    {
        loop {
            let head = self.read_head();
            if head == self.position {
                break;
            }

            let data = self.data_slice();
            let pos = (self.position % self.data_size) as usize;

            let header: PerfEventHeader = if pos + mem::size_of::<PerfEventHeader>() <= data.len()
            {
                unsafe { ptr::read_unaligned(data.as_ptr().add(pos) as *const PerfEventHeader) }
            } else {
                let mut buf = [0u8; mem::size_of::<PerfEventHeader>()];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = data[(pos + i) % data.len()];
                }
                unsafe { ptr::read_unaligned(buf.as_ptr() as *const PerfEventHeader) }
            };

            let record_size = header.size as usize;
            let body_offset = mem::size_of::<PerfEventHeader>();
            let body_size = record_size - body_offset;
            let body_start = (pos + body_offset) % data.len();

            let record = if body_start + body_size <= data.len() {
                RawRecord {
                    header,
                    body: RecordBody::Contiguous(&data[body_start..body_start + body_size]),
                }
            } else {
                RawRecord {
                    header,
                    body: RecordBody::Split(
                        &data[body_start..],
                        &data[..body_size - (data.len() - body_start)],
                    ),
                }
            };

            f(record);

            self.position += record_size as u64;
            self.write_tail(self.position);
        }
    }

    fn read_head(&self) -> u64 {
        unsafe {
            let page = &*(self.base as *const PerfEventMmapPage);
            let head = ptr::read_volatile(&page.data_head);
            fence(Ordering::Acquire);
            head
        }
    }

    fn write_tail(&self, value: u64) {
        unsafe {
            let page = &mut *(self.base as *mut PerfEventMmapPage);
            fence(Ordering::Release);
            ptr::write_volatile(&mut page.data_tail, value);
        }
    }

    fn data_slice(&self) -> &[u8] {
        unsafe {
            let data_ptr = self.base.add(4096); // skip metadata page
            std::slice::from_raw_parts(data_ptr, self.data_size as usize)
        }
    }
}

impl Drop for RingBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.mmap_size);
        }
    }
}

/// A raw record read from the ring buffer, before parsing.
pub struct RawRecord<'a> {
    pub header: PerfEventHeader,
    pub body: RecordBody<'a>,
}

/// The body of a record, which may be contiguous or split across the ring buffer wrap point.
pub enum RecordBody<'a> {
    Contiguous(&'a [u8]),
    Split(&'a [u8], &'a [u8]),
}

impl<'a> RecordBody<'a> {
    /// Read a u64 at the given byte offset within the body.
    pub fn read_u64(&self, offset: usize) -> u64 {
        let mut buf = [0u8; 8];
        self.read_bytes(offset, &mut buf);
        u64::from_ne_bytes(buf)
    }

    /// Read a u32 at the given byte offset within the body.
    pub fn read_u32(&self, offset: usize) -> u32 {
        let mut buf = [0u8; 4];
        self.read_bytes(offset, &mut buf);
        u32::from_ne_bytes(buf)
    }

    /// Copy `dst.len()` bytes from the given offset into `dst`.
    pub fn read_bytes(&self, offset: usize, dst: &mut [u8]) {
        match self {
            RecordBody::Contiguous(data) => {
                dst.copy_from_slice(&data[offset..offset + dst.len()]);
            }
            RecordBody::Split(first, second) => {
                for (i, b) in dst.iter_mut().enumerate() {
                    let pos = offset + i;
                    if pos < first.len() {
                        *b = first[pos];
                    } else {
                        *b = second[pos - first.len()];
                    }
                }
            }
        }
    }

    /// Total body length in bytes.
    pub fn len(&self) -> usize {
        match self {
            RecordBody::Contiguous(data) => data.len(),
            RecordBody::Split(first, second) => first.len() + second.len(),
        }
    }
}
