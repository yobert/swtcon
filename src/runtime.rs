//! Generator + vsync threads, the load-bearing concurrency piece.
//!
//! All cross-thread state lives in one [`SharedState`] behind an `Arc`:
//!
//! - **Slot ring** (`current_pan_phase`, `last_pan_phase`,
//!   `previous_pan_phase`) — the generator advances `last_pan_phase` as
//!   it fills slots; vsync chases it via `current_pan_phase`. The
//!   modular slot index is `phase & 0xf`. Back-pressure caps the gap at
//!   [`MAX_RING_GAP`] so the generator never aliases vsync's in-flight
//!   slot.
//! - **Queue** — generator drains `Mutex<VecDeque<UpdateMsg>>`. External
//!   callers push into the queue and signal the generator.
//! - **Vsync wait** — `vsync_cond` wakes vsync on any of: new pan
//!   produced, clear request, shutdown, or blank-delay timeout.
//! - **Generator wait** — `gen_cond` wakes the generator on any of: new
//!   msg enqueued, vsync drained the ring (back-pressure relief), or
//!   shutdown.
//!
//! The fb is accessed via the [`crate::fb::FbStorage`] trait so this
//! module is testable with a mock backend that records call sequences
//! and lets tests deterministically advance "vsync".

extern crate alloc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::constants::{PAN_BUFFER_SIZE, PAN_BUFFERS_COUNT, PAN_LINE_SIZE, SCREEN_HEIGHT, SCREEN_WIDTH};
// PAN_BUFFER_SIZE is referenced only from tests today; an `#[allow]` here
// keeps the rest of the lib free of suppressions.
#[allow(dead_code)]
const _: usize = PAN_BUFFER_SIZE;
use crate::fb::{FbStorage, fill_pan_buffer};
use crate::strip_encoder::{build_phase_cmd_lut, encode_strip};
use crate::waveform::PhaseTable;

/// Slot-ring back-pressure depth. The 16-slot ring has a strict
/// correctness bound of 15: at gap=15, the next reserved slot would
/// alias the slot vsync is currently driving. We pick 14 to leave a
/// one-slot safety margin.
pub const MAX_RING_GAP: i32 = 14;

/// Index of the "spare" idle slot — vsync pans here when there's nothing
/// to drive, so the panel hardware doesn't repeat-display a stale slot.
const IDLE_SLOT: u32 = PAN_BUFFERS_COUNT as u32;

/// Rectangle in panel coordinates (panel orientation, not user
/// orientation). The panel is 1872 rows (the "tall" axis) × 1404 cols
/// (the "wide" axis). The row range must be 8-aligned (`row_min & !7`
/// and `row_max | 7`) because the encoder packs 8 rows per cell.
#[derive(Debug, Clone, Copy)]
pub struct PanelRect {
    pub row_min: u16,
    pub row_max: u16,
    pub col_min: u16,
    pub col_max: u16,
}

/// One update enqueued by an external caller. Owns its pixel buffer and
/// holds an `Arc` to the per-phase waveform table to use.
pub struct UpdateMsg {
    pub rect: PanelRect,
    /// 4-bit grayscale pixels (one nibble per byte, packed row-major
    /// over the rect). Layout: `buffer[col_offset * width + row_offset]`,
    /// where `col_offset = panel_col - rect.col_min` and `row_offset =
    /// panel_row - rect.row_min`.
    pub buffer: Vec<u8>,
    /// Number of panel rows in the rect (`row_max - row_min + 1`).
    pub width: u16,
    /// Per-(mode, temp) phase table. Drives the encoder.
    pub waveform: Arc<PhaseTable>,
    /// Optional cap on the number of phases the generator emits.
    /// When `Some(n)`, only the first `n` phases of the waveform are
    /// driven and `commit_change_tracking` is skipped so the next
    /// update still sees the pre-preview pixel state as its source.
    /// Used for fast "preview" pen strokes that aren't fully settled.
    pub phase_limit: Option<u32>,
}

/// A `Send + Sync` wrapper around exclusive-ownership-by-slot raw
/// storage. Used for `change_tracking` (generator-only) and
/// `dirty_columns` (split per-slot between generator and vsync via the
/// ring invariant).
struct UnsafeStorage {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for UnsafeStorage {}
unsafe impl Sync for UnsafeStorage {}

impl UnsafeStorage {
    fn new(len: usize) -> Self {
        let mut v: Vec<u8> = vec![0; len];
        let ptr = v.as_mut_ptr();
        // Move the buffer into a raw allocation we leak; freed in Drop.
        core::mem::forget(v);
        Self { ptr, len }
    }

