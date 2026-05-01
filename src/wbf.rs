//! WBF (waveform binary file) parser. Pure: takes a byte slice, exposes the
//! header fields and a table decoder. No IO, no allocation beyond the decoded
//! table itself.
//!
//! The WBF format is a compact RLE-ish encoding used by E-Ink panel firmware
//! to ship per-(mode, temperature) waveform tables. Each table decodes to a
//! sequence of "elements" of fixed size (256 or 1024 bytes, set in the
//! header), where each element holds the per-(src,tgt) panel-drive commands
//! for one phase of a waveform mode.
//!
//! Header layout (offsets used here):
//! - `0x04..0x08` (u32 LE): file size — must equal the slice length.
//! - `0x0e..0x10` (u16 LE): `fpl_lot` — the panel signature used to match a
//!   `.wbf` file to a specific panel (decoded from the EPD barcode).
//! - `0x20..0x23` (u24 LE): mode-table offset.
//! - `0x24` (u8): bit 2..3 set = 1024-byte elements; otherwise 256-byte.
//! - `0x25` (u8): max valid mode index.
//! - `0x26` (u8): max valid temp index (so there are temp_count + 1 ranges).
//! - `0x28` (u8): END marker byte value.
//! - `0x29` (u8): row-toggle byte value.
//! - `0x30..` : temperature boundary table (one byte per range start).

use core::fmt;

#[cfg(feature = "std")]
extern crate alloc;
#[cfg(feature = "std")]
use alloc::vec::Vec;

const HDR_SIZE_OFF:        usize = 0x04;
const HDR_FPL_LOT_OFF:     usize = 0x0e;
const HDR_MODE_OFF_OFF:    usize = 0x20;
const HDR_FLAGS_OFF:       usize = 0x24;
const HDR_MODE_COUNT_OFF:  usize = 0x25;
const HDR_TEMP_COUNT_OFF:  usize = 0x26;
const HDR_END_BYTE_OFF:    usize = 0x28;
const HDR_TOGGLE_BYTE_OFF: usize = 0x29;
const HDR_TEMP_TABLE_OFF:  usize = 0x30;
const HDR_MIN_SIZE:        usize = 0x31;

/// Errors returned by the WBF parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WbfError {
    /// Slice is shorter than the minimum header size.
    TooSmall,
    /// File-size field at offset 4 doesn't match the slice length.
    SizeMismatch { header: u32, actual: usize },
    /// `mode` exceeded the file's mode count (header[0x25]).
    ModeOutOfRange { mode: u8, max: u8 },
    /// `temp` exceeded the file's temp count (header[0x26]).
    TempOutOfRange { temp: u8, max: u8 },
    /// Table indirection pointer landed past EOF.
    OffsetOutOfBounds { offset: usize, size: usize },
}

impl fmt::Display for WbfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WbfError::TooSmall => write!(f, "wbf: file smaller than header"),
            WbfError::SizeMismatch { header, actual } =>
                write!(f, "wbf: size header {header} != actual {actual}"),
            WbfError::ModeOutOfRange { mode, max } =>
                write!(f, "wbf: mode {mode} > max {max}"),
            WbfError::TempOutOfRange { temp, max } =>
                write!(f, "wbf: temp {temp} > max {max}"),
            WbfError::OffsetOutOfBounds { offset, size } =>
                write!(f, "wbf: offset 0x{offset:x} >= size 0x{size:x}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for WbfError {}

/// A parsed WBF file. Owns the raw byte slice via borrow; cheap to construct.
#[derive(Debug, Clone, Copy)]
pub struct Wbf<'a> {
    raw: &'a [u8],
}

impl<'a> Wbf<'a> {
    /// Validate the file-size header and return a parsed view.
    pub fn parse(raw: &'a [u8]) -> Result<Self, WbfError> {
        if raw.len() < HDR_MIN_SIZE {
            return Err(WbfError::TooSmall);
        }
        let header_size = u32::from_le_bytes(
            raw[HDR_SIZE_OFF..HDR_SIZE_OFF + 4].try_into().unwrap()
        );
        if header_size as usize != raw.len() {
            return Err(WbfError::SizeMismatch {
                header: header_size,
                actual: raw.len(),
            });
        }
        Ok(Self { raw })
    }

    /// Panel signature — used to match a `.wbf` to a specific panel via the
    /// EPD barcode-derived `fpl_lot`.
    pub fn fpl_lot(&self) -> u16 {
        u16::from_le_bytes(
            self.raw[HDR_FPL_LOT_OFF..HDR_FPL_LOT_OFF + 2].try_into().unwrap()
        )
    }

    /// Max valid mode index. Total mode count is `mode_count() + 1`.
    pub fn mode_count(&self) -> u8 { self.raw[HDR_MODE_COUNT_OFF] }

