//! Framebuffer ownership + pan-buffer scaffolding.
//!
//! Two layers, intentionally separable:
//!
//! 1. [`fill_pan_buffer`] and helpers — pure functions that build the SWTCON
//!    cell preamble and per-row scaffolding. The strip encoder only writes
//!    the low 16 bits of each cell; the high 16 bits are static panel
//!    control flags set here once at initialization. Host-testable.
//!
//! 2. [`Framebuffer`] — Linux-only. Owns `/dev/fb0`'s file descriptor +
//!    mmap region. Wraps the FBIO* ioctls used to pan/blank/unblank. RAII:
//!    Drop munmaps and closes.

use crate::constants::{PAN_BUFFERS_COUNT, PAN_BUFFER_SIZE, PAN_LINE_SIZE, SCREEN_WIDTH};

extern crate alloc;
use alloc::vec::Vec;

// -----------------------------------------------------------------------
// Pan-buffer fill — pure, host-testable.
// -----------------------------------------------------------------------

/// Number of u32 cells per content row.
const CELLS_PER_LINE: usize = PAN_LINE_SIZE / 4;

/// Number of preamble rows at the top of each pan-buffer slot before
/// content rows begin.
pub const PREAMBLE_ROWS: usize = 4;

/// OR `value` into `line[start .. start + length]`.
fn or_in_range(line: &mut [u32], value: u32, start: usize, length: usize) {
    for cell in &mut line[start..start + length] {
        *cell |= value;
    }
}

/// Fill the very first row of a pan-buffer slot — special preamble.
pub fn fill_first_line(line: &mut [u32]) {
    debug_assert!(line.len() >= CELLS_PER_LINE);
    for cell in &mut line[..CELLS_PER_LINE] {
        *cell = 0x0043_0000;
    }
    or_in_range(line, 0x0004_0000, 20, 123);
    for cell in &mut line[40..103] {
        *cell &= 0xfffd_ffff;
    }
}

/// Fill any other row. Pass `value=None` for the secondary preamble rows
/// (rows 1..=3) and `Some(pixel_word)` for content rows (row 3 + per-column
/// content rows).
pub fn fill_line(line: &mut [u32], value: Option<u32>) {
    debug_assert!(line.len() >= CELLS_PER_LINE);
    for cell in &mut line[..CELLS_PER_LINE] {
        *cell = 0x0041_0000;
    }
    or_in_range(line, 0x0020_0000, 8, 0xb);
    or_in_range(line, 0x0002_0000, 0x37, 200);

    if let Some(v) = value {
        or_in_range(line, 0x0010_0000, 0x1a, 0xea);
        or_in_range(line, v & 0xffff, 0x1a, 0xea);
    }
}

/// Initialize one pan-buffer slot: preamble rows + content rows. Each
/// content row is a copy of row 3 (which itself is `fill_line(value=Some)`).
///
/// `buffer` must be at least `PAN_BUFFER_SIZE * PAN_LINE_SIZE` bytes and
/// 4-byte aligned (it normally is, since it's an mmap'd page).
pub fn fill_pan_buffer(buffer: &mut [u8], value: u32) {
    let needed = PAN_BUFFER_SIZE * PAN_LINE_SIZE;
    debug_assert!(buffer.len() >= needed,
        "pan buffer too small: {} < {}", buffer.len(), needed);

    // SAFETY: alignment of 4 is required; mmap returns page-aligned, which
    // is >= 4. Caller-provided buffers in tests use `Vec<u8>` which is
    // aligned to alignof(usize) >= 4 on all supported platforms.
    let cells: &mut [u32] = unsafe {
        let ptr = buffer.as_mut_ptr() as *mut u32;
        debug_assert_eq!(ptr.align_offset(4), 0, "pan buffer not 4-byte aligned");
        core::slice::from_raw_parts_mut(ptr, needed / 4)
    };

    // Row 0: special preamble.
    fill_first_line(&mut cells[0..CELLS_PER_LINE]);
    // Rows 1, 2: secondary preamble — no value-overlay.
    fill_line(&mut cells[CELLS_PER_LINE..2 * CELLS_PER_LINE], None);
    fill_line(&mut cells[2 * CELLS_PER_LINE..3 * CELLS_PER_LINE], None);

    // Row 3: content row template — fill_line(value).
    fill_line(&mut cells[3 * CELLS_PER_LINE..4 * CELLS_PER_LINE], Some(value));

    // Rows 4..(4 + SCREEN_WIDTH): copy row 3 into each per-column row.
    let (template_area, rest) = cells.split_at_mut(4 * CELLS_PER_LINE);
    let template = &template_area[3 * CELLS_PER_LINE..];
    for line_idx in 0..SCREEN_WIDTH {
        let dst_start = line_idx * CELLS_PER_LINE;
        rest[dst_start..dst_start + CELLS_PER_LINE].copy_from_slice(template);
    }
}