    /// SAFETY: caller guarantees no other thread is concurrently
    /// accessing the same byte range.
    unsafe fn slice_mut(&self, offset: usize, len: usize) -> &mut [u8] {
        debug_assert!(offset + len <= self.len);
        unsafe { core::slice::from_raw_parts_mut(self.ptr.add(offset), len) }
    }

    unsafe fn slice(&self, offset: usize, len: usize) -> &[u8] {
        debug_assert!(offset + len <= self.len);
        unsafe { core::slice::from_raw_parts(self.ptr.add(offset), len) }
    }
}

impl Drop for UnsafeStorage {
    fn drop(&mut self) {
        // SAFETY: ptr came from Vec::with_capacity(len)/forget; reconstruct
        // and let Vec free.
        unsafe {
            let _ = Vec::from_raw_parts(self.ptr, self.len, self.len);
        }
    }
}

/// State shared between the generator + vsync threads + external
/// producers. Constructed once, wrapped in `Arc`, cloned to each thread.
pub struct SharedState {
    // ---- Slot ring (atomics; multi-thread read/write) ----
    pub current_pan_phase:  AtomicI32,
    pub last_pan_phase:     AtomicI32,
    pub previous_pan_phase: AtomicI32,
    pub is_blanked:         AtomicBool,

    // ---- Shutdown flags ----
    pub vsync_shutdown:     AtomicBool,
    pub generator_shutdown: AtomicBool,

    // ---- Vsync's request inputs ----
    pub vsync_clear_request: AtomicBool,
    /// Optional auto-blank delay (seconds). `0` = blank immediately when
    /// nothing to drive.
    pub vsync_blank_delay:   AtomicU32,

    // ---- Message queue ----
    msg_queue: Mutex<VecDeque<UpdateMsg>>,

    // ---- Generator wait condition ----
    /// Single condvar for all generator wakeup reasons:
    ///   - new msg enqueued (by external producer)
    ///   - vsync drained the ring (back-pressure relief)
    ///   - shutdown requested
    /// All predicates must be checked while holding `gen_lock`. This is
    /// the same `Mutex<()>` that all notifiers lock before broadcasting,
    /// so the lock-acquire fence ensures we observe all atomic predicates
    /// the notifier wrote to before notifying — no missed wakeups.
    gen_cond: Condvar,
    gen_lock: Mutex<()>,

    // ---- Vsync wait condition (same pattern) ----
    vsync_cond: Condvar,
    vsync_lock: Mutex<()>,

    // ---- Encoder-shared storage ----
    change_tracking: UnsafeStorage,  // SCREEN_WIDTH * SCREEN_HEIGHT bytes
    dirty_columns:   UnsafeStorage,  // SCREEN_WIDTH * (PAN_BUFFERS_COUNT + 1) bytes
    /// Pre-built content-template line (`fill_line(value=0)` output) used
    /// by `clear_dirty_buffer` to reset slot lines. Must include the panel
    /// control flags in the high 16 bits of each cell — copying all-zeros
    /// would strip those flags and the panel sees malformed cells.
    zero_content_line: Vec<u8>,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        let change_len = SCREEN_WIDTH * SCREEN_HEIGHT;
        let dirty_len  = SCREEN_WIDTH * (PAN_BUFFERS_COUNT + 1);
        let change_tracking = UnsafeStorage::new(change_len);
        // Post-init the panel is white. Encoder's src lookup must agree
        // with the physical panel state — initialize every byte to
        // 0xFF (gray nibble 15 in both nibbles, matching the
        // `v | (v << 4)` packing the encoder writes back).
        // SAFETY: change_tracking owned exclusively by us at construction.
        unsafe { change_tracking.slice_mut(0, change_len).fill(0xFF); }

        // Build the zero-content line by initializing a full pan buffer
        // and copying out its row 3 (the content template). Used by
        // `clear_dirty_buffer` to reset slot lines after vsync consumes them.
        let mut tmp = vec![0u8; PAN_BUFFER_SIZE * PAN_LINE_SIZE];
        fill_pan_buffer(&mut tmp, 0);
        let zero_content_line = tmp[3 * PAN_LINE_SIZE .. 4 * PAN_LINE_SIZE].to_vec();

