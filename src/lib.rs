//! Software TCON for the reMarkable 2 e-paper panel.
//!
//! `swtcon` is a pure-userspace driver for the rM2's EPD panel. It talks
//! `/dev/fb0` directly using the SWTCON wire format, with no dependency on
//! libqsgepaper, the rm2fb shim, or any vendor symbols — so it keeps
//! working across firmware updates that change the stock UI's
//! user-space components.
//!
//! See [`Swtcon`] for the public API. Briefly:
//!
//! ```ignore
//! let swtcon = Swtcon::open(Path::new("/dev/fb0"), wbf_path)?;
//! swtcon.update(&image, Rect::full(), Mode::Gc16)?;
//! swtcon.flush(Duration::from_secs(10))?;
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
// ARMv7 NEON intrinsics in `core::arch::arm` are still gated behind a feature
// flag on nightly (rust-lang/rust#111800). When stabilized, drop this and the
// crate works on stable.
#![cfg_attr(all(target_arch = "arm", target_feature = "neon"),
            feature(stdarch_arm_neon_intrinsics))]

pub mod constants;
pub mod fb;
pub mod runtime;
pub mod strip_encoder;
pub mod swtcon;
pub mod waveform;
pub mod wbf;

// Re-export the public API at the crate root for convenience.
pub use crate::swtcon::{Error, Mode, Rect, Swtcon};