/// Convenience constructor: produce a freshly-initialized zero pan buffer
/// as an owned `Vec<u8>`.
pub fn allocate_zero_pan_buffer() -> Vec<u8> {
    let mut buf = vec![0u8; PAN_BUFFER_SIZE * PAN_LINE_SIZE];
    fill_pan_buffer(&mut buf, 0);
    buf
}

// -----------------------------------------------------------------------
// Storage trait — abstracts slot read/write + ioctl ops so the runtime
// can be tested with a mock backend.
// -----------------------------------------------------------------------

use core::ptr::NonNull;

/// Operations the runtime needs from a framebuffer-like backend.
///
/// Implementors must be `Send + Sync` because both the generator and
/// vsync threads share an `Arc<dyn FbStorage>`. The slot-pointer methods
/// return raw `*mut u8` so per-slot access can be split between threads
/// without going through a mutex; the slot-ring back-pressure invariant
/// (`last_pan_phase + 1 ≤ current_pan_phase + ring_gap`) is what
/// guarantees the generator and vsync never touch the same slot at
/// the same time.
pub trait FbStorage: Send + Sync {
    /// Bytes per slot.
    fn slot_size(&self) -> usize;

    /// Number of pan-buffer slots (typically `pan_buffers + 1`, where the
    /// extra is the "spare" slot used during idle).
    fn slot_count(&self) -> u32;

    /// Raw pointer to slot `idx`. Length is exactly [`slot_size`].
    ///
    /// # Safety
    /// Caller guarantees no other thread is concurrently reading or
    /// writing to slot `idx` for the duration of any access through the
    /// returned pointer.
    unsafe fn slot_ptr(&self, idx: u32) -> NonNull<u8>;

    /// Pan to slot `idx`. On a real device, may EINVAL while blanked —
    /// see [`unblank`](Self::unblank).
    fn pan(&self, idx: u32) -> Result<(), FbError>;

    /// Blank the panel.
    fn blank(&self) -> Result<(), FbError>;

    /// Unblank to slot `idx`. Required at least once before [`pan`] on
    /// the rM2 driver.
    fn unblank(&self, idx: u32) -> Result<(), FbError>;
}

/// Errors from framebuffer operations. Defined here (not under `cfg(linux)`)
/// so the trait can be referenced in non-Linux test compilation paths.
#[derive(Debug)]
pub enum FbError {
    Open(std::io::Error),
    Ioctl { name: &'static str, source: std::io::Error },
    Mmap(std::io::Error),
    /// Used by mock backends; never produced by the Linux impl.
    Mock(&'static str),
}

impl core::fmt::Display for FbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FbError::Open(e) => write!(f, "fb open: {e}"),
            FbError::Ioctl { name, source } => write!(f, "fb ioctl {name}: {source}"),
            FbError::Mmap(e) => write!(f, "fb mmap: {e}"),
            FbError::Mock(s) => write!(f, "fb mock: {s}"),
        }
    }
}

impl std::error::Error for FbError {}

// -----------------------------------------------------------------------
// Linux framebuffer wrapper — owns fd + mmap, RAII'd via Drop.
// -----------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub use linux_fb::Framebuffer;

#[cfg(target_os = "linux")]
mod linux_fb {
    use super::*;
    use core::ptr::NonNull;
    use std::ffi::CString;
    use std::path::Path;
    use std::sync::Mutex;

    use libc::{
        c_int, c_ulong, c_void, close, ioctl, mmap, munmap, open,
        MAP_FAILED, MAP_SHARED, O_RDWR, PROT_READ, PROT_WRITE,
    };

