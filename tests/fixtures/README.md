# Test fixtures

This directory holds vendor panel-firmware files (`*.wbf`) used by the
waveform loader tests. They are gitignored — pull them from your own
rM2 to populate:

```
scp root@remarkable:'/var/lib/uboot/*.wbf' tests/fixtures/
```

Tests that need a fixture skip cleanly with a printed message when the
file is absent. To run them:

```
cargo test --target armv7-unknown-linux-gnueabihf --release
```
