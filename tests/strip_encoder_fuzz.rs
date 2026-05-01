//! Bit-exact validation for the SWTCON strip encoder.
//!
//! - `scalar_matches_spec` (5k trials): the optimized scalar path matches a
//!   from-spec oracle that does the lookup the textbook way (no LUT hoist).
//! - `lane_to_bit_mapping`: documents the panel's bit ordering with a
//!   hand-computed expected output (0x1B1B).
//! - `high_nibbles_masked`: change-tracking buffer stores `v | (v << 4)`;
//!   encoder must mask to low nibble before lookup.
//! - `neon_matches_scalar` (200k trials): NEON output matches scalar
//!   byte-for-byte. Only compiled on ARM with NEON; runnable via:
//!     cargo test --target armv7-unknown-linux-gnueabihf --release
//!   under `qemu-arm-static`.

use swtcon::strip_encoder::{build_phase_cmd_lut, encode_strip_scalar};

/// Tiny LCG so tests have zero deps. Not cryptographically meaningful —
/// just deterministic across runs and architectures.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes constants.
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }
    // Used only by the NEON test; cfg-gating the impl is uglier than the warn.
    #[allow(dead_code)]
    fn next_u8(&mut self) -> u8 {
        self.next_u32() as u8
    }
    fn next_nibble(&mut self) -> u8 {
        (self.next_u32() & 0xF) as u8
    }
    fn next_phase(&mut self) -> u32 {
        self.next_u32() % 32
    }
}

/// 32-phase fake waveform table = 4 phaseBytes * 256 shorts. The shape doesn't
/// have to be physically meaningful — only stable.
fn fake_waveform() -> Vec<u16> {
    let mut rng = Lcg::new(0xC0FFEE);
    (0..(256 * 4)).map(|_| rng.next_u32() as u16).collect()
}

fn phase_table(wf: &[u16], phase_idx: u32) -> &[u16; 256] {
    let off = (phase_idx >> 3) as usize * 256;
    <&[u16; 256]>::try_from(&wf[off..off + 256]).unwrap()
}

/// Spec re-implementation of one strip's encoding — the oracle.
fn encode_strip_spec(
    src8: &[u8; 8],
    tgt8: &[u8; 8],
    phase_table: &[u16; 256],
    phase_lane: u32,
) -> u16 {
    let mut packed: u16 = 0;
    for lane in 0..8 {
        let s = src8[lane] & 0xF;
        let t = tgt8[lane] & 0xF;
        let entry = phase_table[((s << 4) | t) as usize];
        let cmd = ((entry >> (phase_lane * 2)) & 0x3) as u8;
        packed |= (cmd as u16) << ((7 - lane) * 2);
    }
    packed
}

#[test]
fn lane_to_bit_mapping() {
    // Per the encoder convention: panel reads bits[14..15] as the FIRST
    // panel_row of the strip. So lane 0's command must end up in bits[14..15],
    // lane 7's in bits[0..1].
    let mut cmd_lut = [0u8; 256];
    cmd_lut[(0 << 4) | 0] = 0;
    cmd_lut[(0 << 4) | 1] = 1;
    cmd_lut[(0 << 4) | 2] = 2;
    cmd_lut[(0 << 4) | 3] = 3;

    let src = [0u8; 8];
    let tgt = [0, 1, 2, 3, 0, 1, 2, 3];

    // Lane 0 cmd 0 -> bits[14..15] = 00
    // Lane 1 cmd 1 -> bits[12..13] = 01
    // Lane 2 cmd 2 -> bits[10..11] = 10
    // Lane 3 cmd 3 -> bits[ 8.. 9] = 11
    // Lane 4 cmd 0 -> bits[ 6.. 7] = 00
    // Lane 5 cmd 1 -> bits[ 4.. 5] = 01
    // Lane 6 cmd 2 -> bits[ 2.. 3] = 10
    // Lane 7 cmd 3 -> bits[ 0.. 1] = 11
    // = 0b0001_1011_0001_1011 = 0x1B1B
    assert_eq!(encode_strip_scalar(&src, &tgt, &cmd_lut), 0x1B1B);
}

#[test]
fn high_nibbles_masked() {
    // `changeTrackingBuffer` stores `v | (v << 4)`. The encoder must ignore
    // the high nibble for the lookup.
    let wf = fake_waveform();
    let cmd_lut = build_phase_cmd_lut(phase_table(&wf, 5), 5);

    let src_dirty = [0xAB, 0x3C, 0xFF, 0x00, 0x99, 0x4D, 0x71, 0xE2];
    let tgt_dirty = [0xF0, 0x55, 0xCC, 0x12, 0x88, 0xA3, 0x6E, 0x40];
    let mut src_low = [0u8; 8];
    let mut tgt_low = [0u8; 8];
    for i in 0..8 {
        src_low[i] = src_dirty[i] & 0xF;
        tgt_low[i] = tgt_dirty[i] & 0xF;
    }
    assert_eq!(
        encode_strip_scalar(&src_dirty, &tgt_dirty, &cmd_lut),
        encode_strip_scalar(&src_low, &tgt_low, &cmd_lut),
    );
}

#[test]
fn scalar_matches_spec() {
    let wf = fake_waveform();
    let mut rng = Lcg::new(0xBEEF);

    for _ in 0..5000 {
        let mut src = [0u8; 8];
        let mut tgt = [0u8; 8];
        for i in 0..8 {
            src[i] = rng.next_nibble();
            tgt[i] = rng.next_nibble();
        }
        let phase_idx = rng.next_phase();
        let phase_lane = phase_idx & 7;
        let pt = phase_table(&wf, phase_idx);

        let cmd_lut = build_phase_cmd_lut(pt, phase_lane);

        let got = encode_strip_scalar(&src, &tgt, &cmd_lut);
        let want = encode_strip_spec(&src, &tgt, pt, phase_lane);
        assert_eq!(got, want,
            "phase={} src={:?} tgt={:?}", phase_idx, src, tgt);
    }
}

#[cfg(all(target_arch = "arm", target_feature = "neon"))]
#[test]
fn neon_matches_scalar() {
    use swtcon::strip_encoder::encode_strip_neon;

    let wf = fake_waveform();
    let mut rng = Lcg::new(0xC0DE);

    for _ in 0..200_000 {
        let mut src = [0u8; 8];
        let mut tgt = [0u8; 8];
        for i in 0..8 {
            // Fill high nibble with garbage to verify NEON masks it.
            src[i] = rng.next_u8();
            tgt[i] = rng.next_u8();
        }
        let phase_idx = rng.next_phase();
        let phase_lane = phase_idx & 7;
        let cmd_lut = build_phase_cmd_lut(phase_table(&wf, phase_idx), phase_lane);

        let scalar = encode_strip_scalar(&src, &tgt, &cmd_lut);
        let neon = encode_strip_neon(&src, &tgt, &cmd_lut);
        assert_eq!(scalar, neon,
            "phase={} src={:?} tgt={:?}", phase_idx, src, tgt);
    }
}