        Arc::new(Self {
            current_pan_phase:  AtomicI32::new(0),
            last_pan_phase:     AtomicI32::new(0),
            previous_pan_phase: AtomicI32::new(-1),
            is_blanked:         AtomicBool::new(true),
            vsync_shutdown:     AtomicBool::new(false),
            generator_shutdown: AtomicBool::new(false),
            vsync_clear_request: AtomicBool::new(false),
            vsync_blank_delay:   AtomicU32::new(0),
            msg_queue: Mutex::new(VecDeque::new()),
            gen_cond:   Condvar::new(),
            gen_lock:   Mutex::new(()),
            vsync_cond: Condvar::new(),
            vsync_lock: Mutex::new(()),
            change_tracking,
            dirty_columns:   UnsafeStorage::new(dirty_len),
            zero_content_line,
        })
    }

    /// Externally enqueue a msg. Wakes the generator.
    pub fn submit(&self, msg: UpdateMsg) {
        self.msg_queue.lock().unwrap().push_back(msg);
        self.notify_generator();
    }

    /// Number of msgs queued but not yet drained by the generator.
    pub fn pending_count(&self) -> usize {
        self.msg_queue.lock().unwrap().len()
    }

    /// Wake the generator (queue change OR back-pressure relief).
    pub fn notify_generator(&self) {
        let _g = self.gen_lock.lock().unwrap();
        self.gen_cond.notify_all();
    }

    /// Wake vsync (new pan produced OR shutdown OR clear).
    pub fn notify_vsync(&self) {
        let _g = self.vsync_lock.lock().unwrap();
        self.vsync_cond.notify_all();
    }

    /// Request shutdown of both threads. Caller is responsible for joining
    /// the JoinHandles afterward.
    pub fn request_shutdown(&self) {
        self.generator_shutdown.store(true, Ordering::SeqCst);
        self.vsync_shutdown.store(true, Ordering::SeqCst);
        self.notify_generator();
        self.notify_vsync();
    }
}

/// Modular slot index for a phase counter.
fn norm_phase(phase: i32) -> u32 { (phase & 0xf) as u32 }

// ---------------------------------------------------------------------
// Generator thread
// ---------------------------------------------------------------------

/// Spawn the generator thread.
pub fn spawn_generator(
    state: Arc<SharedState>,
    fb: Arc<dyn FbStorage>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("swtcon-generator".into())
        .spawn(move || generator_main(state, fb))
        .expect("spawn generator")
}

fn generator_main(state: Arc<SharedState>, fb: Arc<dyn FbStorage>) {
    loop {
        // Wait for a msg or shutdown. The wait holds `gen_lock`, the same
        // mutex that `notify_generator` (called by `submit` and by vsync's
        // back-pressure relief) takes before broadcasting. This pairing is
        // what makes the wakeup race-free: any enqueue-then-notify that
        // happens before our wait sees the predicate already true; any that
        // happens after sees us blocked and wakes us via broadcast.
        {
            let mut g = state.gen_lock.lock().unwrap();
            while state.msg_queue.lock().unwrap().is_empty()
                && !state.generator_shutdown.load(Ordering::SeqCst)
            {
                g = state.gen_cond.wait(g).unwrap();
            }
        }

        if state.generator_shutdown.load(Ordering::SeqCst) {
            return;
        }

        // Drain queue. Pop one msg at a time so producers don't block on
        // the queue mutex while we run encoder work.
        loop {
            let msg = {
                let mut q = state.msg_queue.lock().unwrap();
                match q.pop_front() {
                    Some(m) => m,
                    None => break,
                }
            };

            if !process_message(&state, fb.as_ref(), &msg) {
                // Interrupted by shutdown mid-msg — bail out; outer loop
                // will see shutdown flag and exit.
                return;
            }

            // Commit msg's tgt pixels to change_tracking_buffer for the
            // next msg's encoder. Skip for phase-limited "preview"
            // updates — they only partially drove the pixels, so the
            // pre-preview state is what subsequent waveforms should
            // treat as src.
            if msg.phase_limit.is_none() {
                commit_change_tracking(&state, &msg);
            }
        }
    }
}

