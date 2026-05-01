//! Public Rust-native API. Composes [`crate::fb::Framebuffer`] +
//! [`crate::waveform::WaveformPack`] + [`crate::runtime::Runtime`] into
//! a single ergonomic [`Swtcon`] type.

use core::sync::atomic::{AtomicI32, Ordering};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::constants::{PAN_BUFFER_SIZE, PAN_BUFFERS_COUNT, PAN_LINE_SIZE, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::fb::{FbError, FbStorage, Framebuffer, fill_pan_buffer};
use crate::runtime::{PanelRect, Runtime, UpdateMsg};
use crate::waveform::{InitPhases, WaveformPack, standard_xochitl_modes};
use crate::wbf::{Wbf, WbfError};

/// User-facing waveform tier. Each variant maps to one of the WBF
/// modes that [`standard_xochitl_modes`] loads into the [`WaveformPack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 1-bit black/white. Fast. The waveform table only has defined
    /// commands for `(src, tgt) ∈ {0, 15}^2`; intermediate-gray targets
    /// get cmd=0 (no panel drive). Use only with pure black/white content.
    /// (WBF mode 1.)
    Du,
    /// 16-level grayscale, full quality. ~46 phases at room temperature.
    /// (WBF mode 2.)
    Gc16,
    /// Faster grayscale than [`Gc16`](Self::Gc16) but with more visible
    /// ghosting on re-painted areas. (WBF mode 3.)
    Gc16Fast,
    /// General-purpose / glide. (WBF mode 6.)
    Glf,
    /// 4-level grayscale, fast. (WBF mode 7.)
    Du4,
    /// "Animation" 1-bit waveform — the fastest mode the panel
    /// supports, ~50–100 ms per refresh. Some ghosting on pixels that
    /// were grayscale before. Ideal for active pen-stroke updates.
    /// (WBF mode 4.)
    A2,
}

impl Mode {
    /// Index into `WaveformPack::modes` for this mode (assumes the pack
    /// was built from [`standard_xochitl_modes`]).
    fn pack_index(self) -> usize {
        match self {
            Mode::Du       => 0, // WBF mode 1, no skipInit
            Mode::Gc16     => 1, // WBF mode 2, separate partial, no skipInit
            Mode::Gc16Fast => 2, // WBF mode 3, separate partial, no skipInit
            Mode::Glf      => 3, // WBF mode 6
            Mode::Du4      => 4, // WBF mode 7
            Mode::A2       => 8, // WBF mode 4
        }
    }
}

/// User-coordinate rectangle in screen orientation (1404 wide x 1872
/// tall, origin top-left). Inclusive bottom-right.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x1: u16,
    pub y1: u16,
    pub x2: u16,
    pub y2: u16,
}

impl Rect {
    pub fn full() -> Self {
        Self { x1: 0, y1: 0,
               x2: SCREEN_WIDTH as u16 - 1,
               y2: SCREEN_HEIGHT as u16 - 1 }
    }
}

#[derive(Debug)]
pub enum Error {
    Fb(FbError),
    Wbf(WbfError),
    Io(std::io::Error),
    /// Image buffer length doesn't match `SCREEN_WIDTH * SCREEN_HEIGHT`.
    BufferSize { expected: usize, got: usize },
    /// (mode, temp) lookup landed outside the loaded waveform pack.
    NoWaveform { mode_idx: usize, temp_idx: usize },
    /// Returned by [`Swtcon::flush`] if the timeout elapses with msgs
    /// still pending.
    FlushTimeout,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Fb(e)      => write!(f, "{e}"),
            Error::Wbf(e)     => write!(f, "{e}"),
            Error::Io(e)      => write!(f, "io: {e}"),
            Error::BufferSize { expected, got } =>
                write!(f, "image buffer length mismatch: expected {expected}, got {got}"),
            Error::NoWaveform { mode_idx, temp_idx } =>
                write!(f, "no waveform for mode_idx={mode_idx} temp_idx={temp_idx}"),
            Error::FlushTimeout => write!(f, "flush timed out"),
        }
    }
}

