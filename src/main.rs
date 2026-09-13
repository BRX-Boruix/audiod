//! BORUIX `audiod`：用户态音频混音守护进程（plan_audio_vfs.md 批次四）。
//!
//! **职责（M1 阶段）**：把混音器输入端 `/devices/audio/stream/0` 的 PCM
//! **原样转发**到输出端 `/devices/audio/dsp`。此阶段**不含任何混音算法**——
//! 目的是在引入算法之前，先证明多路输入 -> 中间层 -> dsp 这条链路本身是通的。
//! 若此时输出与直写 dsp 不同，那一定是链路错误而非算法错误，故障定位被简化。
//!
//! **为何要用户态中间层**（而不是内核里混音）：内核的音频节点是**哑管道**——
//! 对 PCM 零知识，只搬字节。混音是策略而非机制，放用户态才能在不改内核的
//! 前提下迭代算法（M2 混音、M3 欠载、M4 音量都在本进程内完成）。
//!
//! **独占消费者的顺序约束（重要）**：`dsp` 是独占消费者语义，消费者必须
//! `attach()`。本进程启动即 attach 且**永不 detach**（守护进程）。故任何其它
//! 需要 attach 的东西都必须在本进程之前运行——与 intel-hda 对 A2 造成的约束
//! 同源。init 的启动顺序已按此约定排列。
//!
//! **失败模式（S20，先定义再写正常路径）**：
//! - `attach` 失败（已被别的消费者占用）-> 如实报错并**退出**；
//! - `stream/0` / `dsp` 打不开 -> 如实报错并退出；
//! - `stream/0` 暂时无数据（EAGAIN）-> 本轮**跳过**，不是错误；
//! - `dsp` ring 满（EAGAIN）-> 让出 CPU 后**重试同一段数据**，绝不丢弃；
//! - 非 EAGAIN 的硬错误 -> 如实报错并退出（不静默消失）。
//!
//! 所有失败路径都返回非 0，使 init 的 waitpid 能如实观察到"混音器挂了"。

#![no_std]
#![no_main]
extern crate alloc;

use libsys::*;

/// 混音器输入端**目录**。各路为 `<STREAM_DIR>/<下标>`。
///
/// 内核挂 `stream/0..AUDIO_STREAM_COUNT-1`（见 vfs::AUDIO_STREAM_COUNT）。
/// 这里只写目录前缀，具体路径在启动时按下标拼出——避免硬编码字符串表（S13）。
const STREAM_DIR: &str = "/devices/audio/stream";

/// 输出端路径（哑管道：数据写入即被 intel-hda 取走喂 DMA）。
const DSP_PATH: &str = "/devices/audio/dsp";

/// 混音器参与混音的路数。
///
/// **必须与内核 `vfs::AUDIO_STREAM_COUNT` 一致**（4）。本进程打开 0..N-1；
/// 内核没挂的路会被 `NotFound` 跳过并如实记录，故即使两者暂时不一致也不会
/// 崩，只会减少实际混音路数——这是有意的容错（S20），不是掩盖配置错误。
const MIX_INPUTS: usize = 4;

/// 每轮从**每一路**读取的最大字节数。
///
/// **必须帧对齐**（`FRAME_BYTES` 的整数倍），否则读到半帧会与下一轮拼接错位，
/// 使左右声道互换。4096 = 1024 帧（约 21ms @48kHz）。
/// 与 `dsp` 的 64 KiB ring 相比足够小，使单次写通常放得下（背压罕见）。
const MIX_READ_BYTES: usize = 4096;

/// 一轮最多能读到的帧数。M6 用它把"本轮读到多少帧"归一化到 0..1。
///
/// 由 `MIX_READ_BYTES` **推导**而非另写一个数字：两者必须一致，
/// 否则归一化出来的"水位"与实际读取能力脱节（S15 单点定义）。
const MAX_FRAMES_PER_ROUND: usize = MIX_READ_BYTES / FRAME_BYTES;

/// 一个 PCM 帧的字节数（s16 立体声：2 声道 x 2 字节）。
///
/// 该值是**格式契约**的一部分，与 dsp 属性文件报告的 format 对应；
/// M4/批次五若要支持其它格式，必须让本常量与格式同步变化，而非各处硬编码。
const FRAME_BYTES: usize = 4;

/// 每帧的声道数。**单点定义在库中**（audiod::FORMAT_CHANNELS），
/// 这里只做别名，避免同一个事实有两处定义（S15）。
const FORMAT_CHANNELS: usize = audiod::FORMAT_CHANNELS;