    // Standard Linux fb subsystem ioctl request numbers. See
    // <linux/fb.h>. Values are stable across kernel versions.
    const FBIOGET_VSCREENINFO: c_ulong = 0x4600;
    const FBIOPUT_VSCREENINFO: c_ulong = 0x4601;
    const FBIOGET_FSCREENINFO: c_ulong = 0x4602;
    const FBIOPAN_DISPLAY:     c_ulong = 0x4606;
    const FBIOBLANK:           c_ulong = 0x4611;

    /// Subset of `struct fb_var_screeninfo` from `<linux/fb.h>`. Layout
    /// must match the kernel's exactly — every field, in order. Padding
    /// or reordering corrupts the ioctl payload.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FbVarScreenInfo {
        xres: u32,
        yres: u32,
        xres_virtual: u32,
        yres_virtual: u32,
        xoffset: u32,
        yoffset: u32,
        bits_per_pixel: u32,
        grayscale: u32,
        red: FbBitfield,
        green: FbBitfield,
        blue: FbBitfield,
        transp: FbBitfield,
        nonstd: u32,
        activate: u32,
        height: u32,
        width: u32,
        accel_flags: u32,
        pixclock: u32,
        left_margin: u32,
        right_margin: u32,
        upper_margin: u32,
        lower_margin: u32,
        hsync_len: u32,
        vsync_len: u32,
        sync: u32,
        vmode: u32,
        rotate: u32,
        colorspace: u32,
        reserved: [u32; 4],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FbBitfield {
        offset: u32,
        length: u32,
        msb_right: u32,
    }

    /// Subset of `struct fb_fix_screeninfo` — we only fetch it to satisfy
    /// the kernel's expectations during open; we don't use any fields.
    #[repr(C)]
    #[derive(Default)]
    struct FbFixScreenInfo {
        id: [u8; 16],
        smem_start: usize,
        smem_len: u32,
        type_: u32,
        type_aux: u32,
        visual: u32,
        xpanstep: u16,
        ypanstep: u16,
        ywrapstep: u16,
        line_length: u32,
        mmio_start: usize,
        mmio_len: u32,
        accel: u32,
        capabilities: u16,
        reserved: [u16; 2],
    }

    /// RAII wrapper for `/dev/fb0`. Owns one fd and one mmap region of
    /// `(pan_buffers + 1) * PAN_BUFFER_SIZE * PAN_LINE_SIZE` bytes.
    /// The `+1` slot is a "spare" the panel hardware can rotate to during
    /// idle.
    pub struct Framebuffer {
        fd: c_int,
        map: NonNull<u8>,
        map_len: usize,
        // Interior mutability so pan/blank/unblank can take &self and the
        // struct can live behind an `Arc`. The mutex is held only briefly
        // around each ioctl — never across encoder work.
        var_info: Mutex<FbVarScreenInfo>,
        pan_buffers: u32,
    }

    // SAFETY: the raw pointer + fd are stable for the lifetime of self;
    // any aliasing concerns are pushed to the FbStorage::slot_ptr safety
    // contract, which the runtime upholds via slot-ring back-pressure.
    unsafe impl Send for Framebuffer {}
    unsafe impl Sync for Framebuffer {}

    impl Framebuffer {
        /// Open `path` (typically `/dev/fb0`), configure the var-screeninfo
        /// for SWTCON's pan-buffer geometry, mmap the framebuffer, and
        /// initialize all slots to a zero pattern.
        pub fn open(path: &Path, pan_buffers: u32) -> Result<Self, FbError> {
            let pan_count = pan_buffers + 1;
            let map_len = (pan_count as usize) * PAN_BUFFER_SIZE * PAN_LINE_SIZE;

            let cpath = CString::new(path.as_os_str().as_encoded_bytes())
                .map_err(|e| FbError::Open(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput, e)))?;

            // SAFETY: cpath is null-terminated; flags valid.
            let fd = unsafe { open(cpath.as_ptr(), O_RDWR) };
            if fd < 0 {
                return Err(FbError::Open(std::io::Error::last_os_error()));
            }

            // Wrap the fd ASAP so it gets closed if any subsequent step fails.
            let mut guard = FdGuard(fd);

