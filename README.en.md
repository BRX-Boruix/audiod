# audiod

BORUIX's audio mixing daemon: combines multiple audio inputs into one stream and writes it to the output device.

[简体中文](README.md)

Started by the system init process at boot; runs for the lifetime of the system.

## What it does

```
producers → /devices/audio/stream/0..3 → audiod → /devices/audio/dsp → hardware driver → hardware
```

Each producer writes PCM into its own audio stream node; this process mixes up to 4 of them into one:

- A path with no data contributes silence and leaves the gain of the others untouched — losing a path removes one source, it does not click
- Input sample rates that differ from the output rate are converted proportionally; each rate is reported by the system, never assumed
- Volume changes ramp over about 5 ms, with no step
- Per-path connection state and starvation counts go into the periodic log

## Failure behaviour

- An input or output node cannot be opened: report and exit
- An input temporarily has no data: skip the round, not an error
- The output buffer is full: yield the CPU and retry the same data, never drop it
- Any other error: report and exit

Every failure path exits non-zero, so the process that started the daemon can see that it died.

## Known limitations

- The output device accepts a single consumer; programs that need it follow the boot-order convention
- The number of mix paths is fixed at 4, matching the stream nodes the system mounts; unmounted paths are skipped
- There is no external volume control interface; volume is set inside the mixer

## Building

```bash
cargo build --release
cargo test
```

The mixing core is pure functions; its tests run on the host and need no audio device.

## Repository layout

```
audiod/
├── Cargo.toml    # package manifest
├── build.rs      # injects the linker script
├── linker.ld     # user-space segment layout
└── src/
    ├── lib.rs    # mixing core: format conversion, summing, clamping, resampling
    └── main.rs   # mixing loop and failure handling
```

## Related projects

- [`intel-hda`](https://github.com/BRX-Boruix/intel-hda) — hardware driver for the output device
- [`audiofile`](https://github.com/BRX-Boruix/audiofile) — WAV player writing straight to the output device
- [`audioe2e`](https://github.com/BRX-Boruix/audioe2e) — audio end-to-end acceptance test
- [`libsys`](https://github.com/BRX-Boruix/libsys) — user-space system call wrappers

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
