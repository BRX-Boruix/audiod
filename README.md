# audiod

**简体中文** | [English](#english)

BORUIX **用户态音频混音守护进程**（`plan_audio_vfs.md` 批次四）。

`audiod` 把内核音频节点从**哑管道**变成可迭代的混音器：内核的 `stream/N` 与 `dsp` 节点对 PCM
零知识、只搬字节，而**混音是策略而非机制**，因此放在用户态，使算法能在不改内核的前提下演进。

架构约定：内核挂载 `/devices/audio/stream/0..AUDIO_STREAM_COUNT-1`（各路生产者写入）与
`/devices/audio/dsp`（独占输出端）。`audiod` 读各路 PCM，混音后写回 `dsp`，
由 `intel-hda` 取走喂 DMA。

---

## 职责与架构

```
生产者 -> /devices/audio/stream/0 ┐
生产者 -> /devices/audio/stream/1 ┤
生产者 -> /devices/audio/stream/2 ┼-> audiod（用户态混音）-> /devices/audio/dsp -> intel-hda -> DMA
生产者 -> /devices/audio/stream/3 ┘
```

**为何要用户态中间层**（而不是内核里混音）：内核音频节点是哑管道，对 PCM 零知识。混音是策略，
放用户态才能在不改内核的前提下迭代算法——M2 混音、M3 欠载、M4 音量、M5 重采样、M6 趋势观测
全部在本进程内完成。

**独占消费者的顺序约束（重要）**：`dsp` 是独占消费者语义，消费者必须 `attach()`。本进程
启动即 attach 且**永不 detach**（守护进程）。故任何其它需要 attach 的东西都必须在本进程之前
运行——与 `intel-hda` 对 A2 造成的约束同源。init 的启动顺序已按此约定排列。

## 代码结构

工程刻意拆成 **库 + 二进制** 两个 target：

- **`src/lib.rs`** — 纯 DSP 核心。格式转换、相加、钳位、斜坡、重采样、趋势判定都是**纯函数**，
  与 I/O 分离，因而可以在**宿主机上穷举验证**，无需 QEMU 与真实音频设备。留在裸机二进制里的话，
  唯一可用的检查就是跑整个虚拟机读打印输出——既慢，又覆盖不到真实设备永远不会产生的输入。
- **`src/main.rs`** — 只提供 I/O 的二进制：attach、轮询读取、混音、写回 dsp。

库中共有 **48 个单元测试**（`cargo test --lib`，0.12s 跑完），覆盖每个边界条件。

## 里程碑

| 阶段 | 内容 |
| --- | --- |
| M1 | 原样转发 `stream/0` -> `dsp`，**不含任何混音算法**——先证明多路输入 -> 中间层 -> dsp 这条链路本身是通的。若此时输出与直写 dsp 不同，那一定是链路错误而非算法错误，故障定位被简化 |
| M2 | f32 混音核心：多路相加、固定衰减、钳位 |
| M3 | per-path underrun（滞回判定，非单轮空即算欠载） |
| M4 | 每路音量 + ~5ms 线性斜坡（消除爆音） |
| M5 | 线性插值重采样器（32.32 定点相位） |
| M6 | 供数趋势观测器（第一版**只观测不调节**） |

## 关键设计决策

### 混音增益用 1/n，而非 1/√n

`1/√n` 保持**非相关**声源的合成功率，适合统计意义上的无关素材混音；`1/n` 则**无条件**保证
和永不超过满量程。选 `1/n` 是因为这个保证是无条件的：`1/√n` 只约束平均功率，n 路**相关**
信号（全部播放同一音调——正是硬件测试所做的事）仍会削波，于是削波会成为常态而非兜底。
代价是非相关素材比本可以做到的更安静；那是增益决策，日后可逆，而削波是烙进输出的可听失真。

**增益必须用配置路数，而非当前存活路数**。用存活路数会让增益在单个缓冲区内变化，产生不连续
即爆音；更糟的是同一信号会因邻居那一刻恰好有没有数据而以不同电平渲染。因此调用方**不得**过滤
掉饥饿的路——每条配置路都传一个（可能为空的）切片，并声明配置总数。

### 欠载判定用滞回，而非单轮空

阈值取 `STARVATION_THRESHOLD = 32` **连续**空轮，来自实测而非直觉：QEMU 上混音循环远快于生产者
填充缓冲，某一路在多数轮次里合法为空；按每轮计数会在第 6016 轮、**两路生产者都健康**的情况下
报出 5871 次欠载。32 轮阈值远高于该轮询抖动，又能在几毫秒内检测到生产者停止。

**"从没供过数"不等于"饥饿"**：饥饿是曾经存在的东西消失了。缺这个判断，尚未启动的路会与停止的
路看起来一样，启动瞬态会被误报为故障。

### 重采样位置与定点相位

**音量在重采样之后施加**：斜坡以**输出**采样率计时，若在重采样前施加，同一斜坡在不同源速率下
时长就不同，5ms 这个设计值只对 48k 源成立。

**相位累加用定点而非 f32**：读位置每输出样本推进一个分数值，f32 累积会随位置增长丢失精度——
位置接近 2^23 后，加上小于尾数分辨率的步长毫无效果，于是输出样本重复、流变慢。这种漂移很渐进，
听起来只是音调略偏，比硬故障更难察觉。定点没有这个失效模式。

**采样率由内核报告，而非本进程假定**：`stream/N` 提供只读 `rate` 属性。若写死 48000，任何一路
被配成别的采样率时都会以错误速度播放，而且听起来只是音调略偏，极难归因。

### 漂移观测看**方向**而非电平

瞬时水位对此无用：生产者突发写入，水位逐轮大幅摆动而均值不动。因此比较近窗口与较早窗口的均值，
并看**变化**而非电平——稳定停在 90% 不是泄漏。经窗口平均 128 个样本，漂移小于 1% 视为噪声而非
移动：对抖动做出反应会把正常波动变成音高抖动，比它试图修复的漂移更糟。

## 失败模式（S20：先定义再写正常路径）

| 情形 | 行为 |
| --- | --- |
| `attach` 失败（已被别的消费者占用） | 如实报错并**退出** |
| `stream/N` / `dsp` 打不开 | 如实报错并退出 |
| `stream/N` 暂时无数据（`EAGAIN`） | 本轮**跳过**，不是错误 |
| `dsp` ring 满（`EAGAIN`） | 让出 CPU 后**重试同一段数据**，绝不丢弃 |
| 非 `EAGAIN` 的硬错误 | 如实报错并退出（不静默消失） |

所有失败路径都返回非 0，使 init 的 `waitpid` 能如实观察到"混音器挂了"。**绝不静默丢数据**是
贯穿性纪律：例如混音输出长度取**最长**输入，短的路在其结尾之后视为静音，而不是截断其它路。

## 关键常量

| 常量 | 值 | 说明 |
| --- | --- | --- |
| `MIX_INPUTS` | 4 | 必须与内核 `vfs::AUDIO_STREAM_COUNT` 一致 |
| `MIX_READ_BYTES` | 4096 | 每轮每路最大读取字节数；**必须帧对齐**，否则读到半帧会错位、左右声道互换 |
| `FRAME_BYTES` | 4 | s16 立体声一帧；与 dsp 属性文件报告的 format 对应 |
| `SAMPLE_RATE_HZ` | 48000 | 混音总线速率（单一速率） |
| `RAMP_SECONDS` | 0.005 | 音量斜坡时长（约 240 帧 @48kHz） |
| `SUPPORTED_INPUT_RATES` | 48k / 44.1k / 32k / 22.05k / 16k | 只支持有测试覆盖的比率；通用比率需多相 FIR，已在计划中明确排除——诚实的边界就是一张表 |
| `PROGRESS_EVERY` | 65536 | 进度日志间隔（按实测修正：原 64 轮在墙上仅约 16ms，刷出 72,000+ 行淹没 shell 提示符） |

## 构建与测试

`build.rs` 仅在 `target_os == "none"` 时注入 `linker.ld` 与 `-no-pie`（强制 `ET_EXEC`，
内核 ELF 加载器只接受它；缺此注入会把段链接到 vaddr 0x0，与内核用户半区冲突，加载器无法装载）。

```bash
# 宿主机上跑纯 DSP 单元测试（无需 QEMU、无需音频设备）
cargo test --lib

# 交叉编译裸机用户态二进制
cargo build --release --target x86_64-unknown-none
```

产物需部署为 BORUIX 系统中的用户态程序，由 `init` 按启动顺序拉起。

## 依赖

- [`libsys`](../libsys) —— BORUIX 用户态系统调用封装
- [`intel-hda`](../intel-hda) —— `dsp` 的底层驱动，取走 PCM 喂 DMA

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。

---

# English

[简体中文](#audiod) | **English**

The BORUIX **user-space audio mixing daemon** (`plan_audio_vfs.md`, batch four).

`audiod` turns the kernel's audio nodes from **dumb pipes** into an iterable mixer: the kernel's
`stream/N` and `dsp` nodes have zero knowledge of PCM and only move bytes, while **mixing is a
policy, not a mechanism** — so it lives in user space, letting the algorithm evolve without
touching the kernel.

Architectural contract: the kernel exposes `/devices/audio/stream/0..AUDIO_STREAM_COUNT-1`
(where producers write) and `/devices/audio/dsp` (the exclusive output). `audiod` reads PCM
from each path, mixes, and writes back to `dsp`, from which `intel-hda` feeds DMA.

---

## Responsibilities and architecture

```
producer -> /devices/audio/stream/0 ┐
producer -> /devices/audio/stream/1 ┤
producer -> /devices/audio/stream/2 ┼-> audiod (user-space mix) -> /devices/audio/dsp -> intel-hda -> DMA
producer -> /devices/audio/stream/3 ┘
```

**Why a user-space middle layer** (rather than mixing in the kernel): the kernel's audio nodes
are dumb pipes with zero PCM knowledge. Mixing is policy, and putting it in user space is what
lets the algorithm be iterated without changing the kernel — M2 mixing, M3 underruns, M4 volume,
M5 resampling and M6 trend observation all happen inside this one process.

**Exclusive-consumer ordering constraint (important)**: `dsp` has exclusive-consumer semantics,
and a consumer must `attach()`. This process attaches at startup and **never detaches** (it is a
daemon). Any other component that needs to attach must therefore run before this process — the
same constraint `intel-hda` imposes on A2. The init startup order already respects this.

## Code layout

The project is deliberately split into a **library + binary** pair:

- **`src/lib.rs`** — the pure DSP core. Format conversion, summing, clamping, ramping,
  resampling and trend decisions are all **pure functions**, separated from I/O so they can be
  **exhaustively verified on the host**, with no QEMU and no audio device. Left inside the
  bare-metal binary, the only available check would be running the whole VM and reading printed
  output — slow, and unable to cover inputs a real device never produces.
- **`src/main.rs`** — the I/O-only binary: attach, poll, mix, write back to dsp.

The library carries **48 unit tests** (`cargo test --lib`, completing in 0.12s), pinning every boundary condition.

## Milestones

| Stage | Content |
| --- | --- |
| M1 | Forward `stream/0` to `dsp` verbatim, with **no mixing algorithm at all** — first prove that the multi-input -> middle layer -> dsp chain works. If the output then differs from writing dsp directly, it is a chain bug and not an algorithm bug, which simplifies fault localization |
| M2 | f32 mixing core: N-path summing, fixed attenuation, clamping |
| M3 | Per-path underruns (hysteresis, not "one empty round counts") |
| M4 | Per-path volume plus a ~5 ms linear ramp (no clicks) |
| M5 | Linear-interpolation resampler (32.32 fixed-point phase) |
| M6 | Supply trend observer (first version **observes only, never corrects**) |

## Key design decisions

### Mix gain is 1/n, not 1/√n

`1/√n` preserves summed **power** for uncorrelated sources, which suits a statistical mix of
unrelated material. `1/n` instead **unconditionally** guarantees the sum can never exceed full
scale. `1/n` is chosen because that guarantee is unconditional: `1/√n` bounds only average
power, so n correlated paths (all playing the same tone — exactly what a hardware test does)
still clip, and clipping would then be the normal outcome rather than the fallback. The cost is
that uncorrelated material is quieter than it could be; that is a gain decision, reversible
later, whereas clipping is audible distortion baked into the output.

**Gain uses the configured path count, not the count alive at a given moment.** Using the live
count would make gain vary within one buffer, producing a discontinuity — a click — and worse,
the same signal would render at different levels depending on whether a neighbour happened to
have data that instant. Callers must therefore **not** filter out starved paths: they pass one
(possibly empty) slice per configured path and state the configured total.

### Underrun detection uses hysteresis

The threshold is `STARVATION_THRESHOLD = 32` **consecutive** empty rounds, chosen from
measurement rather than intuition: on QEMU the mixer loop runs far faster than producers fill
buffers, so a path is legitimately empty in most rounds; counting every one produced 5871
underruns by round 6016 with **both producers healthy**. A threshold of 32 is comfortably above
that polling jitter while still detecting a stopped producer within a few milliseconds.

**"Never delivered" is not "starving"**: starvation is the absence of something that was
previously present. Without that distinction, a path merely not yet started looks identical to
one that stopped, and the startup transient is reported as a fault.

### Resampling order and fixed-point phase

**Volume is applied after resampling**: the ramp is timed at the **output** sample rate, so
applying it before resampling would make the same ramp last different durations for different
source rates, and the 5 ms design value would hold only for 48 kHz sources.

**Phase accumulation is fixed point, not f32**: the read position advances by a fractional amount
per output sample, and accumulating that in f32 loses precision as the position grows — once near
2^23, adding a step smaller than the mantissa resolution does nothing at all, so output samples
repeat and the stream slows down. That drift is gradual and sounds like a slightly wrong pitch,
far harder to notice than a hard failure. Fixed point has no such failure mode.

**Sample rates are reported by the kernel, not assumed by this process**: each `stream/N`
exposes a read-only `rate` attribute. Hard-coding 48000 would make any path configured at
another rate play at the wrong speed — and it would only sound slightly off-pitch, which is very
hard to attribute.

### Drift observation looks at *direction*, not level

Instantaneous watermark is useless for this: producers write in bursts, so the level swings
widely round to round while its average stays put. Comparing a recent window average against an
older one, and looking at the **change** rather than the level, keeps a steadily-full buffer from
being reported as a problem — sitting at 90% forever is not a leak. Averaging 128 samples per
window brings the noise well below the drift being detected, and drift under 1% counts as jitter
rather than movement: acting on jitter turns normal variation into pitch wobble, worse than the
drift it was trying to fix.

## Failure modes (S20: defined before the happy path)

| Situation | Behaviour |
| --- | --- |
| `attach` fails (another consumer holds it) | Report honestly and **exit** |
| `stream/N` / `dsp` cannot be opened | Report honestly and exit |
| `stream/N` momentarily has no data (`EAGAIN`) | **Skip** this round; not an error |
| `dsp` ring full (`EAGAIN`) | Yield, then **retry the same data** — never discard |
| Any non-`EAGAIN` hard error | Report honestly and exit (never vanish silently) |

Every failure path returns non-zero so init's `waitpid` can honestly observe that the mixer died.
**Never silently lose data** is a cross-cutting discipline: the mix output length is the
**longest** input, treating a short path as silent past its end rather than truncating the others.

## Key constants

| Constant | Value | Notes |
| --- | --- | --- |
| `MIX_INPUTS` | 4 | Must match the kernel's `vfs::AUDIO_STREAM_COUNT` |
| `MIX_READ_BYTES` | 4096 | Max bytes read per path per round; **must be frame-aligned**, else a half-frame read desynchronizes and swaps left/right |
| `FRAME_BYTES` | 4 | One s16 stereo frame; corresponds to the format reported by the dsp attribute file |
| `SAMPLE_RATE_HZ` | 48000 | The mix bus rate (exactly one rate) |
| `RAMP_SECONDS` | 0.005 | Volume ramp duration (~240 frames at 48 kHz) |
| `SUPPORTED_INPUT_RATES` | 48k / 44.1k / 32k / 22.05k / 16k | Only ratios with test coverage; general ratios need a polyphase FIR, explicitly out of scope — the honest boundary is a list |
| `PROGRESS_EVERY` | 65536 | Progress log interval (corrected from measurement: 64 rounds is only ~16 ms of wall time and flooded the shell with 72,000+ lines) |

## Building and testing

`build.rs` injects `linker.ld` and `-no-pie` only when `target_os == "none"` (forcing
`ET_EXEC`, the only kind the kernel ELF loader accepts; without this injection sections link to
vaddr 0x0, colliding with the kernel's user half, and the loader cannot load it).

```bash
# Run the pure DSP unit tests on the host (no QEMU, no audio device)
cargo test --lib

# Cross-compile the bare-metal user-space binary
cargo build --release --target x86_64-unknown-none
```

The artifact is deployed as a user-space program in the BORUIX system and launched by `init`
in the prescribed startup order.

## Dependencies

- [`libsys`](../libsys) — the BORUIX user-space syscall wrapper
- [`intel-hda`](../intel-hda) — the driver under `dsp`, feeding DMA from the PCM written to it

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