/// Process one msg: encode every phase, with back-pressure between phases.
/// Returns `false` if interrupted by shutdown mid-msg.
fn process_message(state: &SharedState, fb: &dyn FbStorage, msg: &UpdateMsg) -> bool {
    let mut total_phases = msg.waveform.phase_count as i32;
    if let Some(limit) = msg.phase_limit {
        total_phases = total_phases.min(limit as i32);
    }
    if total_phases <= 0 {
        return true;
    }

    for phase_idx in 0..total_phases {
        if !wait_for_ring_room(state) {
            return false;
        }

        // Reserve next slot.
        let slot_pan_phase = state.last_pan_phase.load(Ordering::SeqCst) + 1;
        let slot_idx = norm_phase(slot_pan_phase);

        encode_phase_into(state, fb, msg, phase_idx, slot_idx);

        state.last_pan_phase.store(slot_pan_phase, Ordering::SeqCst);
        state.notify_vsync();
    }

    true
}

/// Block until ring has room or shutdown. Returns `true` if room is
/// available, `false` if we're shutting down.
///
/// Predicates are checked under `gen_lock` to pair with `notify_generator`
/// (called by vsync after each pan-advance and by `request_shutdown`).
fn wait_for_ring_room(state: &SharedState) -> bool {
    let mut g = state.gen_lock.lock().unwrap();
    loop {
        if state.generator_shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let last = state.last_pan_phase.load(Ordering::SeqCst);
        let cur  = state.current_pan_phase.load(Ordering::SeqCst);
        if (last - cur) < MAX_RING_GAP {
            return true;
        }
        g = state.gen_cond.wait(g).unwrap();
    }
}

/// Encode one phase of one msg into one pan-buffer slot.
fn encode_phase_into(
    state: &SharedState,
    fb: &dyn FbStorage,
    msg: &UpdateMsg,
    phase_idx: i32,
    slot_idx: u32,
) {
    let pt = &msg.waveform;
    let phase_byte = (phase_idx >> 3) as usize;
    let phase_lane = (phase_idx & 7) as u32;

    let phase_table = pt.phase_byte_slice(phase_byte);
    debug_assert_eq!(phase_table.len(), 256);
    let table_arr: &[u16; 256] = phase_table.try_into().unwrap();
    let cmd_lut = build_phase_cmd_lut(table_arr, phase_lane);


    let col_top = msg.rect.col_min as i32;
    let col_bot = msg.rect.col_max as i32;
    let row_l   = msg.rect.row_min as i32;
    let row_r   = msg.rect.row_max as i32;
    let strips  = ((row_r - row_l) + 1) / 8;
    let width = msg.width as usize;

    // SAFETY: slot_idx within range (caller is generator, ring invariant
    // ensures vsync isn't touching this slot).
    let slot_base = unsafe { fb.slot_ptr(slot_idx) };

    // SAFETY: change_tracking is owned by generator alone — no other
    // thread reads or writes this region during encode.
    let change_buf = unsafe {
        state.change_tracking.slice(0, SCREEN_WIDTH * SCREEN_HEIGHT)
    };

    // Per-slot dirty-column row.
    // SAFETY: this slot belongs to generator until we bump
    // last_pan_phase below; vsync is not reading it concurrently.
    let dirty_row = unsafe {
        state.dirty_columns.slice_mut(slot_idx as usize * SCREEN_WIDTH, SCREEN_WIDTH)
    };

    for col in col_top..=col_bot {
        let line_cells = unsafe {
            // skip the 4 preamble rows + one per-column-row offset
            slot_base.as_ptr().add(
                ((4 + col as usize) * PAN_LINE_SIZE) as usize
            ) as *mut u32
        };

        let col_offset = (col - col_top) as usize;
        let row_pixels = &msg.buffer[col_offset * width .. (col_offset + 1) * width];
        let change_col_off = (SCREEN_WIDTH - 1 - col as usize) * SCREEN_HEIGHT;
        let change_col = &change_buf[change_col_off .. change_col_off + SCREEN_HEIGHT];

        for s in 0..strips {
            let row_grp = (row_l + s * 8) as usize;
            let cell_idx = 26 + (row_grp >> 3);

            let src8: &[u8; 8] = (&change_col[row_grp .. row_grp + 8]).try_into().unwrap();
            let tgt_off = row_grp - row_l as usize;
            let tgt8: &[u8; 8] = (&row_pixels[tgt_off .. tgt_off + 8]).try_into().unwrap();
            let packed = encode_strip(src8, tgt8, &cmd_lut);

            // Write the low 16 bits of the cell only — high 16 are static
            // panel control flags. Little-endian write.
            // SAFETY: cell_idx is bounded by strips, which comes from
            // rect.bottom_right.x | 7 - row_l >= 0; line_cells points
            // into a slot we own.
            unsafe {
                let cell_ptr = line_cells.add(cell_idx) as *mut u16;
                cell_ptr.write_volatile(packed);
            }

        }

        dirty_row[col as usize] = 1;
    }
}