impl std::error::Error for Error {}

impl From<FbError>         for Error { fn from(e: FbError)         -> Self { Error::Fb(e) } }
impl From<WbfError>        for Error { fn from(e: WbfError)        -> Self { Error::Wbf(e) } }
impl From<std::io::Error>  for Error { fn from(e: std::io::Error)  -> Self { Error::Io(e) } }

/// Top-level handle. Owns the framebuffer, the loaded waveform pack,
/// and the generator + vsync threads.
///
/// On drop: the runtime joins both threads (which may take up to a few
/// seconds if there are msgs in flight).
pub struct Swtcon {
    runtime: Runtime,
    waveforms: Arc<WaveformPack>,
    current_temp: AtomicI32,
}

impl Swtcon {
    /// Open the framebuffer, parse the WBF, run the INIT clear, and
    /// spawn the generator + vsync threads.
    ///
    /// Reads the SY7636A PMIC temperature sensor (via hwmon) to pick
    /// the temperature index; falls back to index 7 (~24 °C, room temp)
    /// if the sensor isn't present or can't be read.
    pub fn open(fb_path: &Path, wbf_path: &Path) -> Result<Self, Error> {
        let fb = Arc::new(Framebuffer::open(fb_path, PAN_BUFFERS_COUNT as u32)?);

        let wbf_bytes = std::fs::read(wbf_path)?;
        let wbf = Wbf::parse(&wbf_bytes)?;
        let waveforms = Arc::new(WaveformPack::from_wbf(&wbf, &standard_xochitl_modes())?);

        let temp_idx = pick_temp_idx(&waveforms.temp_ranges);
        run_init_clear(&fb, &waveforms.init[temp_idx])?;

        let runtime = Runtime::new(fb as Arc<dyn FbStorage>);

        Ok(Self { runtime, waveforms, current_temp: AtomicI32::new(temp_idx as i32) })
    }

    /// Submit a panel update. Async — returns once the msg is enqueued.
    /// Use [`flush`](Self::flush) to wait for completion.
    ///
    /// `image` must be exactly `SCREEN_WIDTH * SCREEN_HEIGHT` u16s in
    /// screen orientation (row-major: `image[y * SCREEN_WIDTH + x]`).
    /// The encoder reads each pixel as `(low_byte >> 1) & 0xf` to get a
    /// 4-bit gray value; the rest of the u16 is ignored.
    pub fn update(&self, image: &[u16], rect: Rect, mode: Mode) -> Result<(), Error> {
        self.update_with_limit(image, rect, mode, None)
    }

    /// Like [`update`], but caps the number of waveform phases the
    /// generator emits. The first `phase_limit` phases drive the
    /// panel partway toward the target state — pixels won't be fully
    /// settled, but the visible result appears `phase_count /
    /// phase_limit` × faster. The change-tracking buffer is **not**
    /// committed for phase-limited updates so a subsequent full
    /// update still drives from the original src state.
    ///
    /// Use this for a "preview ink" pass on top of which a later
    /// settled update cleans up the partial transitions.
    pub fn update_with_limit(
        &self,
        image: &[u16],
        rect: Rect,
        mode: Mode,
        phase_limit: Option<u32>,
    ) -> Result<(), Error> {
        let expected = SCREEN_WIDTH * SCREEN_HEIGHT;
        if image.len() != expected {
            return Err(Error::BufferSize { expected, got: image.len() });
        }

        let temp = self.current_temp.load(Ordering::SeqCst) as usize;
        let mode_idx = mode.pack_index();
        let mode_wf = self.waveforms.modes.get(mode_idx)
            .ok_or(Error::NoWaveform { mode_idx, temp_idx: temp })?;
        let phase_table = mode_wf.partial.get(temp)
            .ok_or(Error::NoWaveform { mode_idx, temp_idx: temp })?
            .clone();

        let (panel_rect, buffer, width) = build_msg_buffer(image, rect);
        self.runtime.submit(UpdateMsg {
            rect: panel_rect,
            buffer,
            width,
            waveform: phase_table,
            phase_limit,
        });
        Ok(())
    }