/// Frames of silence emitted for a round in which no input produced data.
///
/// Matches one round of normal intake (MIX_READ_BYTES / FRAME_BYTES = 1024 frames,
/// about 21ms at 48kHz). The output timeline stays continuous, so producing paths
/// that resume later are not shifted in time. Emitting silence is not fabricating
/// data: it states accurately that no audio existed for this interval.
const SILENCE_ROUND_FRAMES: usize = MIX_READ_BYTES / FRAME_BYTES;

/// Rounds of total silence tolerated before an idle mixer stops writing to dsp.
///
/// With no connected producer there is nothing to keep the DMA fed; continuing to
/// push silence forever would busy-spin. A short grace period still covers the
/// startup transient before producers connect. The mixer never exits -- a producer
/// may attach at any time.
const SILENCE_GRACE_ROUNDS: u64 = 64;

/// Mixed-audio round at which the M4 demonstration changes stream/0 volume.
///
/// Counted over rounds that actually mixed audio (live_now > 0), NOT over total
/// loop iterations. Total iterations include long idle stretches -- measured at
/// 14144 rounds while audio ended near round 192 -- so a threshold on the total
/// fires after the music is over, proving nothing. This counter advances only
/// while sound is being produced, so the change is guaranteed to land inside the
/// audible window.
///
/// 64 mixed rounds is roughly 256 KiB of audio (about 1.4s at 48kHz): late enough
/// that startup transients are past, early enough to leave a long steady tail.
const DEMO_VOLUME_ROUND: u64 = 64;

/// 每多少轮打印一次进度。
///
/// **取值理由（已按实测修正）**：轮询一轮只搬 `MIX_READ_BYTES` = 4096 字节
/// （1024 帧，约 21ms 音频）。原取 64 轮，理由是"约 256 KiB / 1.4s 音频"——
/// 但那是**音频时间**，不是**墙上时间**：本循环不做实时节流，实测 7 分钟内
/// 跑到 1,747,776 轮，即每 64 轮一条的间隔在墙上只有约 16ms，于是刷出
/// **72,000+ 行**日志，把 shell 提示符彻底淹没（用户实测反馈）。
///
/// 改为 65536 轮：按实测速率约合墙上 4.5 秒一条，既能证明"在持续推进"，
/// 也不再淹没真实交互输出。判别条件仍用轮次（而非 `now()`）——`libsys::now()`
/// 每次都要走 SysFS 读 `/system/info/kernel`，放进每轮热路径代价过高。
const PROGRESS_EVERY: u64 = 65536;

/// rate 属性文本的最大长度（如 `"48000\n"` 共 6 字节；留足余量）。
///
/// 属性读取是**定长缓冲**读取：超出该长度的内容会被截断，而截断的 rate
/// 解析必然失败并如实报错——不会静默产生错误速率。
const RATE_BUF_BYTES: usize = 32;
/// 一路已打开的输入流。
///
/// `path` 保留是为了错误信息能指名道姓（编号 + 路径），而不是只报下标——
/// 排查时"哪一路"与"哪个文件"是两个都要立刻知道的信息。
struct Input {
    fd: u64,
    path: alloc::string::String,
    /// M5: resampler for this path, chosen from the rate the kernel reports for it.
    ///
    /// Built once at open time and never rebuilt: the rate is part of the stream's
    /// format contract, and changing it mid-stream would invalidate the phase the
    /// resampler has accumulated. A path whose rate cannot be read stays `None` and its
    /// samples are used as-is, which is only correct at the bus rate -- so the read
    /// failure is reported rather than silently assumed away.
    resampler: Option<audiod::StereoResampler>,
}


