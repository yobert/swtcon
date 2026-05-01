# swtcon

A pure-userspace software TCON for the reMarkable 2's e-paper panel. I'd
love to support other panels also--- if somebody wants to send me a hardware
sample I'll do my best!

`swtcon` drives the rM2's EPD panel directly through `/dev/fb0` using the
SWTCON wire format — no dependency on `libqsgepaper`, the `rm2fb` shim,
or any vendor symbols. The library is small, dependency-light (just
`libc` and `log`), and (hopefully) resilient to firmware updates that change
the stock UI's user-space components.

## Thanks

https://github.com/timower/rM2-stuff -- Was a huge help with their C++ swtcon implementation.
https://github.com/rmkit-dev/rmkit -- Where I started with this. But remarkable 2 no longer has rm2fb support unfortunately.

## AI disclaimer

I used claude code heavily for this development.

## Limitations

It's not as low latency as xiochitl. I'm working on this though.

## Quick start

Cargo:

```toml
[dependencies]
swtcon = "0.1"
```

Code:

```rust
use std::path::Path;
use std::time::Duration;
use swtcon::{Mode, Rect, Swtcon};

let swtcon = Swtcon::open(
    Path::new("/dev/fb0"),
    Path::new("/var/lib/uboot/320_R327_AFEC21_ED103TC2M1_VB3300-KCD_TC.wbf"),
)?;

// `image` is `1404 * 1872` `u16`s in screen orientation (origin
// top-left, row-major). The encoder reads `(low_byte >> 1) & 0xf` of
// each pixel as a 4-bit gray value.
let image: Vec<u16> = vec![0x001e; 1404 * 1872]; // gray nibble 15 = white

swtcon.update(&image, Rect::full(), Mode::Gc16)?;
swtcon.flush(Duration::from_secs(10))?;
```

[`Swtcon::open`](src/swtcon.rs) opens the framebuffer, parses the WBF,
runs the INIT clear synchronously so the panel is in a known white
state, and spawns the generator + vsync threads. On drop, the threads
are joined and the fb is `munmap`'d.

## Update modes

| `Mode`      | WBF mode | Phases @ 24 °C | Notes                                          |
|-------------|---------:|---------------:|------------------------------------------------|
| `Du`        |        1 |             25 | 1-bit black/white only. Use for pure B/W.      |
| `Gc16`      |        2 |             46 | 16-level grayscale, full quality.              |
| `Gc16Fast`  |        3 |             46 | Faster than `Gc16` with more visible ghosting. |
| `Glf`       |        6 |             10 | General-purpose / glide. Fastest grayscale.    |
| `Du4`       |        7 |              — | 4-level grayscale, fast.                       |
| `A2`        |        4 |             38 | 1-bit "animation" waveform.                    |

(Phase counts are from a `ED103TC2M1` panel at `temp_idx=7` ~ 24 °C.
They vary across temperature and across panel revisions.)

I think the fastest mode on the rM2 panel is `Glf` (10 phases). On the `ED103TC2M1`,
WBF mode 4 is not the fast animation mode the name "A2" suggests — its
phase count is similar to `Gc16`. Probably use `Glf` for fast pen-stroke updates.

End-to-end refresh latency on the rM2 (full panel, real hardware,
`Mode::Gc16`) is roughly 330–445 ms across 1–10 phases — dominated by a
fixed per-submit overhead (~330 ms) plus ~12 ms per phase. See
`examples/refresh_bench.rs` to measure on your panel.

## Truncated previews (`update_with_limit`)

For latency-sensitive UIs (e.g. live pen strokes), you can cap how many
waveform phases the generator emits and skip committing the partial
state to the change-tracking buffer:

```rust
swtcon.update_with_limit(&image, rect, Mode::Glf, Some(3))?;
```

The visible result appears `phase_count / phase_limit` * faster but
pixels won't be fully settled. The change-tracking buffer is not
committed for phase-limited updates, so a follow-up full update still
drives from the original src state to clean up the partial transitions.

## Temperature compensation

