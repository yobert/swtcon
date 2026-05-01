//! End-to-end on-device smoke for the public Swtcon API.
//!
//! Test sequence:
//!   1. 8 alternating B/W bands via Du. Validates that DU still works.
//!   2. 16-step gradient via Gc16. Main grayscale validation.
//!   3. Inverted gradient via Gc16. Exercises every (src, tgt) pair
//!      because src has the full 16-gray spread from round 2.
//!   4. 7-px-cell black/mid-gray checkerboard via Gc16Fast.
//!   5. Partial-rect: clear panel to white, draw a single 400x400 black
//!      square in the middle, update only that rect via Gc16. The
//!      surrounding panel area must NOT change. Validates the
//!      coord-transform math for non-full rects.
//!
//! Cross-compile + deploy:
//!     cargo build --release --target armv7-unknown-linux-gnueabihf --example swtcon_smoke
//!     scp target/.../release/examples/swtcon_smoke root@remarkable:/home/root/
//!
//! Run on device (with the stock UI stopped so /dev/fb0 is free):
//!     ssh root@remarkable bash <<'REMOTE'
//!     trap 'systemctl reset-failed xochitl 2>/dev/null; systemctl start xochitl' EXIT
//!     systemctl stop xochitl && /home/root/swtcon_smoke
//!     REMOTE
//!
//! Usage: swtcon_smoke [round...]   — default runs all five rounds.
//!        swtcon_smoke 2            — runs only round 2.

use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use swtcon::constants::{SCREEN_HEIGHT, SCREEN_WIDTH};
use swtcon::{Mode, Rect, Swtcon};

const FB_PATH: &str = "/dev/fb0";
const WBF_PATH: &str = "/var/lib/uboot/320_R327_AFEC21_ED103TC2M1_VB3300-KCD_TC.wbf";

/// xochitl's image format is 16-bit per pixel; the encoder reads
/// `(low_byte >> 1) & 0xf` as the 4-bit gray nibble. So we encode gray
/// G into the low byte as `G << 1`. Bits 0 and 5..7 are unused.
fn pixel_for_gray(g: u8) -> u16 {
    ((g & 0xf) as u16) << 1
}

fn fill_gray(image: &mut [u16], x0: usize, y0: usize, x1: usize, y1: usize, gray: u8) {
    let px = pixel_for_gray(gray);
    for y in y0..y1 {
        for x in x0..x1 {
            image[y * SCREEN_WIDTH + x] = px;
        }
    }
}

fn draw_bw_bands(image: &mut [u16], band_count: usize) {
    let band_h = SCREEN_HEIGHT / band_count;
    for b in 0..band_count {
        let y0 = b * band_h;
        let y1 = if b == band_count - 1 { SCREEN_HEIGHT } else { (b + 1) * band_h };
        let g = if b & 1 == 0 { 15 } else { 0 };
        fill_gray(image, 0, y0, SCREEN_WIDTH, y1, g);
    }
}

fn draw_gradient(image: &mut [u16], inverted: bool) {
    let bands = 16;
    let band_h = SCREEN_HEIGHT / bands;
    for b in 0..bands {
        let y0 = b * band_h;
        let y1 = if b == bands - 1 { SCREEN_HEIGHT } else { (b + 1) * band_h };
        let g = if inverted { b as u8 } else { (bands - 1 - b) as u8 };
        fill_gray(image, 0, y0, SCREEN_WIDTH, y1, g);
    }
}

fn draw_checkerboard(image: &mut [u16], cell_px: usize, gray_a: u8, gray_b: u8) {
    let pa = pixel_for_gray(gray_a);
    let pb = pixel_for_gray(gray_b);
    for y in 0..SCREEN_HEIGHT {
        for x in 0..SCREEN_WIDTH {
            let dark = ((x / cell_px) ^ (y / cell_px)) & 1 != 0;
            image[y * SCREEN_WIDTH + x] = if dark { pa } else { pb };
        }
    }
}