            // FBIOGET_FSCREENINFO — we don't use the result but the kernel
            // expects the call (some drivers assume both fix and var have
            // been queried before PUT_VSCREENINFO).
            let mut fix = FbFixScreenInfo::default();
            // SAFETY: fd is open, fix is a stack-allocated struct of the
            // expected layout, ioctl request matches the kernel's expectation.
            if unsafe { ioctl(fd, FBIOGET_FSCREENINFO as _, &mut fix as *mut _) } < 0 {
                return Err(FbError::Ioctl {
                    name: "FBIOGET_FSCREENINFO",
                    source: std::io::Error::last_os_error(),
                });
            }

            let mut var = FbVarScreenInfo::default();
            if unsafe { ioctl(fd, FBIOGET_VSCREENINFO as _, &mut var as *mut _) } < 0 {
                return Err(FbError::Ioctl {
                    name: "FBIOGET_VSCREENINFO",
                    source: std::io::Error::last_os_error(),
                });
            }

            // SWTCON-specific geometry.
            var.yres = PAN_BUFFER_SIZE as u32;
            var.yres_virtual = pan_count * PAN_BUFFER_SIZE as u32;
            var.yoffset = (PAN_BUFFER_SIZE * PAN_BUFFERS_COUNT) as u32;
            var.xres = (PAN_LINE_SIZE / 4) as u32;
            var.xres_virtual = (PAN_LINE_SIZE / 4) as u32;
            var.xoffset = 0;
            var.pixclock = 0x7080;
            var.lower_margin = 143;
            var.upper_margin = 1;
            var.left_margin = 1;
            var.right_margin = 1;
            var.hsync_len = 1;
            var.vsync_len = 1;
            var.bits_per_pixel = 32;

            // FBIOPUT_VSCREENINFO writes back the values the kernel actually
            // committed (it may re-quantize). Pass a mutable pointer so we
            // capture the post-commit state for subsequent FBIOPAN_DISPLAY
            // calls.
            if unsafe { ioctl(fd, FBIOPUT_VSCREENINFO as _, &mut var as *mut _) } < 0 {
                return Err(FbError::Ioctl {
                    name: "FBIOPUT_VSCREENINFO",
                    source: std::io::Error::last_os_error(),
                });
            }

            // SAFETY: ptr/len validated against MAP_FAILED below; PROT_*/MAP_*
            // and offset are correct for an fb device. fd is open.
            let raw = unsafe {
                mmap(core::ptr::null_mut(), map_len, PROT_READ | PROT_WRITE,
                     MAP_SHARED, fd, 0)
            };
            if raw == MAP_FAILED {
                return Err(FbError::Mmap(std::io::Error::last_os_error()));
            }
            let map = NonNull::new(raw as *mut u8)
                .expect("MAP_FAILED already checked");

            // Initialize every slot to a zero pan buffer.
            let zero = allocate_zero_pan_buffer();
            // SAFETY: map is valid for `map_len` bytes; we only read/write
            // within it.
            unsafe {
                for i in 0..pan_count as usize {
                    let dst = map.as_ptr().add(i * PAN_BUFFER_SIZE * PAN_LINE_SIZE);
                    core::ptr::copy_nonoverlapping(zero.as_ptr(), dst, zero.len());
                }
            }