E-paper waveform timing is temperature-sensitive. `swtcon` reads the
panel's SY7636A PMIC sensor via sysfs (`/sys/class/hwmon/*` with
`name == "sy7636a_temperature"`, then `temp0` for °C) at `open` time
and picks the matching range in `waveforms.temp_ranges`. Falls back to
range index 7 (~24 °C) if the sensor isn't present or readable.

Picking a wrong range produces ghosting or weak contrast.

## Building

This is a normal cargo crate. To run on the rM2 you cross-compile to
`armv7-unknown-linux-gnueabihf`. A `.cargo/config.toml` is included
that wires up:

- `linker = "arm-linux-gnueabihf-gcc"` (Arch users: AUR
  `arm-linux-gnueabihf-gcc`, but you'll likely have to go through a
  stage1 and stage2 build first from the AUR.)
- `runner = ["qemu-arm-static", ...]` so `cargo test --target
  armv7-unknown-linux-gnueabihf` runs the test binaries locally
- `target-cpu=cortex-a9`, `target-feature=+neon,+vfp3`

Build + deploy + run a smoke test on the device:

```sh
cargo build --release --target armv7-unknown-linux-gnueabihf \
            --example swtcon_smoke
scp target/armv7-unknown-linux-gnueabihf/release/examples/swtcon_smoke \
    root@remarkable:/home/root/

ssh root@remarkable bash <<'REMOTE'
trap 'systemctl reset-failed xochitl 2>/dev/null; systemctl start xochitl' EXIT
systemctl stop xochitl && /home/root/swtcon_smoke
REMOTE
```

The stock UI must be stopped (it owns `/dev/fb0` while running). The
trap ensures it gets restarted even if the smoke crashes.

### Nightly requirement on armv7

ARMv7 NEON intrinsics in `core::arch::arm` are still gated behind a
nightly feature flag ([`stdarch_arm_neon_intrinsics`][1]). When that's
stabilized the crate works on stable everywhere; until then you need
nightly to build for armv7. Host (x86_64) builds and tests use the
scalar encoder path and work on stable.

[1]: https://github.com/rust-lang/rust/issues/111800

## Tests

```sh
cargo test                                                 # host scalar paths
cargo test --target armv7-unknown-linux-gnueabihf --release  # incl. NEON, via qemu
```

Tests that need a real `.wbf` fixture skip cleanly when the file isn't
present — see [`tests/fixtures/README.md`](tests/fixtures/README.md)
for how to populate from your own device.

## Module layout

| Module           | What it owns                                                                |
|------------------|-----------------------------------------------------------------------------|
| `wbf`            | WBF (waveform binary file) parser. Pure: takes `&[u8]`, no allocation.      |
| `waveform`       | Assembles a `WaveformPack` from a `Wbf`. Builds the per-(mode, temp, phase) tables the encoder consumes. |
| `strip_encoder`  | Per-strip pixel encoder. Scalar + NEON variants, bit-exact equivalent.       |
| `fb`             | Pan-buffer fill (host-testable) + Linux `Framebuffer` (`/dev/fb0`, mmap, FBIO* ioctls). |
| `runtime`        | Generator + vsync threads, slot-ring back-pressure, `SharedState`.          |
| `swtcon`         | Public ergonomic API: `Swtcon::open` / `update` / `flush`.                  |

## Hardware notes

- **Panel**: `ED103TC2M1` (rM2). Coordinate convention: 1404 × 1872,
  origin top-left in user space. The panel hardware itself is rotated
  90°; `swtcon` handles the transform internally.
- **rM2 driver quirk**: `FBIOPAN_DISPLAY` returns `EINVAL` while the
  panel is in the blanked state. Callers must `unblank()` at least once
  before any `pan()`. `Framebuffer::pan` documents this; the runtime
  handles it automatically.
- **WBF file**: stock firmware ships the panel's `.wbf` at
  `/var/lib/uboot/<id>.wbf` (e.g. `320_R327_AFEC21_ED103TC2M1_VB3300-KCD_TC.wbf`
  on rM2 OS 3.26.0.68). The exact filename varies per panel revision.
- **Validated on**: rM2 OS 3.26.0.68.

## License

MIT - see [LICENSE-MIT](LICENSE-MIT).
