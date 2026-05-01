//! Waveform-pack assembly.
//!
//! Builds the packed per-(mode, temp, phase) tables that [`crate::strip_encoder`]
//! consumes, from a parsed [`crate::wbf::Wbf`].
//!
//! Each [`PhaseTable`] is laid out as `((phase_count + 7) / 8) * 256` little-
//! endian `u16`s. To look up the 2-bit panel-drive command for one (src, tgt)
//! pixel pair at one phase:
//!
//! ```ignore
//! let phase_byte = phase_idx >> 3;
//! let phase_lane = phase_idx & 7;
//! let combined  = ((src as usize) << 4) | (tgt as usize);
//! let entry     = pt.data[phase_byte * 256 + combined];
//! let cmd       = (entry >> (phase_lane * 2)) & 0x3;
//! ```
//!
//! That's exactly the access pattern in
//! [`crate::strip_encoder::build_phase_cmd_lut`].

use crate::wbf::{Wbf, WbfError};

extern crate alloc;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

/// Initialize-mode pan codes per phase, derived from the first byte of each
/// element of the (mode=0, temp) table. Indexed by phase.
#[derive(Debug, Clone)]
pub struct InitPhases {
    /// Per-phase pan code (low 4 bits drive the panel rotation).
    pub pan_codes: Vec<u8>,
}

/// Packed phase table — exactly the buffer the strip encoder reads from.
#[derive(Debug, Clone)]
pub struct PhaseTable {
    /// Number of phases driven by this (mode, temp). Visible cell-update
    /// time scales with this.
    pub phase_count: u32,
    /// `((phase_count + 7) / 8) * 256` u16s. Cell layout described above.
    pub data: Vec<u16>,
}

impl PhaseTable {
    /// Number of phase-byte rows this table holds (each row covers up to
    /// 8 phases).
    pub fn phase_byte_count(&self) -> usize {
        ((self.phase_count as usize) + 7) >> 3
    }

    /// 256-short slice for one phase-byte. Suitable for passing to
    /// [`crate::strip_encoder::build_phase_cmd_lut`].
    pub fn phase_byte_slice(&self, phase_byte: usize) -> &[u16] {
        let start = phase_byte * 256;
        &self.data[start..start + 256]
    }
}

/// One mode's tables, one PhaseTable per temperature index. Wrapped in
/// `Arc` so update msgs can cheaply share the same table (no copy of the
/// ~4 KB packed data per submission).
#[derive(Debug, Clone)]
pub struct ModeWaveform {
    /// Full-refresh table per tempIdx — drives every (src, tgt) pair.
    pub full: Vec<Arc<PhaseTable>>,
    /// Partial-refresh table per tempIdx — same as `full`, but with the
    /// diagonal entries (`src == tgt`) zeroed so unchanged pixels aren't
    /// driven. Equal to `full` when the source mode doesn't request a
    /// separate partial.
    pub partial: Vec<Arc<PhaseTable>>,
}

/// Internal WBF mode index for a logical waveform tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WbfMode {
    pub mode: u8,
    pub separate_partial: bool,
    pub skip_init: bool,
}

/// Top-level waveform pack. Built from a Wbf via [`WaveformPack::from_wbf`].
#[derive(Debug, Clone)]
pub struct WaveformPack {
    /// Inclusive (low, high) °C bounds per temperature index. The last entry
    /// extends to 100 by convention.
    pub temp_ranges: Vec<(u8, u8)>,
    /// INIT pan codes per tempIdx.
    pub init: Vec<InitPhases>,
    /// Loaded mode tables, in the order `requested_modes` was given to
    /// `from_wbf`.
    pub modes: Vec<ModeWaveform>,
}