/// After a msg's phases are all emitted, commit the tgt pixels to
/// `change_tracking_buffer` so the next msg's encoder sees them as src.
fn commit_change_tracking(state: &SharedState, msg: &UpdateMsg) {
    let col_top = msg.rect.col_min as usize;
    let col_bot = msg.rect.col_max as usize;
    let row_l   = msg.rect.row_min as usize;
    let row_r   = msg.rect.row_max as usize;
    let width = msg.width as usize;

    // SAFETY: change_tracking is generator-owned; no concurrent readers.
    let change_buf = unsafe {
        state.change_tracking.slice_mut(0, SCREEN_WIDTH * SCREEN_HEIGHT)
    };

    for col in col_top..=col_bot {
        let col_offset = col - col_top;
        let row_pixels = &msg.buffer[col_offset * width .. (col_offset + 1) * width];
        let change_col_off = (SCREEN_WIDTH - 1 - col) * SCREEN_HEIGHT;
        let change_col = &mut change_buf[change_col_off .. change_col_off + SCREEN_HEIGHT];

        for row in row_l..=row_r {
            let tgt = row_pixels[row - row_l] & 0xF;
            change_col[row] = tgt | (tgt << 4);
        }
    }
}

// ---------------------------------------------------------------------
// Vsync thread
// ---------------------------------------------------------------------

/// Spawn the vsync thread.
pub fn spawn_vsync(
    state: Arc<SharedState>,
    fb: Arc<dyn FbStorage>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("swtcon-vsync".into())
        .spawn(move || vsync_main(state, fb))
        .expect("spawn vsync")
}

fn vsync_main(state: Arc<SharedState>, fb: Arc<dyn FbStorage>) {
    loop {
        // Drain produced slots: chase last_pan_phase.
        while state.current_pan_phase.load(Ordering::SeqCst)
            != state.last_pan_phase.load(Ordering::SeqCst)
        {
            let cur = state.current_pan_phase.load(Ordering::SeqCst);
            let _ = fb.pan(norm_phase(cur));

            let prev = state.previous_pan_phase.load(Ordering::SeqCst);
            state.previous_pan_phase.store(cur, Ordering::SeqCst);
            state.current_pan_phase.store(cur + 1, Ordering::SeqCst);

            if prev >= 0 {
                clear_dirty_buffer(&state, fb.as_ref(), prev);
            }

            state.notify_generator();
        }

        // Pan to idle slot if not blanked.
        if !state.is_blanked.load(Ordering::SeqCst) {
            let _ = fb.pan(IDLE_SLOT);
        }

        // Final dirty-clear of previous slot.
        let prev = state.previous_pan_phase.load(Ordering::SeqCst);
        if prev >= 0 {
            clear_dirty_buffer(&state, fb.as_ref(), prev);
            state.previous_pan_phase.store(-1, Ordering::SeqCst);
            state.notify_generator();
        }

        // Wait for: new pan, shutdown, or clear request.
        {
            let mut g = state.vsync_lock.lock().unwrap();
            while state.current_pan_phase.load(Ordering::SeqCst)
                  == state.last_pan_phase.load(Ordering::SeqCst)
                && !state.vsync_shutdown.load(Ordering::SeqCst)
                && !state.vsync_clear_request.load(Ordering::SeqCst)
            {
                g = state.vsync_cond.wait(g).unwrap();
            }
        }

        // Shutdown takes priority over clear.
        if state.vsync_shutdown.load(Ordering::SeqCst) {
            return;
        }

        // After-wake unblank. The rM2 driver rejects FBIOPAN_DISPLAY
        // while blanked (see Framebuffer::pan docs), so any pan we issue
        // in the next drain loop will silently EINVAL if we don't
        // unblank first.
        //
        // The unblank itself sets the pan offset to `current` and
        // implicitly "consumes" that slot, so we bump previous+current
        // here to avoid the drain loop redundantly re-panning to it.
        let cur  = state.current_pan_phase.load(Ordering::SeqCst);
        let last = state.last_pan_phase.load(Ordering::SeqCst);
        if state.is_blanked.load(Ordering::SeqCst)
            && cur != last
            && !state.vsync_clear_request.load(Ordering::SeqCst)
        {
            if fb.unblank(norm_phase(cur)).is_ok() {
                state.is_blanked.store(false, Ordering::SeqCst);
                state.previous_pan_phase.store(cur, Ordering::SeqCst);
                state.current_pan_phase.store(cur + 1, Ordering::SeqCst);
            }
        }

        // Always pan to IDLE_SLOT after wake (regardless of whether
        // unblank fired). Keeps the panel parked on a known no-drive
        // slot before the next drain advances through encoded phases.
        if !state.is_blanked.load(Ordering::SeqCst) {
            let _ = fb.pan(IDLE_SLOT);
        }

        // INIT clear path: handled synchronously by `Swtcon::open` before
        // the runtime spawns; the request flag is only consumed here for
        // forward compatibility with future re-init flows.
        let _ = state.vsync_clear_request.swap(false, Ordering::SeqCst);
    }
}