/// 把一路的原始 PCM 转成 f32、按需重采样、再施加音量，追加到 `out`。
///
/// 分离成独立函数而不是留在大循环里，是因为这里有三个**必须按顺序**发生的
/// 步骤，顺序错了不会崩、只会让声音不对（S24 单组件专注）：
///
/// 1. 字节 -> f32（按帧解析，左右分开）；
/// 2. 重采样到总线速率（若有重采样器）；
/// 3. 施加音量（逐样本推进斜坡）。
///
/// 顺序要点：音量在**重采样之后**施加。斜坡以**输出**采样率计时，
/// 若在重采样前施加，同一斜坡在不同源速率下的时长就不同，
/// 约 5ms 这个设计值便只对 48k 源成立——那正是隐性魔法值（S13）。
///
/// `resampler` 为 `None` 时按原样使用输入样本。这**只在**该路确实是总线速率
/// 时才正确，故取不到 rate 时 `read_stream_rate` 会如实告警而非静默走到这里（S20）。
fn convert_and_resample(
    raw: &[u8],
    resampler: Option<&mut audiod::StereoResampler>,
    src_plane_l: &mut alloc::vec::Vec<f32>,
    src_plane_r: &mut alloc::vec::Vec<f32>,
    interleaved: &mut alloc::vec::Vec<f32>,
    resample_out: &mut alloc::vec::Vec<f32>,
    out: &mut alloc::vec::Vec<f32>,
    ramp: &mut audiod::Ramp,
) {
    let src_frames = raw.len() / FRAME_BYTES;
    if src_frames == 0 {
        return;
    }
    src_plane_l.clear();
    src_plane_r.clear();
    let mut f = 0usize;
    while f < src_frames {
        let o = f * FRAME_BYTES;
        let l = i16::from_le_bytes([raw[o], raw[o + 1]]);
        let r = i16::from_le_bytes([raw[o + 2], raw[o + 3]]);
        src_plane_l.push(audiod::s16_to_f32(l));
        src_plane_r.push(audiod::s16_to_f32(r));
        f += 1;
    }

    match resampler {
        None => {
            // 该路已是总线速率（或取不到 rate 且已告警）：逐帧施加音量即可。
            out.reserve(src_frames * FORMAT_CHANNELS);
            let mut i = 0usize;
            while i < src_frames {
                let vg = ramp.next_sample();
                out.push(src_plane_l[i] * vg);
                out.push(src_plane_r[i] * vg);
                i += 1;
            }
        }
        Some(sr) => {
            // 交织回交错格式再交给 StereoResampler（它按平面拆分后各自重采样）。
            interleaved.clear();
            interleaved.reserve(src_frames * FORMAT_CHANNELS);
            let mut i = 0usize;
            while i < src_frames {
                interleaved.push(src_plane_l[i]);
                interleaved.push(src_plane_r[i]);
                i += 1;
            }
            let cap = sr.output_capacity(src_frames);
            resample_out.clear();
            resample_out.resize(cap * FORMAT_CHANNELS, 0.0);
            let n = sr.process(interleaved, resample_out);
            // 音量在**重采样之后**施加：斜坡以输出速率计时，
            // 若在重采样前施加，同一斜坡在不同源速率下的时长就不同。
            out.reserve(n * FORMAT_CHANNELS);
            let mut k = 0usize;
            while k < n {
                let vg = ramp.next_sample();
                out.push(resample_out[k * FORMAT_CHANNELS] * vg);
                out.push(resample_out[k * FORMAT_CHANNELS + 1] * vg);
                k += 1;
            }
        }
    }
}

/// 读取某一路输入流的采样率（Hz）。
///
/// 内核为每个 `stream/N` 提供只读的 `rate` 属性（见 vfs `audio.rs` 的 rate 节点）。
/// **格式由内核报告而非由本进程假定**——若这里写死 48000，任何一路被配成别的
/// 采样率时，混音器都会以错误速度播放它，而且听起来只是音调略偏，极难归因
/// （S13/S40：不臆测环境，读真实来源）。
///
/// 返回 `None` 表示该路没有可用的 rate（如内核版本不一致）。调用方如实记录
/// 并**不重采样**，而不是假定一个值继续跑。
fn read_stream_rate(stream_path: &str) -> Option<u32> {
    let path = alloc::format!("{}/rate", stream_path);
    let fd = match open(path.as_str(), OpenFlags::READ_ONLY, Permissions::readonly()) {
        Ok(fd) => fd,
        Err(e) => {
            logf(format_args!("warn: open({}) failed: {:?}; no resampling on this path", path, e));
            return None;
        }
    };
    let mut buf = [0u8; RATE_BUF_BYTES];
    let n = match read(fd, &mut buf) {
        Ok(n) => n,
        Err(e) => {
            logf(format_args!("warn: read({}) failed: {:?}; no resampling on this path", path, e));
            let _ = close(fd);
            return None;
        }
    };
    // 属性是普通文件，读完即关。失败要如实记录（S18 资源生命周期）。
    if close(fd).is_err() {
        logf(format_args!("warn: close({}) failed", path));
    }
    // 属性以文本给出（如 "48000\n"）。解析失败即如实报错，
    // 绝不 default 到 48000——那会把"读不到"伪装成"读到了 48k"（S20）。
    let text = match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s.trim(),
        Err(_) => {
            logf(format_args!("warn: rate of {} is not UTF-8; no resampling", stream_path));
            return None;
        }
    };
    match text.parse::<u32>() {
        Ok(rate) => Some(rate),
        Err(_) => {
            logf(format_args!("warn: rate {:?} unparsable; no resampling", text));
            None
        }
    }
}
/// 向 STDOUT 输出一行日志（`[audiod] ...`）。
fn log(msg: &[u8]) {
    let _ = write(STDOUT, b"[audiod] ");
    let _ = write(STDOUT, msg);
    let _ = write(STDOUT, b"\n");
}

