//! Tests against the real .wbf pulled from a rM2 OS 3.26.0.68 device. Skips
//! cleanly if the fixture isn't present — see tests/fixtures/README.md for
//! how to populate it.

use std::path::PathBuf;
use swtcon::waveform::{standard_xochitl_modes, WaveformPack};
use swtcon::wbf::Wbf;

fn fixture_bytes() -> Option<Vec<u8>> {
    let mut p: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    p.push("tests/fixtures/320_R327_AFEC21_ED103TC2M1_VB3300-KCD_TC.wbf");
    if !p.exists() {
        eprintln!("SKIP: fixture not present at {}", p.display());
        return None;
    }
    Some(std::fs::read(&p).expect("fixture is present but unreadable"))
}

#[test]
fn header_matches_device_observation() {
    let Some(raw) = fixture_bytes() else { return };
    let wbf = Wbf::parse(&raw).expect("wbf parses");

    // Device printed `Got signature: 327` and matched this file via fpl_lot.
    assert_eq!(wbf.fpl_lot(), 327, "fpl_lot");

    // Device printed 14 temp ranges (indices 0..=13).
    assert_eq!(wbf.temp_count(), 13, "temp_count");

    // Verify the first and last boundaries from the device output:
    //   temp range 0:  0 - 3
    //   temp range 13: 43 - 100
    assert_eq!(wbf.temp_boundary(0), 0);
    assert_eq!(wbf.temp_boundary(1), 3);
    assert_eq!(wbf.temp_boundary(13), 43);
    assert_eq!(wbf.temp_boundary(14), 100);  // synthesized last bound
}

#[test]
fn temp_ranges_match_device_observation() {
    let Some(raw) = fixture_bytes() else { return };
    let wbf = Wbf::parse(&raw).expect("wbf parses");
    let pack = WaveformPack::from_wbf(&wbf, &standard_xochitl_modes())
        .expect("pack builds");

    // Match the device's full temp range table:
    //   0: 0-3, 1: 3-6, 2: 6-9, 3: 9-12, 4: 12-15, 5: 15-18, 6: 18-21,
    //   7: 21-24, 8: 24-27, 9: 27-30, 10: 30-33, 11: 33-38, 12: 38-43, 13: 43-100
    let expected = [
        (0, 3), (3, 6), (6, 9), (9, 12), (12, 15), (15, 18), (18, 21),
        (21, 24), (24, 27), (27, 30), (30, 33), (33, 38), (38, 43), (43, 100),
    ];
    assert_eq!(pack.temp_ranges, expected);
}

#[test]
fn init_phase_count_matches_device_observation() {
    let Some(raw) = fixture_bytes() else { return };
    let wbf = Wbf::parse(&raw).expect("wbf parses");
    let pack = WaveformPack::from_wbf(&wbf, &standard_xochitl_modes())
        .expect("pack builds");

    // Device printed `Running init (111 phases)` at temp idx 7 (24°C).
    assert_eq!(pack.init[7].pan_codes.len(), 111,
               "init phase count at tempIdx=7");
}

#[test]
fn fast_and_gc16_phase_counts_at_temp7() {
    let Some(raw) = fixture_bytes() else { return };
    let wbf = Wbf::parse(&raw).expect("wbf parses");
    let pack = WaveformPack::from_wbf(&wbf, &standard_xochitl_modes())
        .expect("pack builds");

    // From swtcon_smoke output at tempIdx=7 (~24°C):
    //   user-FAST  = internal DU      = mode 1 = pack.modes[0] = 25 phases
    //   user-MED   = internal GC16    = mode 2 = pack.modes[1] = 46 phases
    //   user-HQ    = internal FASTwfm = mode 3 = pack.modes[2] = 46 phases
    //
    // (The "FAST internal" tier happens to hit the same 46-phase count as
    // GC16 at this temp; that's a property of this particular WBF, not a
    // general expectation.)
    assert_eq!(pack.modes[0].full[7].phase_count, 25, "DU (mode 1)");
    assert_eq!(pack.modes[1].full[7].phase_count, 46, "GC16 (mode 2)");
    assert_eq!(pack.modes[2].full[7].phase_count, 46, "FAST (mode 3)");
}

#[test]
fn partial_table_zeros_diagonal() {
    let Some(raw) = fixture_bytes() else { return };
    let wbf = Wbf::parse(&raw).expect("wbf parses");
    let pack = WaveformPack::from_wbf(&wbf, &standard_xochitl_modes())
        .expect("pack builds");

    // GC16 (mode index 1 in our pack) requests separate_partial=true. So all
    // diagonal entries (src == tgt) in the partial table must be zero across
    // every phase byte and every temp.
    let gc16 = &pack.modes[1];
    for (temp_idx, pt) in gc16.partial.iter().enumerate() {
        for phase_byte in 0..pt.phase_byte_count() {
            for sg in 0..16usize {
                let combined = (sg << 4) | sg;
                let v = pt.data[phase_byte * 256 + combined];
                assert_eq!(v, 0,
                    "GC16 partial: temp={} phase_byte={} sg={} expected 0 got 0x{:04x}",
                    temp_idx, phase_byte, sg, v);
            }
        }
    }
}

#[test]
fn full_and_partial_agree_off_diagonal() {
    let Some(raw) = fixture_bytes() else { return };
    let wbf = Wbf::parse(&raw).expect("wbf parses");
    let pack = WaveformPack::from_wbf(&wbf, &standard_xochitl_modes())
        .expect("pack builds");

    // Same GC16 mode: all OFF-diagonal entries (src != tgt) must match
    // between full and partial tables (partial is just full with diag zeroed).
    let gc16 = &pack.modes[1];
    for (temp_idx, (full, partial)) in gc16.full.iter().zip(gc16.partial.iter()).enumerate() {
        for phase_byte in 0..full.phase_byte_count() {
            for src in 0..16usize {
                for tgt in 0..16usize {
                    if src == tgt { continue; }
                    let combined = (src << 4) | tgt;
                    let f = full.data[phase_byte * 256 + combined];
                    let p = partial.data[phase_byte * 256 + combined];
                    assert_eq!(f, p,
                        "off-diag mismatch: temp={} phase_byte={} src={} tgt={}",
                        temp_idx, phase_byte, src, tgt);
                }
            }
        }
    }
}