    /// Block until all pending msgs have been encoded AND vsync has
    /// drained the slot ring.
    pub fn flush(&self, timeout: Duration) -> Result<(), Error> {
        if self.runtime.flush(timeout) { Ok(()) } else { Err(Error::FlushTimeout) }
    }

    /// Number of msgs queued but not yet drained by the generator.
    pub fn pending(&self) -> usize { self.runtime.state.pending_count() }

    /// Direct access to the loaded waveform pack — useful for diagnostics
    /// or for callers that want phase counts at a given temperature.
    pub fn waveforms(&self) -> &WaveformPack { &self.waveforms }
}

// ---------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------

/// Pick a temperature index for the WBF waveform tables, based on the
/// SY7636A PMIC sensor reading. Falls back to index 7 (~24 °C, room
/// temp) if the sensor isn't present or can't be read.
fn pick_temp_idx(temp_ranges: &[(u8, u8)]) -> usize {
    const FALLBACK: usize = 7;
    let temp_c = match read_pmic_temperature() {
        Some(t) => t,
        None => {
            log::warn!("swtcon: SY7636A temp sensor not found; using idx {FALLBACK}");
            return FALLBACK.min(temp_ranges.len().saturating_sub(1));
        }
    };
    // Each range is (lo, hi); the WBF convention is "lo inclusive, hi
    // exclusive" except the last which extends to 100. Pick the first
    // range whose hi exceeds our reading.
    for (i, (lo, hi)) in temp_ranges.iter().enumerate() {
        if temp_c < *hi as i32 || i == temp_ranges.len() - 1 {
            log::info!(
                "swtcon: SY7636A reads {}°C → temp_idx={i} (range {}..{})",
                temp_c, lo, hi,
            );
            return i;
        }
    }
    FALLBACK.min(temp_ranges.len().saturating_sub(1))
}

/// Walk `/sys/class/hwmon/*` looking for a device named
/// `sy7636a_temperature`, then read its `temp0` file (plain integer
/// degrees Celsius). Returns `None` on any failure.
fn read_pmic_temperature() -> Option<i32> {
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = std::fs::read_to_string(path.join("name")).ok()?;
        if name.trim() == "sy7636a_temperature" {
            let raw = std::fs::read_to_string(path.join("temp0")).ok()?;
            return raw.trim().parse::<i32>().ok();
        }
    }
    None
}

/// Run the INIT clear sequence: fill 3 slots with init patterns, walk
/// through the per-phase pan codes, blank, then zero the slots back.
fn run_init_clear(fb: &Framebuffer, init: &InitPhases) -> Result<(), Error> {
    if init.pan_codes.is_empty() {
        return Ok(()); // nothing to do
    }

    // Fill 3 slots with the panel's init-mode patterns. The panel
    // hardware cycles through these as the init waveform drives. SAFETY:
    // we have exclusive access to the fb here (no threads spawned yet).
    let slot_size = fb.slot_size();
    let mut s0 = vec![0u8; slot_size];
    let mut s1 = vec![0u8; slot_size];
    let mut s2 = vec![0u8; slot_size];
    fill_pan_buffer(&mut s0, 0);
    fill_pan_buffer(&mut s1, 0x5555);
    fill_pan_buffer(&mut s2, 0xaaaa);
    unsafe {
        core::ptr::copy_nonoverlapping(s0.as_ptr(), fb.slot_ptr(0).as_ptr(), slot_size);
        core::ptr::copy_nonoverlapping(s1.as_ptr(), fb.slot_ptr(1).as_ptr(), slot_size);
        core::ptr::copy_nonoverlapping(s2.as_ptr(), fb.slot_ptr(2).as_ptr(), slot_size);
    }

    // Drive the init waveform: unblank to first phase, then pan through
    // each subsequent phase, then idle + blank.
    fb.unblank((init.pan_codes[0] & 0xf) as u32)?;
    fb.pan(PAN_BUFFERS_COUNT as u32)?;

    for &code in init.pan_codes.iter().skip(1) {
        fb.pan((code & 0xf) as u32)?;
    }

    fb.pan(PAN_BUFFERS_COUNT as u32)?;
    fb.blank()?;

    // Zero the three init slots back to the standard zero pan-buffer
    // pattern so subsequent generator writes start from a clean base.
    let zero = {
        let mut b = vec![0u8; slot_size];
        fill_pan_buffer(&mut b, 0);
        b
    };
    unsafe {
        for slot in 0..3 {
            core::ptr::copy_nonoverlapping(
                zero.as_ptr(), fb.slot_ptr(slot).as_ptr(), slot_size);
        }
    }

    Ok(())
}

