# audiod

**简体中文** | [English](#english)

BORUIX 的**用户态音频混音守护进程**。

内核把每个音频通道暴露成一个"哑管道"：`/devices/audio/stream/N` 只负责搬运字节，
对 PCM 格式一无所知。`audiod` 从这些通道读入音频、混合成一路，再写入独占输出
`/devices/audio/dsp`，由内核音频驱动取走交给 DMA 播放。

把混音放在用户态而不是内核里，是为了让混音算法能独立演进——调整混音、音量、重采样
都不需要重新编译内核。

---

## 架构

```
生产者 -> /devices/audio/stream/0 ┐
生产者 -> /devices/audio/stream/1 ┤
生产者 -> /devices/audio/stream/2 ┼-> audiod（用户态混音）-> /devices/audio/dsp -> DMA -> 扬声器
生产者 -> /devices/audio/stream/3 ┘
```

默认混合 4 路输入。每路可以有不同的采样率，`audiod` 会把它们统一到内部总线速率后相加。

## 功能

- **多路混音** —— 将 N 路输入相加为一路输出，各路等权衰减，保证结果不越界
- **每路独立音量** —— 支持运行时调节，音量变化走约 5 ms 线性斜坡，避免切换时的爆音
- **采样率转换** —— 支持 48 kHz / 44.1 kHz / 32 kHz / 22.05 kHz / 16 kHz 输入，线性插值重采样到 48 kHz 总线
- **欠载检测** —— 逐路跟踪供数中断，区分"还没开始供数"与"供数中断"
- **缓冲趋势观测** —— 判断某路的缓冲水位是在持续上涨还是持续下降

音频格式为 16-bit 有符号整型、立体声交错、48 kHz。

## 设计要点

### 混音衰减用 1/n 而非 1/√n

多路混音时每路乘上固定衰减。常见做法是乘 `1/√n`（保持非相关声源的总功率不变），
这里选择 `1/n`。

原因是 `1/n` 提供的保证是**无条件**的：任意一路的样本都在 `[-1, 1]` 内，因此 `n` 路之和
除以 `n` 必然也在 `[-1, 1]` 内，永不削波。而 `1/√n` 只约束平均功率——当多路信号**高度相关**
时（例如多路播放同一个测试音，这正是硬件测试会做的事）依然会削波。一旦削波成为常态而非兜底，
输出就会出现可听失真。代价是非相关素材比理论值更轻，但这只是增益取值，日后可调；削波则是已经
烙进输出的失真。

衰减系数取决于**配置的路数**，而不是"当前有几路真的给了数据"。否则每路是否恰好在某个瞬间有数据
会直接影响其余各路的声音大小，增益在单个缓冲区内跳变，听起来就是爆音。因此某路暂时没有数据时，
它贡献静音，但不改变整体增益。

### 欠载检测需要滞回

某一路"本轮没读到数据"并不等于出问题。混音循环的轮询速度远快于生产者填充缓冲的速度，所以某路在
多数轮次里合法地为空。只有当它**连续 32 轮**都没有数据、而此前确实供过数据时，才判定为欠载。

"从没供过数"与"供数中断"必须区分：前者是正常的启动过程，后者才是故障。没有这个区分，任何一路
在启动阶段都会被误报为欠载。

### 重采样用定点累加

重采样时读位置每输出一个样本就前进一个分数值。如果用浮点数累加，位置越大精度损失越明显：当位置
接近 2^23 后，加上一个小于尾数分辨率的步长完全无效，于是输出样本开始重复、音频变慢。这种漂移很
渐进，听感上只是音调略微偏低，比直接崩溃难察觉得多。定点数没有这个失效模式。

音量施加减在重采样**之后**，这样斜坡以输出采样率计时，约 5 ms 的设计值对任何源采样率都成立。

### 采样率来自内核

每路输入的采样率通过内核提供的只读属性读取，而不是假定为 48 kHz。若某路被配置成其他采样率，
写死值会让它以错误速度播放，而听感上只是音调略偏，很难归因。

## 文件结构

```
audiod/
├── src/
│   ├── lib.rs    # 混音核心：格式转换、相加、限幅、音量斜坡、重采样、趋势判定（纯函数）
│   └── main.rs   # 程序入口与全部 I/O：打开设备、轮询读取、混音、写回
├── build.rs      # 注入链接脚本
├── linker.ld     # 用户态程序段布局
└── Cargo.toml
```

混音逻辑全部放在 `lib.rs` 中作为**纯函数**实现，与设备 I/O 分离。这样格式转换、限幅、重采样这些
容易出边界错误的地方，可以在开发机上直接穷举测试，不需要启动虚拟机和真实音频设备。

## 构建与测试

```bash
# 在开发机上运行混音核心的单元测试（不需要虚拟机和音频设备）
cargo test --lib

# 交叉编译裸机用户态程序
cargo build --release --target x86_64-unknown-none
```

当前单元测试覆盖 48 个用例，全部通过。

编译产物部署到 BORUIX 系统中作为用户态程序运行。

## 运行前置条件

`dsp` 输出端是**独占**的：同一时刻只能有一个消费者持有它。`audiod` 启动时会取得所有权并且
在整个生命周期内持有，因此任何其他需要访问 `dsp` 的组件必须在它之前启动。BORUIX 的 init 启动
顺序已经按这个约束排好。

## 出错时的行为

`audiod` 是常驻进程。任何无法恢复的错误都会让它明确报错并退出，而不是静默降级——退出码非 0，
调用方（init）可以直接观察到混音器已经停止。

| 情况 | 行为 |
| --- | --- |
| 输出端已被其他消费者占用 | 报错并退出 |
| 输入或输出通道打不开 | 报错并退出 |
| 某路本次没有数据 | 跳过本轮，等待下次（正常情况，不退出） |
| 输出缓冲已满 | 让出 CPU 后重试同一批数据，不丢弃音频 |
| 其他错误 | 报错并退出 |

音频数据不会被静默丢弃：混音输出的长度以最长的输入为准，短的那路在其数据结束之后按静音处理，
而不是把其他路截断。

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。

---

# English

[简体中文](#audiod) | **English**

The BORUIX **user-space audio mixing daemon**.

The kernel exposes each audio channel as a "dumb pipe": `/devices/audio/stream/N` only moves
bytes and knows nothing about PCM format. `audiod` reads audio from those channels, mixes them
into a single stream, and writes it to the exclusive output `/devices/audio/dsp`, from which the
kernel audio driver feeds DMA for playback.

Mixing lives in user space rather than in the kernel so the mixing algorithm can evolve
independently — adjusting mixing, volume, or resampling never requires rebuilding the kernel.

---

## Architecture

```
producer -> /devices/audio/stream/0 ┐
producer -> /devices/audio/stream/1 ┤
producer -> /devices/audio/stream/2 ┼-> audiod (user-space mix) -> /devices/audio/dsp -> DMA -> speaker
producer -> /devices/audio/stream/3 ┘
```

Four input channels are mixed by default. Each channel may run at a different sample rate;
`audiod` brings them all to the internal bus rate before summing.

## Features

- **Multi-channel mixing** — sums N inputs into one output with equal attenuation per channel,
  guaranteeing the result never exceeds full scale
- **Independent per-channel volume** — adjustable at runtime; changes ramp over roughly 5 ms to
  avoid clicks
- **Sample rate conversion** — accepts 48 kHz / 44.1 kHz / 32 kHz / 22.05 kHz / 16 kHz input and
  resamples linearly to the 48 kHz bus
- **Underrun detection** — tracks supply interruptions per channel, distinguishing "has not
  started supplying yet" from "stopped supplying"
- **Buffer trend observation** — determines whether a channel's buffer level is steadily rising or
  falling

Audio format is 16-bit signed integer, stereo interleaved, 48 kHz.

## Design notes

### Mix attenuation uses 1/n, not 1/√n

Each channel is scaled by a fixed attenuation before summing. The common choice is `1/√n`, which
preserves total power for uncorrelated sources; this project uses `1/n` instead.

The reason is that `1/n` gives an **unconditional** guarantee: every sample lies within
`[-1, 1]`, so the sum of `n` channels divided by `n` must also lie within `[-1, 1]` and can
never clip. `1/√n` bounds only average power — when several channels are highly **correlated**
(for instance several playing the same test tone, exactly what hardware testing does) the sum
still clips. Once clipping becomes the normal outcome rather than a fallback, audible distortion
is baked into the output. The cost is that uncorrelated material is quieter than it theoretically
could be, but that is merely a gain choice, adjustable later; clipping is distortion already
committed to the output.

The attenuation is derived from the **configured** channel count, not from how many channels
happened to deliver data that instant. Otherwise whether one channel has data at a given moment
would change the loudness of all the others, making gain jump within a single buffer — which is
audible as a click. A channel with no data therefore contributes silence without altering the
overall gain.

### Underrun detection needs hysteresis

A channel reading no data this round is not by itself a problem. The mixing loop polls far faster
than producers fill buffers, so a channel is legitimately empty in most rounds. It counts as an
underrun only after **32 consecutive** empty rounds, and only if that channel had actually
supplied data before.

"Never supplied" and "stopped supplying" must be distinguished: the former is normal startup, the
latter is a fault. Without that distinction every channel would be falsely reported as underrunning
during startup.

### Resampling accumulates in fixed point

During resampling the read position advances by a fractional amount per output sample. Accumulating
that in floating point loses precision as the position grows: past roughly 2^23, adding a step
smaller than the mantissa resolution does nothing at all, so output samples begin repeating and the
audio slows down. The drift is gradual and sounds only like a slightly flat pitch — far harder to
notice than a crash. Fixed point has no such failure mode.

Volume is applied **after** resampling, so the ramp is timed at the output rate and the ~5 ms
design value holds for any source rate.

### Sample rates come from the kernel

Each input's sample rate is read from a read-only attribute the kernel provides, rather than
assuming 48 kHz. If a channel is configured at another rate, a hard-coded value would play it at
the wrong speed — and that sounds merely like a slightly off pitch, making it very hard to trace.

## Project layout

```
audiod/
├── src/
│   ├── lib.rs    # mixing core: format conversion, summing, limiting, volume ramps, resampling, trend detection (pure functions)
│   └── main.rs   # entry point and all I/O: opening devices, polling, mixing, writing back
├── build.rs      # injects the linker script
├── linker.ld     # user-space program section layout
└── Cargo.toml
```

All mixing logic lives in `lib.rs` as **pure functions**, separated from device I/O. This lets the
parts most prone to boundary errors — format conversion, limiting, resampling — be exhaustively
tested on a development machine, with no virtual machine and no audio hardware.

## Building and testing

```bash
# Run the mixing core unit tests on a development machine (no VM, no audio hardware)
cargo test --lib

# Cross-compile the bare-metal user-space program
cargo build --release --target x86_64-unknown-none
```

The unit tests currently cover 48 cases, all passing.

The build artifact is deployed as a user-space program in a BORUIX system.

## Runtime prerequisites

The `dsp` output is **exclusive**: only one consumer may hold it at a time. `audiod` takes
ownership at startup and holds it for its entire lifetime, so any other component needing access to
`dsp` must start before it. The BORUIX init startup order already respects this constraint.

## Behaviour on failure

`audiod` is a long-running process. Any unrecoverable error makes it report the problem and exit
rather than degrade silently — the exit code is non-zero, so the caller (init) can directly observe
that the mixer has stopped.

| Situation | Behaviour |
| --- | --- |
| Output already held by another consumer | Report and exit |
| Input or output channel cannot be opened | Report and exit |
| A channel has no data this round | Skip this round and wait (normal, does not exit) |
| Output buffer full | Yield the CPU and retry the same data; audio is never dropped |
| Any other error | Report and exit |

Audio data is never silently dropped: the mix output length follows the longest input, treating a
short channel as silent past its end rather than truncating the others.

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