fn run_round(swtcon: &Swtcon, label: &str, image: &[u16], mode: Mode, sleep_secs: u64) {
    println!("[smoke] {label}: update mode={mode:?} full screen");
    swtcon.update(image, Rect::full(), mode).expect("update");
    println!("[smoke]   sleeping {sleep_secs}s — LOOK AT PANEL");
    sleep(Duration::from_secs(sleep_secs));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse round selectors.
    let mut wanted = [false; 6];
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        wanted[1..].fill(true);
    } else {
        for a in &args {
            let n: usize = a.parse().unwrap_or(0);
            if (1..=5).contains(&n) { wanted[n] = true; }
            else { eprintln!("[smoke] ignoring bad round arg: {a}"); }
        }
    }

    println!("[smoke] Swtcon::open({FB_PATH}, {WBF_PATH})");
    let swtcon = Swtcon::open(&PathBuf::from(FB_PATH), &PathBuf::from(WBF_PATH))?;
    println!("[smoke] open OK (init clear ran)");

    let mut image = vec![pixel_for_gray(15); SCREEN_WIDTH * SCREEN_HEIGHT]; // start white

    if wanted[1] {
        draw_bw_bands(&mut image, 8);
        run_round(&swtcon, "1/5 8 B/W bands Du", &image, Mode::Du, 7);
        let _ = swtcon.flush(Duration::from_secs(10));
    }
    if wanted[2] {
        draw_gradient(&mut image, false);
        run_round(&swtcon, "2/5 gradient Gc16 — expect 16 gray bands",
                  &image, Mode::Gc16, 8);
        let _ = swtcon.flush(Duration::from_secs(10));
    }
    if wanted[3] {
        draw_gradient(&mut image, true);
        run_round(&swtcon, "3/5 inverted gradient Gc16 — full (src,tgt) coverage",
                  &image, Mode::Gc16, 8);
        let _ = swtcon.flush(Duration::from_secs(10));
    }
    if wanted[4] {
        draw_checkerboard(&mut image, 7, 0, 8);
        run_round(&swtcon, "4/5 7-px checker Gc16Fast", &image, Mode::Gc16Fast, 7);
        let _ = swtcon.flush(Duration::from_secs(10));
    }
    if wanted[5] {
        // Clear panel back to white via a full Gc16 update so we can see
        // a partial-rect change against a clean background.
        for px in image.iter_mut() { *px = pixel_for_gray(15); }
        run_round(&swtcon, "5/5a clear to white Gc16", &image, Mode::Gc16, 6);
        let _ = swtcon.flush(Duration::from_secs(10));

        // Now draw a 400x400 black square at user (502..902, 736..1136)
        // (centered) and update only that rect. Surrounding panel must
        // not change.
        let x1 = (SCREEN_WIDTH  / 2 - 200) as u16;
        let x2 = (SCREEN_WIDTH  / 2 + 199) as u16;
        let y1 = (SCREEN_HEIGHT / 2 - 200) as u16;
        let y2 = (SCREEN_HEIGHT / 2 + 199) as u16;
        for y in y1 as usize ..= y2 as usize {
            for x in x1 as usize ..= x2 as usize {
                image[y * SCREEN_WIDTH + x] = pixel_for_gray(0);
            }
        }
        println!("[smoke] 5/5b partial-rect ({x1},{y1})..({x2},{y2}) Gc16 — only that area should change");
        swtcon.update(&image, Rect { x1, y1, x2, y2 }, Mode::Gc16).expect("partial update");
        println!("[smoke]   sleeping 8s — LOOK AT PANEL");
        std::thread::sleep(Duration::from_secs(8));
        let _ = swtcon.flush(Duration::from_secs(10));
    }

    println!("[smoke] dropping Swtcon (joins threads + munmaps fb)");
    drop(swtcon);
    println!("[smoke] OK");
    Ok(())
}
