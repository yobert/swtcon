//! Per-strip pixel encoder for SWTCON. Pure functions — no globals.
//!
//! A *strip* is one 8-pixel column-strip within a panel column: 8 consecutive
//! 4-bit pixels packed into the low 16 bits of one pan-buffer cell, with the
//! per-lane mapping
//!
//! ```text
//! lane K -> cell bits[(7-K)*2 .. (7-K)*2 + 1]
//! ```
//!
//! (panel reads bits[14..15] as the FIRST panel_row of the strip.)
//!
//! Each pixel is a 2-bit panel-drive command looked up from a 256-entry
//! `(src, tgt) -> cmd` table built once per phase by [`build_phase_cmd_lut`].

/// Build the 256-byte command lookup table for one phase.
///
/// `phase_table` is `waveform_shorts[(phase_idx >> 3) * 256 ..]`; `phase_lane`
/// is `phase_idx & 7`. Hoisting this out of the per-pixel hot loop saves a
/// multiply, a short load, and a variable shift per pixel.
#[inline]
pub fn build_phase_cmd_lut(phase_table: &[u16; 256], phase_lane: u32) -> [u8; 256] {
    let shift = phase_lane * 2;
    let mut lut = [0u8; 256];
    for i in 0..256 {
        lut[i] = ((phase_table[i] >> shift) & 0x3) as u8;
    }
    lut
}

/// Scalar encoder. Always available — used as the fallback on non-NEON
/// targets and as the oracle for the NEON unit tests.
#[inline]
pub fn encode_strip_scalar(src8: &[u8; 8], tgt8: &[u8; 8], cmd_lut: &[u8; 256]) -> u16 {
    let mut packed: u16 = 0;
    for lane in 0..8 {
        let s = src8[lane] & 0xF;
        let t = tgt8[lane] & 0xF;
        let cmd = cmd_lut[((s << 4) | t) as usize];
        packed |= (cmd as u16) << ((7 - lane) * 2);
    }
    packed
}

/// NEON encoder. Vectorizes the load + mask + combine + pack steps; the 8
/// table lookups remain scalar because ARMv7 NEON has no scatter/gather and
/// the 256-byte LUT doesn't fit `vtbl4`'s 32-byte limit.
///
/// The pack uses a per-lane variable shift (`vshlq_u16`) followed by a
/// 3-step horizontal OR-reduction across 8 lanes — replaces 16 scalar
/// shift-and-OR ops with 4 NEON ops.
#[cfg(all(target_arch = "arm", target_feature = "neon"))]
#[inline]
pub fn encode_strip_neon(src8: &[u8; 8], tgt8: &[u8; 8], cmd_lut: &[u8; 256]) -> u16 {
    use core::arch::arm::*;

    // SAFETY: NEON intrinsics on armv7 require the `neon` target feature,
    // which the cfg gate above enforces. All loads/stores are to fixed-size
    // arrays so bounds are statically known.
    unsafe {
        let mask = vdup_n_u8(0x0F);
        let src_v = vand_u8(vld1_u8(src8.as_ptr()), mask);
        let tgt_v = vand_u8(vld1_u8(tgt8.as_ptr()), mask);
        // idx = (src << 4) | tgt — produces 8 lookup indices in [0, 255].
        let idx_v = vorr_u8(vshl_n_u8(src_v, 4), tgt_v);

        let mut indices = [0u8; 8];
        vst1_u8(indices.as_mut_ptr(), idx_v);

        // Scalar gather. 8 indexed byte loads from a 256-byte table that fits
        // comfortably in L1.
        let cmds: [u8; 8] = [
            cmd_lut[indices[0] as usize],
            cmd_lut[indices[1] as usize],
            cmd_lut[indices[2] as usize],
            cmd_lut[indices[3] as usize],
            cmd_lut[indices[4] as usize],
            cmd_lut[indices[5] as usize],
            cmd_lut[indices[6] as usize],
            cmd_lut[indices[7] as usize],
        ];

        // Promote to u16 lanes for the variable shift.
        let cmds_v = vmovl_u8(vld1_u8(cmds.as_ptr()));

        // Per-lane left-shifts: lane K shifts by (7-K)*2 so lane 0 ends up in
        // bits[14..15] and lane 7 ends up in bits[0..1].
        let shifts: [i16; 8] = [14, 12, 10, 8, 6, 4, 2, 0];
        let shifts_v = vld1q_s16(shifts.as_ptr());
        let shifted = vshlq_u16(cmds_v, shifts_v);

        // Horizontal OR-reduction in 3 steps: 8 -> 4 -> 2 -> 1.
        let r4 = vorr_u16(vget_low_u16(shifted), vget_high_u16(shifted));
        let r2 = vorr_u16(r4, vext_u16(r4, r4, 2));
        let r1 = vorr_u16(r2, vext_u16(r2, r2, 1));
        vget_lane_u16(r1, 0)
    }
}

/// Public entry point. Picks NEON on ARM, scalar elsewhere. Bit-exact.
#[inline]
pub fn encode_strip(src8: &[u8; 8], tgt8: &[u8; 8], cmd_lut: &[u8; 256]) -> u16 {
    #[cfg(all(target_arch = "arm", target_feature = "neon"))]
    {
        encode_strip_neon(src8, tgt8, cmd_lut)
    }
    #[cfg(not(all(target_arch = "arm", target_feature = "neon")))]
    {
        encode_strip_scalar(src8, tgt8, cmd_lut)
    }
}
