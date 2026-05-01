//! Dumps the phase-count for every loaded mode at every temperature
//! index. Useful for figuring out which WBF mode is the actual A2
//! (smallest phase count + 1-bit only) on a given panel.
//!
//! Run on device:
//!     scp wbf_phases root@remarkable:/home/root/
//!     ssh root@remarkable /home/root/wbf_phases /var/lib/uboot/<panel>.wbf

use std::path::PathBuf;

use swtcon::waveform::{standard_xochitl_modes, WaveformPack};
use swtcon::wbf::Wbf;

fn main() {
    let path = std::env::args().nth(1).expect("usage: wbf_phases <wbf>");
    let bytes = std::fs::read(PathBuf::from(&path)).expect("read wbf");
    let wbf = Wbf::parse(&bytes).expect("parse wbf");
    let modes = standard_xochitl_modes();
    let pack = WaveformPack::from_wbf(&wbf, &modes).expect("build pack");

    println!("temp_count={} (ranges: {:?})", pack.temp_ranges.len(), pack.temp_ranges);
    println!();
    println!(" idx  WBF.mode sep_partial skip_init  phases-per-temp");
    for (i, mw) in pack.modes.iter().enumerate() {
        let m = &modes[i];
        let phases: Vec<u32> = mw.full.iter().map(|t| t.phase_count).collect();
        println!("  {i:2}    {:>4}      {:>5}      {:>5}     {:?}",
            m.mode, m.separate_partial, m.skip_init, phases);
    }
}