impl WaveformPack {
    pub fn from_wbf(wbf: &Wbf<'_>, requested_modes: &[WbfMode])
        -> Result<Self, WbfError>
    {
        let mut temp_ranges = Vec::with_capacity(wbf.temp_count() as usize + 1);
        for i in 0..=wbf.temp_count() {
            temp_ranges.push((wbf.temp_boundary(i), wbf.temp_boundary(i + 1)));
        }

        let init = build_init_table(wbf)?;

        let mut modes = Vec::with_capacity(requested_modes.len());
        for spec in requested_modes {
            modes.push(build_mode_table(wbf, *spec)?);
        }

        Ok(Self { temp_ranges, init, modes })
    }
}

/// Build the per-temperature INIT pan codes. For each temp:
///   1. Decode (mode=0, temp) into a flat byte array.
///   2. Element count = decoded_size / element_size.
///   3. Pull the first byte of each element (= pan code for that phase).
fn build_init_table(wbf: &Wbf<'_>) -> Result<Vec<InitPhases>, WbfError> {
    let elem_sz = wbf.element_size();
    let mut out = Vec::with_capacity(wbf.temp_count() as usize + 1);

    for temp in 0..=wbf.temp_count() {
        let decoded = wbf.decode_table(0, temp)?;
        let count = decoded.len() / elem_sz;
        let mut pan_codes = Vec::with_capacity(count);
        for i in 0..count {
            pan_codes.push(decoded[i * elem_sz]);
        }
        out.push(InitPhases { pan_codes });
    }
    Ok(out)
}

/// Build the per-(mode, temp) packed phase tables. For each temp:
///   1. Decode the (mode, temp) table.
///   2. For each (src, tgt) pair in 16x16:
///        Walk through `phase_count` elements; the byte at the (src, tgt)
///        offset within each element is one 2-bit panel-drive command for
///        that phase. Pack 8 phases into one u16 (LE), with phase % 8 at
///        bits [2*(phase%8) .. 2*(phase%8)+1].
///   3. (Optional) Skip leading "init" phases — phases where the byte is 0.
///      The first non-zero byte starts the recorded sequence.
///   4. (Optional) Build a partial variant that zeros out diagonal entries
///      (src == tgt) so the panel doesn't drive unchanged pixels.
fn build_mode_table(wbf: &Wbf<'_>, spec: WbfMode) -> Result<ModeWaveform, WbfError> {
    let elem_sz = wbf.element_size();
    // The (src, tgt) -> in-element-byte-offset stride differs between the two
    // element-size cases.
    let (src_stride, tgt_stride) = if elem_sz == 0x400 { (2usize, 0x40) }
                                   else                { (1usize, 0x10) };

    let mut full_per_temp = Vec::with_capacity(wbf.temp_count() as usize + 1);
    let mut partial_per_temp = Vec::with_capacity(wbf.temp_count() as usize + 1);

    for temp in 0..=wbf.temp_count() {
        let decoded = wbf.decode_table(spec.mode, temp)?;
        let element_count = (decoded.len() / elem_sz) as u32;
        let phase_bytes = ((element_count + 7) >> 3) as usize;
        let total_shorts = phase_bytes * 256;

        let mut full = vec![0u16; total_shorts];

        // Iterate 16x16 (src, tgt). idx1 = src*16 (already shifted into the
        // high nibble of the combined index); idx2 = tgt.
        for src in 0..16usize {
            for tgt in 0..16usize {
                let combined = (src << 4) | tgt;
                let in_elem_off = src * src_stride + tgt * tgt_stride;

                let mut written: u32 = 0;
                let mut acc: u16 = 0;
                let mut in_init = true;

                for elem_idx in 0..element_count as usize {
                    let byte = decoded[elem_idx * elem_sz + in_elem_off];

                    if spec.skip_init {
                        if (byte & 3) != 0 { in_init = false; }
                        if in_init { continue; }
                    }

                    let lane = written & 7;
                    acc |= ((byte & 3) as u16) << (lane * 2);
                    written += 1;

                    let last_elem = elem_idx == element_count as usize - 1;
                    if (written & 7) == 0 || last_elem {
                        let phase_byte = ((written - 1) >> 3) as usize;
                        full[phase_byte * 256 + combined] = acc;
                        acc = 0;
                    }
                }
            }
        }

        // Partial variant: optionally zero diagonal entries (src == tgt)
        // across all phase-byte rows.
        let partial = if !spec.separate_partial {
            full.clone()
        } else {
            let mut p = full.clone();
            for phase_byte in 0..phase_bytes {
                let row_off = phase_byte * 256;
                for sg in 0..16usize {
                    p[row_off + (sg << 4) + sg] = 0;
                }
            }
            p
        };

        full_per_temp.push(Arc::new(PhaseTable { phase_count: element_count, data: full }));
        partial_per_temp.push(Arc::new(PhaseTable { phase_count: element_count, data: partial }));
    }

    Ok(ModeWaveform { full: full_per_temp, partial: partial_per_temp })
}

/// Convenience: the standard mode list the rM2 stock UI loads. Produces
/// 9 [`ModeWaveform`]s indexed as:
///   0: DU full, 1: GC16 full, 2: FAST full, 3: GLF full, 4: DU4 full,
///   5: DU partial, 6: GC16 partial, 7: DU4 partial, 8: A2
pub fn standard_xochitl_modes() -> [WbfMode; 9] {
    [
        WbfMode { mode: 1, separate_partial: false, skip_init: false }, // DU
        WbfMode { mode: 2, separate_partial: true,  skip_init: false }, // GC16
        WbfMode { mode: 3, separate_partial: true,  skip_init: false }, // FAST
        WbfMode { mode: 6, separate_partial: false, skip_init: false }, // GLF
        WbfMode { mode: 7, separate_partial: false, skip_init: false }, // DU4
        WbfMode { mode: 1, separate_partial: false, skip_init: true  }, // DU
        WbfMode { mode: 2, separate_partial: true,  skip_init: true  }, // GC16
        WbfMode { mode: 7, separate_partial: false, skip_init: true  }, // DU4
        WbfMode { mode: 4, separate_partial: false, skip_init: false }, // A2
    ]
}
