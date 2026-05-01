//! Times the wall-clock latency of a single GLF and DU refresh on the
//! rM2 panel. Stops xochitl/switcher first; assumes /dev/fb0 is free.
//!
//! Run on device:
//!     scp refresh_bench root@remarkable:/home/root/
//!     systemctl stop switcher.service xochitl.service
//!     /home/root/refresh_bench /var/lib/uboot/<panel>.wbf

use std::path::PathBuf;
use std::time::{Duration, Instant};

use swtcon::{Mode, Rect, Swtcon};

const W: usize = 1404;
const H: usize = 1872;

fn run_bench(name: &str, swtcon: &Swtcon, image: &[u16], mode: Mode, rect: Rect) {
    run_bench_limit(name, swtcon, image, mode, rect, None)
}

fn run_bench_limit(
    name: &str,
    swtcon: &Swtcon,
    image: &[u16],
    mode: Mode,
    rect: Rect,
    phase_limit: Option<u32>,
) {
    let n = 5;
    let mut total = Duration::ZERO;
    for _ in 0..n {
        let t0 = Instant::now();
        let _ = swtcon.update_with_limit(image, rect, mode, phase_limit);
        let _ = swtcon.flush(Duration::from_secs(10));
        total += t0.elapsed();
    }
    println!(
        "{name}: {} refreshes in {:?} → avg {:?}",
        n, total, total / n,
    );
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: refresh_bench <wbf>");
    let swtcon = Swtcon::open(std::path::Path::new("/dev/fb0"), &PathBuf::from(&path))
        .expect("open swtcon");

    // Two test images: small black square at center vs whole-screen white.
    let mut white = vec![0xFFFFu16; W * H];
    let mut spotted = white.clone();
    for y in 800..900 {
        for x in 600..700 {
            spotted[y * W + x] = 0x0000;
        }
    }

    // Get the panel into a known white state first.
    let _ = swtcon.update(&white, Rect::full(), Mode::Gc16);
    let _ = swtcon.flush(Duration::from_secs(10));

    let small = Rect { x1: 600, y1: 800, x2: 699, y2: 899 };
    run_bench("GLF small (100x100)", &swtcon, &spotted, Mode::Glf, small);
    let _ = swtcon.update(&white, Rect::full(), Mode::Gc16);
    let _ = swtcon.flush(Duration::from_secs(10));
    run_bench("DU small (100x100)", &swtcon, &spotted, Mode::Du, small);
    let _ = swtcon.update(&white, Rect::full(), Mode::Gc16);
    let _ = swtcon.flush(Duration::from_secs(10));
    run_bench("Gc16Fast small (100x100)", &swtcon, &spotted, Mode::Gc16Fast, small);
    let _ = swtcon.update(&white, Rect::full(), Mode::Gc16);
    let _ = swtcon.flush(Duration::from_secs(10));
    run_bench("Gc16 small (100x100)", &swtcon, &spotted, Mode::Gc16, small);

    // Tiny rect — see if panel scales the time with rect area.
    let tiny = Rect { x1: 600, y1: 800, x2: 619, y2: 819 };
    let _ = swtcon.update(&white, Rect::full(), Mode::Gc16);
    let _ = swtcon.flush(Duration::from_secs(10));
    run_bench("GLF tiny (20x20)", &swtcon, &spotted, Mode::Glf, tiny);

    // Truncated previews — test 1-3 phases of GLF.
    for phases in [1, 2, 3, 5] {
        let _ = swtcon.update(&white, Rect::full(), Mode::Gc16);
        let _ = swtcon.flush(Duration::from_secs(10));
        run_bench_limit(
            &format!("GLF preview ({phases} phases)"),
            &swtcon, &spotted, Mode::Glf, small, Some(phases),
        );
    }
    println!("done");
}