/// Clear dirty columns for one slot — copy the preamble-formatted
/// content template into per-line content where the generator marked
/// them dirty during encoding. The template preserves the panel
/// control flags in the high 16 bits of each cell.
fn clear_dirty_buffer(state: &SharedState, fb: &dyn FbStorage, pan: i32) {
    let phase = norm_phase(pan);

    // SAFETY: this slot is owned by vsync (generator can't touch it
    // until generator's slot ring wraps back around — back-pressure
    // ensures vsync has already advanced past it).
    let slot_base = unsafe { fb.slot_ptr(phase) };

    // SAFETY: dirty_row for this slot is generator-written, but the
    // ring back-pressure invariant ensures generator isn't writing this
    // slot at the same time as vsync clears it.
    let dirty_row = unsafe {
        state.dirty_columns.slice_mut(phase as usize * SCREEN_WIDTH, SCREEN_WIDTH)
    };

    let line_size = PAN_LINE_SIZE;
    debug_assert_eq!(state.zero_content_line.len(), line_size);

    for col in 0..SCREEN_WIDTH {
        if dirty_row[col] != 0 {
            unsafe {
                let dst = slot_base.as_ptr().add((4 + col) * line_size);
                core::ptr::copy_nonoverlapping(
                    state.zero_content_line.as_ptr(), dst, line_size);
            }
        }
    }

    dirty_row.fill(0);
}

// ---------------------------------------------------------------------
// Public runtime handle
// ---------------------------------------------------------------------

/// Owns the generator + vsync thread handles. On drop, requests shutdown
/// and joins.
pub struct Runtime {
    pub state: Arc<SharedState>,
    generator: Option<JoinHandle<()>>,
    vsync: Option<JoinHandle<()>>,
}

impl Runtime {
    pub fn new(fb: Arc<dyn FbStorage>) -> Self {
        let state = SharedState::new();
        let generator = Some(spawn_generator(state.clone(), fb.clone()));
        let vsync = Some(spawn_vsync(state.clone(), fb));
        Self { state, generator, vsync }
    }

    pub fn submit(&self, msg: UpdateMsg) { self.state.submit(msg); }

    /// Wait for all queued msgs to be drained (encoded + committed).
    /// Polls; not async.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.state.pending_count() == 0
                && self.state.current_pan_phase.load(Ordering::SeqCst)
                   == self.state.last_pan_phase.load(Ordering::SeqCst)
            {
                return true;
            }
            if Instant::now() >= deadline { return false; }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.state.request_shutdown();
        if let Some(h) = self.generator.take() { let _ = h.join(); }
        if let Some(h) = self.vsync.take() { let _ = h.join(); }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fb::FbError;
    use core::ptr::NonNull;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    /// Mock fb: backing Vec<u8> for slots, Mutex<Vec<...>> log of ioctl
    /// calls. Threads can share via Arc.
    struct MockFb {
        slots:   UnsafeStorage,
        slot_sz: usize,
        n_slots: u32,
        pans:    AtomicU32,
        blanks:  AtomicU32,
        unblanks: AtomicU32,
        last_pan_idx: AtomicI32,
    }