            // Successful open: defuse the guard so Drop only runs from Self.
            guard.defuse();
            Ok(Self {
                fd,
                map,
                map_len,
                var_info: Mutex::new(var),
                pan_buffers,
            })
        }
    }

    impl FbStorage for Framebuffer {
        fn slot_size(&self) -> usize { PAN_BUFFER_SIZE * PAN_LINE_SIZE }
        fn slot_count(&self) -> u32 { self.pan_buffers + 1 }

        unsafe fn slot_ptr(&self, idx: u32) -> NonNull<u8> {
            assert!(idx < self.pan_buffers + 1, "slot {idx} out of range");
            // SAFETY: idx bounds checked; map is valid for map_len bytes;
            // PAN_BUFFER_SIZE * PAN_LINE_SIZE * (pan_buffers+1) <= map_len.
            unsafe {
                NonNull::new_unchecked(
                    self.map.as_ptr().add(idx as usize * PAN_BUFFER_SIZE * PAN_LINE_SIZE)
                )
            }
        }

        /// Pan to slot `idx`. Wakes the panel hardware to consume the slot.
        ///
        /// **rM2 driver quirk:** the FBIOPAN_DISPLAY ioctl is rejected with
        /// `EINVAL` while the panel is in the blanked state. Callers must
        /// invoke [`unblank`](Self::unblank) at least once before any pan.
        fn pan(&self, idx: u32) -> Result<(), FbError> {
            let mut var = self.var_info.lock().unwrap();
            var.yoffset = idx * PAN_BUFFER_SIZE as u32;
            // SAFETY: fd open, struct fully initialized, request stable.
            if unsafe { ioctl(self.fd, FBIOPAN_DISPLAY as _, &*var as *const _) } < 0 {
                return Err(FbError::Ioctl {
                    name: "FBIOPAN_DISPLAY",
                    source: std::io::Error::last_os_error(),
                });
            }
            Ok(())
        }

        fn blank(&self) -> Result<(), FbError> {
            // SAFETY: fd open. FBIOBLANK takes its argument inline, not by ptr.
            if unsafe { ioctl(self.fd, FBIOBLANK as _, 3 as *const c_void) } < 0 {
                return Err(FbError::Ioctl {
                    name: "FBIOBLANK(3)",
                    source: std::io::Error::last_os_error(),
                });
            }
            Ok(())
        }

        /// Unblank to slot `idx`. Sets pan offset, then issues unblank up to
        /// 5 times — the ioctl can transiently fail under load.
        fn unblank(&self, idx: u32) -> Result<(), FbError> {
            let mut var = self.var_info.lock().unwrap();
            var.yoffset = idx * PAN_BUFFER_SIZE as u32;
            if unsafe { ioctl(self.fd, FBIOPUT_VSCREENINFO as _, &mut *var as *mut _) } < 0 {
                return Err(FbError::Ioctl {
                    name: "FBIOPUT_VSCREENINFO (unblank pan)",
                    source: std::io::Error::last_os_error(),
                });
            }

            let mut last_err: Option<std::io::Error> = None;
            for _ in 0..5 {
                if unsafe { ioctl(self.fd, FBIOBLANK as _, 0 as *const c_void) } >= 0 {
                    return Ok(());
                }
                last_err = Some(std::io::Error::last_os_error());
            }
            Err(FbError::Ioctl {
                name: "FBIOBLANK(0)",
                source: last_err.unwrap_or_else(|| std::io::Error::other("unblank gave up")),
            })
        }
    }

    impl Framebuffer {
        /// Convenience for callers that already have `&mut Framebuffer` and
        /// need a slot slice with no aliasing concerns. Equivalent to
        /// `slot_ptr` but returns a typed slice and bounds-checks for free.
        pub fn slot_mut_via_unique(&mut self, idx: u32) -> &mut [u8] {
            assert!(idx < self.pan_buffers + 1, "slot {idx} out of range");
            // SAFETY: &mut self proves no other slot view exists.
            unsafe {
                core::slice::from_raw_parts_mut(
                    self.map.as_ptr().add(idx as usize * PAN_BUFFER_SIZE * PAN_LINE_SIZE),
                    PAN_BUFFER_SIZE * PAN_LINE_SIZE,
                )
            }
        }

        /// Immutable view of slot `idx`. Useful for diagnostics; mutation
        /// from another thread can race, so use only when you know nothing
        /// else is writing.
        pub fn slot_view(&self, idx: u32) -> &[u8] {
            assert!(idx < self.pan_buffers + 1, "slot {idx} out of range");
            unsafe {
                core::slice::from_raw_parts(
                    self.map.as_ptr().add(idx as usize * PAN_BUFFER_SIZE * PAN_LINE_SIZE),
                    PAN_BUFFER_SIZE * PAN_LINE_SIZE,
                )
            }
        }
    }

    impl Drop for Framebuffer {
        fn drop(&mut self) {
            // SAFETY: map + fd were valid at construction; only freed here.
            unsafe {
                munmap(self.map.as_ptr() as *mut c_void, self.map_len);
                close(self.fd);
            }
        }
    }

    /// Closes `fd` on Drop unless `defuse()` is called. Used during open
    /// to guarantee the fd is freed if a later step fails.
    struct FdGuard(c_int);
    impl FdGuard {
        fn defuse(&mut self) { self.0 = -1; }
    }
    impl Drop for FdGuard {
        fn drop(&mut self) {
            if self.0 >= 0 {
                // SAFETY: we own the fd; ignore close error during cleanup.
                unsafe { close(self.0); }
            }
        }
    }
}