/// 带格式的日志（alloc::format 动态拼接，可含数字/错误）。
fn logf(args: core::fmt::Arguments) {
    let s = alloc::format!("{}", args);
    log(s.as_bytes());
}

/// audiod 主流程：attach dsp -> 循环转发 stream/0。
///
/// 这是**守护进程**，正常情况下永不返回。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(_argc: isize, _argv: *const *const u8) -> i32 {
    log(b"audiod starting (batch-4 M2: N-stream f32 mix -> dsp)");

    // ---- 步骤 1：打开各路输入端（只读）与输出端（只写）----
    //
    // **本进程刻意不调 `AUDIO_ATTACH`（实测修正，见提交 f6a172d）**：
    // `attach` 的语义是"成为该节点的**消费**者"——读走数据的那一方。
    // 而 audiod 是 dsp 的**写者**：它把混音结果写进 dsp，由 intel-hda 读走喂硬件。
    // 二者角色不同，本可共存。dsp 的写入门禁只要求"**已有**消费者"
    // （`is_attached()`），并不要求写入方自己是消费者。
    //
    // 计划原文"audiod 启动即 AUDIO_ATTACH"会导致 EBUSY（intel-hda 已常驻占位）。
    //
    // 完整链路：生产者(们) -> stream/0..N-1 -> audiod(混音) -> dsp -> intel-hda -> 硬件。
    let mut inputs: [Option<Input>; MIX_INPUTS] = [const { None }; MIX_INPUTS];
    let mut opened = 0usize;
    let mut i = 0usize;
    while i < MIX_INPUTS {
        // 路径由下标生成（非硬编码字符串表）：路数改常量即可，S13/S15。
        let path = alloc::format!("{}/{}", STREAM_DIR, i);
        match open(path.as_str(), OpenFlags::READ_ONLY, Permissions::readonly()) {
            Ok(fd) => {
                let resampler = match read_stream_rate(path.as_str()) {
                    Some(rate) => match audiod::StereoResampler::new(rate, audiod::SAMPLE_RATE_HZ) {
                        Ok(r) => Some(r),
                        Err(e) => {
                            // A rate the resampler refuses is a real configuration error:
                            // the stream would be mixed at the wrong speed. Reporting and
                            // exiting is the honest option; guessing a rate is not.
                            logf(format_args!("FAIL: input {} rate {} unsupported: {:?}", i, rate, e));
                            return 1;
                        }
                    },
                    None => None,
                };
                inputs[i] = Some(Input { fd, path, resampler });
                opened += 1;
            }
            Err(e) => {
                // **如实区分两种失败**（S20）：
                //   - NotFound：内核没挂这一路（如路数配置不同）-> 跳过并记录；
                //   - 其它：真实错误 -> 退出，不假装在工作。
                if e == Error::NotFound {
                    logf(format_args!("input {} absent (NotFound); skipping", i));
                } else {
                    logf(format_args!("FAIL: open input {} failed: {:?}", i, e));
                    return 1;
                }
            }
        }
        i += 1;
    }
    if opened == 0 {
        log(b"FAIL: no input streams could be opened; nothing to mix");
        return 1;
    }
    logf(format_args!("opened {}/{} input streams", opened, MIX_INPUTS));

    let dfd = match open(DSP_PATH, OpenFlags::WRITE_ONLY, Permissions::read_write()) {
        Ok(fd) => fd,
        Err(e) => {
            logf(format_args!("FAIL: open({}) failed: {:?}", DSP_PATH, e));
            return 1;
        }
    };
    logf(format_args!("opened out={}(fd={})", DSP_PATH, dfd));

    // ---- 步骤 2：混音循环 ----
    //
    // 每轮：从每路读一段 s16 PCM -> 逐路转 f32 -> 相加 + 固定增益 + 钳位
    // -> 转回 s16 -> 写 dsp。**帧对齐**：所有缓冲按 FRAME_BYTES 的整数倍处理，
    // 使一条流的样本不会与另一条流错位半帧（那会造成左右声道互换）。
    // M3：每路的连接状态与欠载统计（语义与判定规则见 audiod::MixerState）。
    //
    // **为何在用户态计数而非内核 ring**：内核 ring 只知道"此刻为空"，
    // 不知道"本该有数据"。而 underrun 的定义是"生产者承诺供数却没给到"——
    // 这个判断需要"该路是否仍在连接"的知识，而那只有混音器有。
    // 内核侧的 `is_attached()` 门槛对 stream/N 恒为 false（输入端不 attach），
    // 故 ring 的 underruns 对输入端**永远为 0**，不可用于此目的。
    let mut state = audiod::MixerState::new(MIX_INPUTS);

    // M4：每路的音量与斜坡。
    //
    // **软件音量 vs 硬件 AMP 音量（必须区分，计划明确要求）**：
    //   - **软件音量**（此处）：在**用户态**对 PCM 样本做乘法。优点是完全通用、
    //     与具体 codec 无关、多路可各自不同；代价是低音量时**损失有效位深**
    //     （16 位乘 0.1 只剩约 13 位有效精度）。
    //   - **硬件 AMP 音量**（批次六 R3）：经 codec 的 AMP 增益寄存器调，在**模拟**
    //     域衰减，不损失数字精度；但每路能否独立、范围多大，取决于具体 codec
    //     的 AMP_CAP 位图（R1 已读但本批次不做路由/AMP 写）。
    //
    // 本批次只做软件音量：它不依赖 codec 能力，可在 QEMU 上完整验证；
    // 硬件 AMP 需要真实 codec 能力且属 R3 范围。二者**不互相替代**——
    // 软件音量用于混音级控制，硬件 AMP 用于最终模拟输出的增益。
    //
    // 初始为 1.0（不衰减）：默认改变用户听到的东西是错的，音量应由使用者设定。
    // `Ramp::new` 不是 const fn（它做的是普通构造，无 const 需求），
    // 故用运行时循环填数组，而不是 `[const { ... }; N]`。
    let mut ramps: [audiod::Ramp; MIX_INPUTS] = core::array::from_fn(|_| audiod::Ramp::new(1.0));

    // M6：每路的供数趋势观测器（第一版**只观测不调节**，见文件头说明）。
    let mut trends: [audiod::WatermarkTrend; MIX_INPUTS] =
        core::array::from_fn(|_| audiod::WatermarkTrend::new());

    let mut raw: [[u8; MIX_READ_BYTES]; MIX_INPUTS] = [[0u8; MIX_READ_BYTES]; MIX_INPUTS];
    let mut chans: [alloc::vec::Vec<f32>; MIX_INPUTS] =
        [const { alloc::vec::Vec::new() }; MIX_INPUTS];
    // M5：重采样所需的工作缓冲，全部复用以免每轮分配。
    //
    // 左右必须**分平面**处理：交错序列里相邻两点是 L 和 R，直接插值会
    // 在左右之间取平均，把立体声糊成一个声道。拆平面是正确性要求，
    // 不是优化（见 StereoResampler 的说明）。
    //
    // 复用是必要的：每轮 1024 帧，若每轮重新分配，分配器压力会随
    // 运行时间线性累积成碎片 —— 这是长期运行的守护进程，不是一次性程序。
    let mut plane_l: [alloc::vec::Vec<f32>; MIX_INPUTS] =
        [const { alloc::vec::Vec::new() }; MIX_INPUTS];
    let mut plane_r: [alloc::vec::Vec<f32>; MIX_INPUTS] =
        [const { alloc::vec::Vec::new() }; MIX_INPUTS];
    let mut scratch: [alloc::vec::Vec<f32>; MIX_INPUTS] =
        [const { alloc::vec::Vec::new() }; MIX_INPUTS];
    let mut resample_out: [alloc::vec::Vec<f32>; MIX_INPUTS] =
        [const { alloc::vec::Vec::new() }; MIX_INPUTS];
    let mut out_bytes = alloc::vec::Vec::new();
    // 复用的静音缓冲（避免每轮重新分配；长度在首次使用时确定）。
    let mut silent_buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    // 真正混过音频的轮次（区别于总轮次，后者含大量空转）。M4 用它触发音量变更。
    let mut mixed_rounds: u64 = 0;
    let mut mixed_total: u64 = 0;
    let mut rounds: u64 = 0;
    let mut silent_rounds: u64 = 0;
    loop {
        rounds += 1;

        // 2a. 从每路读取可用字节（**非阻塞**：无数据的路本轮视作静音）。
        // 本轮实际取到数据的路数（决定该轮输出的组成）。
        let mut live_now = 0usize;
        // 混音轮次计数（只统计 live_now > 0 的轮次，见下方 M4 说明）。
        let mut k = 0usize;
        while k < MIX_INPUTS {
            chans[k].clear();
            // M6：本轮从该路成功读到的帧数。**先置 0**，只有真正读到才赋值。
            // 它在 match 之外被趋势读取，所以"没读到"必须表现为 0 而不是跳过采样
            // —— 跳过采样会让趋势冻结（见下方 push 处的说明）。
            let mut src_frames_this_round = 0usize;
            if let Some(inp) = inputs[k].as_mut() {
                match read(inp.fd, &mut raw[k]) {
                    // 读到字节：转 f32。仅取 FRAME_BYTES 的整数倍，余数留到下一轮
                    // ——但 DspNode 的 read 是**消费**语义（读走即 commit），
                    // 余数无法保留。故要求读长本身就帧对齐：内核 ring 读写按
                    // 调用方给的缓冲长度截断，不会返回半帧，只要缓冲长度帧对齐。
                    Ok(n) if n > 0 => {
                        // 有数据 -> 该路确实在连接，且**本轮供数了**。
                        // `note_data` 会清零连续空缺计数，使单轮抖动不计欠载
                        // （根因见 MixerState::note_starved 的说明）。
                        state.set_connected(k, true);
                        state.note_data(k);
                        let src_frames = n / FRAME_BYTES;
                        src_frames_this_round = src_frames;
                        if src_frames > 0 {
                            // chans[k] 本轮的产出帧数由重采样决定；音量在
                            // 重采样**之后**逐样本施加（见 convert_and_resample）。
                            convert_and_resample(
                                &raw[k][..n],
                                inp.resampler.as_mut(),
                                &mut plane_l[k],
                                &mut plane_r[k],
                                &mut scratch[k],
                                &mut resample_out[k],
                                &mut chans[k],
                                &mut ramps[k],
                            );
                            live_now += 1;
                        }
                    }
                    // Ok(0) 或 WouldBlock：本轮该路无数据。
                    //
                    // M3：只有**仍处于连接状态**的路才计 underrun。
                    // 从未连过的路不计（启动瞬间本来就无数据），
                    // 已断连的路也不计（没人再承诺供数，那不是欠载）。
                    // 该判定规则由 audiod::MixerState 的宿主测试钉死。
                    // Ok(0) 与 WouldBlock 在本轮视为同一件事：该路没有数据。
                    // 合并两个分支（clippy 亦指出原写法的 guard 冗余）——
                    // 分开写会让读者以为二者有语义差别，实际没有。
                    Ok(_) | Err(Error::WouldBlock) => {
                        state.note_starved(k);
                    }
                    Err(e) => {
                        logf(format_args!("FAIL: read {} hard error: {:?}", inp.path, e));
                        return 1;
                    }
                }
            }
            // M6：把本轮的供数率归一化后记入趋势。
            //
            // **必须在 match 之外、每轮无条件执行**。初版把它写在读到数据的分支里，
            // 结果是该路一旦停止供数就**再也不采样**，历史被冻结在最后一次良好的
            // 读数上，新旧窗口的均值恒等 -> drift 恒为 0。
            // 实测：一路写满后退出（live 由 2 变 1）却一条 drift 都没报。
            // 那正是最该报警的场景，而检测器对它是**结构性失明**的。
            //
            // 由此还推出一条一般性教训："没有观测到异常"与"观测不到异常"在输出上
            // 完全一样，只有当检测器被证明能对异常报警时，前者才是有效结论。
            //
            // 为何用**供数率**而不是内核 ring 的水位：audiod 在用户态，看不到
            // stream ring 的占用。但水位的变化率恰是 `供数率 - 消耗率`，而消耗率
            // 对所有路相同（每轮上限固定），故供数率相对满速的偏移与水位趋势
            // 是同一个信息。这是**代理量**，不是计划字面上的内核水位，如实登记。
            //
            // 归一化到 0..1 使不同路可比较，漂移阈值也才有一致含义。
            trends[k].push(src_frames_this_round as f32 / MAX_FRAMES_PER_ROUND as f32);
            k += 1;
        }

        // 2b. 全部输入都无数据时如何处理（M3 修正）。
        //
        // **先前实现直接 `continue`（完全不写 dsp），那是错的**：
        // dsp 的 ring 是有限的，停写会让驱动侧 DMA 立刻跑空 ——
        // 那才是真正的"欠载"，而且会把"某路暂时没数据"放大成"整机静默"。
        //
        // 正确做法：**写一段等长静音**，保持输出时钟连续。
        // 这样做的意义不只是"听起来没断"，更是**时间正确性**：
        // 输出流的时间轴必须连续，否则一旦重新有数据，音画/节拍会漂移。
        //
        // 静音长度取本轮**本该**产出的帧数。因为各路都没数据，无可依据，
        // 故用固定的 SILENCE_ROUND_FRAMES（与一轮的常规读取量对应）。
        // 这不是编造数据：它如实表示"这段时间没有音频"。
        if state.connected_count() == 0 && silent_rounds > SILENCE_GRACE_ROUNDS {
            // 一台完全没有连接任何生产者的混音器不该无限空转刷日志，
            // 但也**不能退出**：生产者随时可能接入。故降频空转。
            silent_rounds += 1;
            let _ = yield_now();
            continue;
        }

        // 2b. **全部输入都空**时，输出一段静音，而不是跳过。
        //
        // 这保证 dsp 不断流（ring 有限，停写会让 DMA 跑空），
        // 更重要的是保证**输出时间轴连续**：若此时直接跳过，等数据恢复时
        // 声音会相对真实时间提前，长时间运行即产生可闻漂移。
        //
        // 静音是真实信息（"这段时间没有音频"），不是伪造数据。
        if state.connected_count() == 0 {
            silent_rounds += 1;
            // 静音帧数 = 一轮常规读取量，使时间轴与正常轮次等长。
            silent_buf.clear();
            silent_buf.resize(SILENCE_ROUND_FRAMES * FRAME_BYTES, 0u8);
            let mut off = 0usize;
            while off < silent_buf.len() {
                match write(dfd, &silent_buf[off..]) {
                    Ok(w) if w > 0 => {
                        off += w;
                        mixed_total += w as u64;
                    }
                    // dsp 满或写 0：让出后重试，绝不丢弃（时间轴必须完整）。
                    Ok(_) => {
                        let _ = yield_now();
                    }
                    // 合并（guard 冗余）：同样是"写不下/写 0 字节 -> 让出后重试"。
                    Err(Error::WouldBlock) => {
                        let _ = yield_now();
                    }
                    Err(e) => {
                        logf(format_args!("FAIL: write({}) during silence: {:?}", DSP_PATH, e));
                        return 1;
                    }
                }
            }
            continue;
        }

        // 2c. 混音。**必须把全部配置路数都传进去**（包括本轮为空的），
        //     并用 `mix_add_configured` 显式声明配置路数。
        //
        // **这是 M2 实现的一处错误，M3 修正**：先前只把"有数据的路"传给
        // `mix_add`，增益于是变成 1/live_count。生产者的写速率天然有抖动，
        // 某路一时没数据（常态！）就会让**其余所有路**的增益跳变 ——
        // 而音量跳变就是可闻的爆音。
        //
        // 正确语义把两个问题**正交分开**：
        //   1. 混音有多大声？ -> 固定 1/配置路数，一次决定，永不改变；
        //   2. 某路此刻没数据？ -> 该路贡献静音，增益不动。
        // 于是"某路掉线"在听感上只表现为少了一个声源，不会引起电平变化。
        let mut refs: alloc::vec::Vec<&[f32]> = alloc::vec::Vec::with_capacity(MIX_INPUTS);
        let mut m = 0usize;
        while m < MIX_INPUTS {
            // 空 Vec 与"显式静音"在数学上等价（都为 0），故直接传入即可。
            refs.push(chans[m].as_slice());
            m += 1;
        }
        let mixed = audiod::mix_add_configured(refs.as_slice(), MIX_INPUTS);

        // 2d. 转回 s16 字节流（小端，交错立体声）。
        out_bytes.clear();
        out_bytes.reserve(mixed.len() * 2);
        let mut s = 0usize;
        while s < mixed.len() {
            let v = audiod::f32_to_s16(mixed[s]);
            out_bytes.push((v & 0xff) as u8);
            out_bytes.push(((v >> 8) & 0xff) as u8);
            s += 1;
        }

        // 2e. 写 dsp。**必须处理部分写入**，剩余留在 out_bytes 里重试，绝不丢弃。
        let mut off = 0usize;
        while off < out_bytes.len() {
            match write(dfd, &out_bytes[off..]) {
                Ok(w) if w > 0 => {
                    off += w;
                    mixed_total += w as u64;
                }
                Ok(_) => {
                    // 写 0 字节：dsp ring 满，等消费者取走再继续。
                    let _ = yield_now();
                }
                // 合并（guard 冗余）：dsp ring 满 -> 等消费者取走后重试。
                Err(Error::WouldBlock) => {
                    let _ = yield_now();
                }
                Err(e) => {
                    logf(format_args!("FAIL: write({}) hard error: {:?}", DSP_PATH, e));
                    return 1;
                }
            }
        }

        // 2f. M4 音量斜坡演示：在第 DEMO_VOLUME_ROUND 轮改变 stream/0 的音量。
        //
        // **为何由 audiod 自己触发而非外部命令**：计划设想的接口是
        // `stream/N/volume` 属性，但那是**内核节点**，其内容由内核生成，
        // 用户态无法写入；要让外部调节生效，需要一个跨进程的"属性写入 ->
        // 混音器读取"机制（新 syscall 或新节点类型）。那超出本批次范围，
        // 且属"接口"问题而非"混音"问题。
        //
        // 本批次要证明的命题是「音量改变经斜坡平滑过渡、不产生阶跃」——
        // 那与"谁触发"无关，只要变化真实发生在**运行中的真实链路**上即可。
        // 故此处自触发一次，并由 intel-hda 的稳态采样观测幅度变化。
        //
        // 外部可调接口**未实现**，已在 unittodo9 如实登记为偏离，不声称完成。
        // **只在"本轮确实混了音频"时推进这个计数器**（M4 修正）。
        //
        // 先前用总轮次 `rounds` 触发，但那个计数包含大量**空转**：
        // 实测 rounds 已到 14144，而音频早在 ~192 轮就放完（live=0）。
        // 于是音量变更发生在**完全没有音频的时刻**，端到端观测什么也证明不了。
        // 用一个与音频进度无关的时钟去触发音频事件，是错的。
        if live_now > 0 {
            mixed_rounds += 1;
        }
        if mixed_rounds == DEMO_VOLUME_ROUND {
            // 目标 0.5，步长用派生值：完整的 0->1 变化恰好耗时 RAMP_SECONDS；
            // 从 1.0 到 0.5 是半个行程，故约 2.5ms 完成，步长本身不变。
            ramps[0].set_target(0.5, audiod::auto_ramp_step());
            logf(format_args!(
                "M4: stream/0 volume -> 0.5 (ramping, step={:.6})",
                audiod::auto_ramp_step()
            ));
        }

        // 2g. 周期性如实汇报（数字取自真实计数）。
        //
        // **M3 的 underrun 披露**：计划原文要求 `stream/N/status` 暴露 underrun，
        // 但那是**内核节点**，其内容由内核属性回调生成，读不到用户态计数。
        // 要让内核暴露它，唯一途径是新增一条"用户态上报"机制——那是内核改动，
        // 且引入未经验证的新表面。本批次选择**不假装满足**该条，改为：
        //   - 在此如实打印每路的 underrun 计数（真实值，非估算）；
        //   - 并在 unittodo9 中把该偏离明确登记（不列入"已完成"）。
        // 这样"多路欠载可观测"这一**意图**是达成的，只是披露渠道是日志而非
        // 计划指定的那个文件。
        if rounds.is_multiple_of(PROGRESS_EVERY) {
            // 同时打印**当前各路的音量**与**本轮实际有数据的路数**。
            //
            // **为何必须一起打**：M4 的端到端观测要判断某个采样幅度
            // 是"音量生效"还是"某路本轮无数据"造成的。两者会给出不同的
            // 预期值，但仅看幅度无法区分。把音量与存活路数与轮次**同时**
            // 记录，才能把 intel-hda 的采样对回确定的混音条件。
            //
            // `live_now` 是本轮实际取到数据的路数（不是累计连接数）——
            // 它才是决定该轮输出组成的量。
            logf(format_args!(
                "mixed {} bytes (rounds={} silent={} connected={} live={} vol0={} vol1={})",
                mixed_total,
                rounds,
                silent_rounds,
                state.connected_count(),
                live_now,
                ramps[0].current(),
                ramps[1].current()
            ));
            // M6：逐路报告供数趋势。
            //
            // **第一版只观测不调节**（计划 §9.4 第 1 级）。理由：在拿到实测数据
            // 之前就写自适应控制律，等于凭空发明一个控制器 —— 若真实系统只有
            // 抖动而无漂移，调节反而把抖动变成可闻的音调摆动，比不调节更糟。
            // 故先如实披露，用数据回答"要不要调、往哪调、调多少"。
            //
            // 只列出**非 stable** 的路：stable 是常态，全列会淹没有意义的信息；
            // 未列出的路趋势即为 stable，这一点由上面一行说明。
            let mut k = 0usize;
            while k < MIX_INPUTS {
                let dir = trends[k].direction();
                if dir != audiod::DriftDirection::Stable {
                    logf(format_args!(
                        "  stream/{}: drift={} (delta={:.6}, samples={})",
                        k,
                        dir.as_str(),
                        trends[k].drift(),
                        trends[k].samples_seen()
                    ));
                }
                k += 1;
            }
            // 逐路明细：只列出**有欠载**的路，避免刷屏。
            // 未列出的路欠载为 0，这一点由上面的 connected 汇总结说明。
            let mut k = 0usize;
            while k < MIX_INPUTS {
                let st = state.stats(k);
                if st.underruns > 0 {
                    logf(format_args!(
                        "  stream/{}: connected={} underruns={}",
                        k, st.connected, st.underruns
                    ));
                }
                k += 1;
            }
        }
    }
}
