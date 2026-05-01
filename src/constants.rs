//! Panel + pan-buffer geometry constants. Shared by the encoder, fb mmap,
//! and waveform-pack code.

/// Visible panel width, in panel-rotated coordinates (the "wide" axis).
pub const SCREEN_WIDTH: usize = 1404;

/// Visible panel height, in panel-rotated coordinates (the "tall" axis).
pub const SCREEN_HEIGHT: usize = 1872;

/// Number of pan-buffer slots in the framebuffer ring. The vsync thread
/// rotates through these; the generator thread fills them ahead.
pub const PAN_BUFFERS_COUNT: usize = 0x10; // 16

/// Number of rows in one pan-buffer slot. Includes 4 preamble rows + one
/// per panel column.
pub const PAN_BUFFER_SIZE: usize = 0x580; // 1408

/// Bytes per content row within a pan-buffer slot. 260 cells x 4 bytes/cell
/// (the panel reads each cell as a u32 with control bits in the high half
/// and pixel commands packed into the low half).
pub const PAN_LINE_SIZE: usize = 0x410; // 1040

/// 4 bits per pixel (16 gray levels).
pub const PAN_BITS_PER_PIXEL: usize = 4;