    /// Max valid temp index. Total temp-range count is `temp_count() + 1`.
    pub fn temp_count(&self) -> u8 { self.raw[HDR_TEMP_COUNT_OFF] }

    /// Element size in bytes — either 256 or 1024 depending on header flags.
    pub fn element_size(&self) -> usize {
        if (self.raw[HDR_FLAGS_OFF] & 0xc) == 4 { 0x400 } else { 0x100 }
    }

    /// Inclusive temperature boundary for range `i`. The last range extends
    /// to 100°C by convention. `i` must be in `0..=temp_count() + 1`.
    pub fn temp_boundary(&self, i: u8) -> u8 {
        if i > self.temp_count() {
            100
        } else {
            self.raw[HDR_TEMP_TABLE_OFF + i as usize]
        }
    }

    /// END marker byte value (varies per file).
    pub fn end_byte(&self) -> u8 { self.raw[HDR_END_BYTE_OFF] }

    /// Row-toggle byte value (varies per file).
    pub fn toggle_byte(&self) -> u8 { self.raw[HDR_TOGGLE_BYTE_OFF] }

    /// Resolve the (mode, temp) → table offset using the two-level
    /// indirection in the file header.
    fn table_offset(&self, mode: u8, temp: u8) -> Result<usize, WbfError> {
        if mode > self.mode_count() {
            return Err(WbfError::ModeOutOfRange { mode, max: self.mode_count() });
        }
        if temp > self.temp_count() {
            return Err(WbfError::TempOutOfRange { temp, max: self.temp_count() });
        }

        // mode-table offset is a 24-bit LE value stored at HDR_MODE_OFF_OFF.
        let mode_off = (self.raw[HDR_MODE_OFF_OFF] as u32)
            | ((self.raw[HDR_MODE_OFF_OFF + 1] as u32) << 8)
            | ((self.raw[HDR_MODE_OFF_OFF + 2] as u32) << 16);

        let mode_entry_off = mode_off as usize + (mode as usize) * 4;
        let temp_table_off = u32::from_le_bytes(
            self.raw[mode_entry_off..mode_entry_off + 4].try_into()
                .map_err(|_| WbfError::OffsetOutOfBounds {
                    offset: mode_entry_off, size: self.raw.len(),
                })?
        ) & 0x00ff_ffff;

        let temp_entry_off = temp_table_off as usize + (temp as usize) * 4;
        let table_off = u32::from_le_bytes(
            self.raw[temp_entry_off..temp_entry_off + 4].try_into()
                .map_err(|_| WbfError::OffsetOutOfBounds {
                    offset: temp_entry_off, size: self.raw.len(),
                })?
        ) & 0x00ff_ffff;

        if (table_off as usize) >= self.raw.len() {
            return Err(WbfError::OffsetOutOfBounds {
                offset: table_off as usize,
                size: self.raw.len(),
            });
        }
        Ok(table_off as usize)
    }

    /// Decode the (mode, temp) table.
    ///
    /// The encoded form mixes "literal" bytes (each holding 4 packed 2-bit
    /// commands) with two control bytes — END (terminates the table) and
    /// TOGGLE (flips between row/run-length modes). When in run-length mode,
    /// each literal byte is followed by a run-length byte.
    ///
    /// Output is one decoded byte per phase per (src,tgt) pixel pair, in
    /// `[mode, temp]` row-major order — i.e., 4 bytes (lanes) per literal
    /// byte, repeated `length` times in run mode.
    #[cfg(feature = "std")]
    pub fn decode_table(&self, mode: u8, temp: u8) -> Result<Vec<u8>, WbfError> {
        let start = self.table_offset(mode, temp)?;
        let table = &self.raw[start..];
        let end_byte = self.end_byte();
        let toggle_byte = self.toggle_byte();

        let mut out: Vec<u8> = Vec::new();
        let mut idx: usize = 0;
        let mut not_at_row_end = true;

        if table[idx] == end_byte {
            return Ok(out);
        }

        loop {
            let elem = table[idx];
            if elem == end_byte {
                break;
            }
            if elem == toggle_byte {
                not_at_row_end = !not_at_row_end;
                idx += 1;
                continue;
            }

            let length = if not_at_row_end {
                // Run-length mode: next byte is run length minus 1.
                let len = table[idx + 1] as usize + 1;
                idx += 2;
                len
            } else {
                idx += 1;
                1
            };

            for _ in 0..length {
                // Each literal byte unpacks into 4 bytes, one per packed 2-bit
                // command. Order: bits[0..1], bits[2..3], bits[4..5], bits[6..7].
                out.push(elem & 0x3);
                out.push((elem >> 2) & 0x3);
                out.push((elem >> 4) & 0x3);
                out.push((elem >> 6) & 0x3);
            }
        }

        Ok(out)
    }
}
