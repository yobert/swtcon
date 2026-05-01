//! Minimal on-device smoke for the framebuffer wrapper.
//!
//! Doesn't drive any waveforms — just exercises the open/mmap/pan ioctls
//! and verifies the slots are filled with the zero pan-buffer pattern.
//!
//! Cross-compile + deploy:
//!     cargo build --release --target armv7-unknown-linux-gnueabihf --example fb_smoke
//!     scp target/armv7-unknown-linux-gnueabihf/release/examples/fb_smoke \
//!         root@remarkable:/home/root/
//!
//! Run on device (with the stock UI stopped so /dev/fb0 is free):
//!     ssh root@remarkable bash <<'REMOTE'
//!     trap 'systemctl reset-failed xochitl 2>/dev/null; systemctl start xochitl' EXIT
//!     systemctl stop xochitl && /home/root/fb_smoke
//!     REMOTE

use std::path::PathBuf;
use swtcon::constants::{PAN_BUFFER_SIZE, PAN_BUFFERS_COUNT, PAN_LINE_SIZE};
use swtcon::fb::{FbStorage, Framebuffer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/fb0".to_string())
        .into();

    println!("[smoke] open {} with {} pan buffers", path.display(), PAN_BUFFERS_COUNT);
    let fb = Framebuffer::open(&path, PAN_BUFFERS_COUNT as u32)?;
    println!("[smoke] open OK");

    let slot_bytes = PAN_BUFFER_SIZE * PAN_LINE_SIZE;
    println!("[smoke] slot size = {} bytes ({} rows x {} bytes/row)",
             slot_bytes, PAN_BUFFER_SIZE, PAN_LINE_SIZE);

    // Confirm the kernel actually mmap'd the right amount and our slots
    // contain the initialized zero pan-buffer pattern (preamble cells
    // should have specific control flags).
    let s0 = fb.slot_view(0);
    let cell0 = u32::from_le_bytes(s0[0..4].try_into().unwrap());
    println!("[smoke] slot 0 cell 0 = 0x{:08x} (expect 0x000430xx after fill_first_line)",
             cell0);
    assert_eq!(cell0 & 0xffff_0000, 0x0043_0000,
               "slot 0 row 0 cell 0 doesn't have the firstLine base value");

    // The rm2 fb driver requires an unblank before any pan is accepted —
    // FBIOPAN_DISPLAY returns EINVAL while blanked.
    println!("[smoke] unblank to slot 0");
    fb.unblank(0)?;

    println!("[smoke] pan to slot 0");
    fb.pan(0)?;

    println!("[smoke] pan to slot {} (the spare slot used for idle)", PAN_BUFFERS_COUNT);
    fb.pan(PAN_BUFFERS_COUNT as u32)?;

    println!("[smoke] re-blank before exit");
    fb.blank()?;

    println!("[smoke] dropping Framebuffer (will munmap + close fd)");
    drop(fb);

    println!("[smoke] OK");
    Ok(())
}