/// Convert a user-space rect + image into the panel-coord rect + the
/// 4-bit packed buffer the encoder expects.
///
/// The panel hardware is rotated 90° from the user's portrait view:
///
/// ```text
///     user.x (horizontal, 0..=1403) → panel column (1403 - x)
///     user.y (vertical,   0..=1871) → panel row    (1871 - y)
/// ```
///
/// Both axes also flip end-for-end. So the user rect's "max" corner
/// becomes the panel rect's "min" corner and vice versa. The encoder
/// requires the panel-row range to be 8-aligned (it packs 8 rows per
/// cell), so we round outward.
fn build_msg_buffer(image: &[u16], r: Rect) -> (PanelRect, Vec<u8>, u16) {
    const MAX_COL: i32 = SCREEN_WIDTH  as i32 - 1;  // 1403
    const MAX_ROW: i32 = SCREEN_HEIGHT as i32 - 1;  // 1871

    let col_min = (MAX_COL - r.x2 as i32).clamp(0, MAX_COL) as u16;
    let col_max = (MAX_COL - r.x1 as i32).clamp(0, MAX_COL) as u16;
    let row_min = ((MAX_ROW - r.y2 as i32).clamp(0, MAX_ROW) & !7) as u16;
    let row_max = ((MAX_ROW - r.y1 as i32).clamp(0, MAX_ROW) | 7) as u16;

    let width  = row_max - row_min + 1;
    let height = col_max - col_min + 1;
    let mut buffer = vec![0u8; (width as usize) * (height as usize)];

    // Buffer layout: buffer[col_offset * width + row_offset], where
    // col_offset = panel_col - col_min and row_offset = panel_row - row_min.
    // The encoder reads it back with the same indexing.
    for panel_col in col_min..=col_max {
        for panel_row in row_min..=row_max {
            let user_x = MAX_COL - panel_col as i32;
            let user_y = MAX_ROW - panel_row as i32;
            // Out-of-bounds can occur near rect edges that the alignment
            // expanded past the user's image.
            if !(0..SCREEN_WIDTH as i32).contains(&user_x)
                || !(0..SCREEN_HEIGHT as i32).contains(&user_y)
            {
                continue;
            }
            let pixel = image[user_y as usize * SCREEN_WIDTH + user_x as usize];
            let gray = ((pixel & 0xff) >> 1) & 0xf;
            let buf_idx = (panel_col - col_min) as usize * width as usize
                        + (panel_row - row_min) as usize;
            buffer[buf_idx] = gray as u8;
        }
    }

    (PanelRect { row_min, row_max, col_min, col_max }, buffer, width)
}

/// Silence dead-code warning when constants are only conditionally used.
#[allow(dead_code)]
const _UNUSED: (usize, usize, usize) = (PAN_BUFFER_SIZE, PAN_BUFFERS_COUNT, PAN_LINE_SIZE);