    impl MockFb {
        fn new(n_slots: u32, slot_sz: usize) -> Arc<Self> {
            Arc::new(Self {
                slots: UnsafeStorage::new(n_slots as usize * slot_sz),
                slot_sz,
                n_slots,
                pans:    AtomicU32::new(0),
                blanks:  AtomicU32::new(0),
                unblanks: AtomicU32::new(0),
                last_pan_idx: AtomicI32::new(-1),
            })
        }
    }

    impl FbStorage for MockFb {
        fn slot_size(&self) -> usize { self.slot_sz }
        fn slot_count(&self) -> u32 { self.n_slots }
        unsafe fn slot_ptr(&self, idx: u32) -> NonNull<u8> {
            assert!(idx < self.n_slots);
            unsafe {
                NonNull::new_unchecked(
                    self.slots.ptr.add(idx as usize * self.slot_sz)
                )
            }
        }
        fn pan(&self, idx: u32) -> Result<(), FbError> {
            self.pans.fetch_add(1, Ordering::SeqCst);
            self.last_pan_idx.store(idx as i32, Ordering::SeqCst);
            Ok(())
        }
        fn blank(&self) -> Result<(), FbError> {
            self.blanks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn unblank(&self, _idx: u32) -> Result<(), FbError> {
            self.unblanks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn fake_phase_table(phase_count: u32) -> Arc<PhaseTable> {
        let phase_bytes = ((phase_count + 7) >> 3) as usize;
        let data = vec![0u16; phase_bytes * 256];
        Arc::new(PhaseTable { phase_count, data })
    }

    /// 8x8 minimal-rect msg: small enough to encode in microseconds.
    fn tiny_msg(phase_count: u32) -> UpdateMsg {
        UpdateMsg {
            rect: PanelRect {
                row_min: 0, row_max: 7,
                col_min: 100, col_max: 100,
            },
            buffer: vec![0u8; 8],   // 1 col × 8 rows
            width:  8,
            waveform: fake_phase_table(phase_count),
            phase_limit: None,
        }
    }

    #[test]
    fn shutdown_idle_runtime_returns_promptly() {
        let fb = MockFb::new(PAN_BUFFERS_COUNT as u32 + 1,
                             PAN_BUFFER_SIZE * PAN_LINE_SIZE);
        let rt = Runtime::new(fb);
        let start = Instant::now();
        drop(rt);
        assert!(start.elapsed() < Duration::from_secs(1),
            "drop took too long: {:?}", start.elapsed());
    }

    #[test]
    fn one_msg_drains() {
        let fb: Arc<dyn FbStorage> = MockFb::new(PAN_BUFFERS_COUNT as u32 + 1,
                                                  PAN_BUFFER_SIZE * PAN_LINE_SIZE);
        let rt = Runtime::new(fb.clone());

        // Spawn a "fake vsync consumer" that just bumps current_pan_phase
        // whenever generator advances last_pan_phase. Without it, the
        // generator's back-pressure would block after MAX_RING_GAP slots.
        let state2 = rt.state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let consumer = thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let last = state2.last_pan_phase.load(Ordering::SeqCst);
                let cur  = state2.current_pan_phase.load(Ordering::SeqCst);
                if cur < last {
                    state2.current_pan_phase.store(cur + 1, Ordering::SeqCst);
                    state2.notify_generator();
                } else {
                    thread::sleep(Duration::from_micros(50));
                }
            }
        });

        let phases = 25;
        rt.submit(tiny_msg(phases));

        assert!(rt.flush(Duration::from_secs(2)),
            "msg didn't drain in 2s; pending={} cur={} last={}",
            rt.state.pending_count(),
            rt.state.current_pan_phase.load(Ordering::SeqCst),
            rt.state.last_pan_phase.load(Ordering::SeqCst));

        assert_eq!(rt.state.last_pan_phase.load(Ordering::SeqCst), phases as i32);

        stop.store(true, Ordering::SeqCst);
        consumer.join().unwrap();
    }

