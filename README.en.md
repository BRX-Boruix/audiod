# audiod

BORUIX's **user-space audio mixing daemon**: it mixes several audio inputs and sends the result to the output device.

[简体中文](README.md)

## What it does

```
producers → stream/0..3 → audiod (mixing) → dsp → hardware driver → hardware
```

Several inputs each write to an audio stream node; this process **mixes them into one stream**, writes it to the output device, and the hardware driver reads it out.

Current capabilities:

| Capability | Details |
| --- | --- |
| **Multi-stream mixing** | Up to 4 inputs mixed into one output |
| **Tolerance for missing streams** | An unconnected stream does not affect the rest — better to mix fewer streams than to fail entirely over one absent input |
| **Sample rate conversion** | Converts in proportion when the input rate differs from the output rate |
| **Volume adjustment** | With ramping, avoiding clicks from abrupt level changes |
| **Underrun accounting** | Records each stream's connection state and shortfalls in data supply |

## Why mixing lives in user space

The kernel's audio node is a **dumb pipe** — it knows nothing about PCM content and only moves bytes. Mixing is **policy, not mechanism**, and keeping it in user space means the algorithms can be iterated without touching the kernel.

The cost is an extra layer. That cost is worth paying: algorithms change repeatedly while the kernel should stay stable — keep what changes where change belongs.

## Ordering constraint from exclusive output

The output device has **exclusive consumer** semantics — a consumer must "attach" before writing, and this process **never detaches** once attached (it is a daemon).

So any other program needing to attach that device **must start before this one**. The startup order is arranged accordingly.

## Behaviour on failure

| Situation | Handling |
| --- | --- |
| The output device is already claimed by another consumer | Report honestly and **exit** |
| An input or output node cannot be opened | Report honestly and exit |
| An input has no data for now | **Skip this round**; not an error |
| The output buffer is full | Yield the CPU and **retry the same block**, never discarding it |
| Any other hard error | Report honestly and exit (no silent disappearance) |

Every failure path returns non-zero, so the flow that started it can observe honestly that "the mixer died".

## Building

```bash
cargo build --release
```

Started by the system init process at boot, then resident.

## Layout

```
audiod/
├── Cargo.toml    # package definition
├── build.rs      # injects the linker script
├── linker.ld     # user-space section layout
└── src/
    └── main.rs   # the mixing loop and failure handling
```

## Related projects

- [`intel-hda`](https://github.com/BRX-Boruix/intel-hda) — the hardware driver reading the output device
- [`audioe2e`](https://github.com/BRX-Boruix/audioe2e) — audio-domain end-to-end acceptance
- [`audiofile`](https://github.com/BRX-Boruix/audiofile) — the WAV player
- [`libsys`](https://github.com/BRX-Boruix/libsys) — the user-space syscall wrapper

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