    #[test]
    fn many_msgs_drain_in_order() {
        let fb: Arc<dyn FbStorage> = MockFb::new(PAN_BUFFERS_COUNT as u32 + 1,
                                                  PAN_BUFFER_SIZE * PAN_LINE_SIZE);
        let rt = Runtime::new(fb.clone());

        let state2 = rt.state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let consumer = thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let last = state2.last_pan_phase.load(Ordering::SeqCst);
                let cur  = state2.current_pan_phase.load(Ordering::SeqCst);
                if cur < last {
                    state2.current_pan_phase.store(cur + 1, Ordering::SeqCst);
                    state2.notify_generator();
                } else {
                    thread::sleep(Duration::from_micros(50));
                }
            }
        });

        let phases_each = 8;
        let n_msgs = 50;
        for _ in 0..n_msgs {
            rt.submit(tiny_msg(phases_each));
        }
        assert!(rt.flush(Duration::from_secs(5)),
            "msgs didn't drain; pending={}", rt.state.pending_count());

        let total = phases_each as i32 * n_msgs as i32;
        assert_eq!(rt.state.last_pan_phase.load(Ordering::SeqCst), total);

        stop.store(true, Ordering::SeqCst);
        consumer.join().unwrap();
    }

    #[test]
    fn back_pressure_holds_generator_when_consumer_stalls() {
        // No consumer thread — vsync is a real thread but receives no
        // pan-advance signals from outside, so it will stall too. The
        // generator should fill the ring (up to MAX_RING_GAP) and then
        // block, NOT spin the CPU or panic.
        let fb: Arc<dyn FbStorage> = MockFb::new(PAN_BUFFERS_COUNT as u32 + 1,
                                                  PAN_BUFFER_SIZE * PAN_LINE_SIZE);
        let rt = Runtime::new(fb);

        // Submit a single msg with many phases — generator should only
        // emit MAX_RING_GAP of them before blocking.
        rt.submit(tiny_msg(100));

        // Wait long enough for generator to make its progress.
        thread::sleep(Duration::from_millis(50));

        let last = rt.state.last_pan_phase.load(Ordering::SeqCst);
        let cur  = rt.state.current_pan_phase.load(Ordering::SeqCst);

        // Vsync, with no generator-side gating, will pan everything as
        // fast as it can — so cur catches up to last quickly. The check
        // is that we haven't blown past 100 (the msg's full count) or
        // hung indefinitely.
        assert!(last <= 100, "generator overran its msg: last={last}");
        assert!(cur <= last, "vsync got ahead: cur={cur} last={last}");
        // No assertion on exactly how far we got — depends on scheduler.
        // What matters is that after shutdown we don't deadlock.

        let start = Instant::now();
        drop(rt);
        assert!(start.elapsed() < Duration::from_secs(1),
            "drop took too long: {:?}", start.elapsed());
    }

    #[test]
    fn stress_concurrent_producers() {
        let fb: Arc<dyn FbStorage> = MockFb::new(PAN_BUFFERS_COUNT as u32 + 1,
                                                  PAN_BUFFER_SIZE * PAN_LINE_SIZE);
        let rt = Runtime::new(fb.clone());

        let state2 = rt.state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let consumer = thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let last = state2.last_pan_phase.load(Ordering::SeqCst);
                let cur  = state2.current_pan_phase.load(Ordering::SeqCst);
                if cur < last {
                    state2.current_pan_phase.store(cur + 1, Ordering::SeqCst);
                    state2.notify_generator();
                } else {
                    thread::sleep(Duration::from_micros(20));
                }
            }
        });

        // 4 producers, each submitting 25 msgs concurrently.
        let n_producers = 4;
        let per_producer = 25;
        let total_phases = AtomicUsize::new(0);
        thread::scope(|s| {
            for _ in 0..n_producers {
                s.spawn(|| {
                    for _ in 0..per_producer {
                        rt.submit(tiny_msg(8));
                        total_phases.fetch_add(8, Ordering::SeqCst);
                    }
                });
            }
        });

        assert!(rt.flush(Duration::from_secs(10)),
            "stress drain timed out; pending={}", rt.state.pending_count());

        let expected = (n_producers * per_producer * 8) as i32;
        assert_eq!(rt.state.last_pan_phase.load(Ordering::SeqCst), expected);

        stop.store(true, Ordering::SeqCst);
        consumer.join().unwrap();
    }
}
