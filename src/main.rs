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

/// 混音器输入端路径（M1 只用第 0 路）。
///
/// 内核挂 `stream/0..AUDIO_STREAM_COUNT-1`，本进程当前只取 0。M2 会改为遍历
/// 全部路数——那时这个常量升级为"要混的音轨集合"。
const STREAM0_PATH: &str = "/devices/audio/stream/0";

/// 输出端路径（独占消费者的哑管道）。
const DSP_PATH: &str = "/devices/audio/dsp";

/// 单次转发的缓冲字节数。
///
/// **取值理由**：4096 字节 = 1024 个 s16 立体声帧（约 21ms @48kHz）。选它是因为
/// `dsp` 的 ring 是 64 KiB，4096 能让单次写**通常**放得下，把"dsp 满"变成罕见
/// 而非每轮都发生的路径；同时小到足以让 `stream/0` 有数据时及时被取走。
/// 更大（如 16 KiB）能减少 syscall 次数，但会让 dsp 一次被填满的比例过高，
/// 反而更频繁地触发背压分支。该取舍在 M2 有实测数据后可再评估。
const FORWARD_CHUNK: usize = 4096;

/// 每多少轮打印一次进度。
///
/// **取值理由**：轮询一轮通常只搬 4096 字节，若每轮都打印会在数十秒内刷出
/// 数万行日志，把真正有价值的证据淹没。64 轮约 256 KiB 数据，既能证明
/// "在持续推进"，又不至于刷屏。
const PROGRESS_EVERY: u64 = 64;

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
    log(b"audiod starting (batch-4 M1: stream/0 -> dsp pass-through, no mixing yet)");

    // ---- 步骤 1：打开输入端（只读）与输出端（只写）----
    //
    // **本进程刻意不调 `AUDIO_ATTACH`（重要修正，有实测依据）**：
    //
    // `attach` 的语义是"成为该节点的**消费**者"——即读走数据的那一方。
    // 而 audiod 在链路里是 dsp 的**写者**（生产者）：它把混音结果写进 dsp，
    // 由 intel-hda 读走喂硬件。二者角色不同，**本可共存**。
    //
    // 计划原文写"audiod 启动即 AUDIO_ATTACH 到 dsp"，那是**错的**：
    // dsp 的消费者槽是**独占**的（A2 已验收语义），而 intel-hda 早已常驻占着。
    // 若 audiod 也 attach，会拿到 EBUSY —— 实测正是如此：
    //   [audio] pid=5 attached as PCM consumer        (intel-hda)
    //   [audiod] FAIL: AUDIO_ATTACH rejected: EBUSY
    // 而 attach 完全不必要：dsp 的写入门禁只要求"**已有**消费者"
    // （`is_attached()`），并不要求写入方**自己**是消费者。
    //
    // 故正确链路是：生产者 -> stream/0 -> audiod -> [写] dsp -> [读] intel-hda -> 硬件。
    let sfd = match open(STREAM0_PATH, OpenFlags::READ_ONLY, Permissions::readonly()) {
        Ok(fd) => fd,
        Err(e) => {
            logf(format_args!("FAIL: open({}) failed: {:?}", STREAM0_PATH, e));
            return 1;
        }
    };
    let dfd = match open(DSP_PATH, OpenFlags::WRITE_ONLY, Permissions::read_write()) {
        Ok(fd) => fd,
        Err(e) => {
            logf(format_args!("FAIL: open({}) failed: {:?}", DSP_PATH, e));
            return 1;
        }
    };
    logf(format_args!("opened in={}(fd={}) out={}(fd={})", STREAM0_PATH, sfd, DSP_PATH, dfd));

    // ---- 步骤 2：转发循环 ----
    let mut buf = [0u8; FORWARD_CHUNK];
    let mut forwarded_total: u64 = 0;
    let mut idle_rounds: u64 = 0;
    let mut rounds: u64 = 0;
    loop {
        rounds += 1;

        // 读输入端。EAGAIN = 暂时无数据（生产者还没写），属正常空转。
        let n = match read(sfd, &mut buf) {
            Ok(0) => {
                idle_rounds += 1;
                let _ = yield_now();
                continue;
            }
            Ok(n) => n,
            Err(e) if e == Error::WouldBlock => {
                idle_rounds += 1;
                let _ = yield_now();
                continue;
            }
            Err(e) => {
                logf(format_args!("FAIL: read({}) hard error: {:?}", STREAM0_PATH, e));
                return 1;
            }
        };

        // 写输出端。**必须处理部分写入**：write 返回实写量，可能 < n。
        // 剩余字节留在 buf 里继续写，绝不丢弃（S18/S19：部分写入是正常路径）。
        let mut off = 0usize;
        while off < n {
            match write(dfd, &buf[off..n]) {
                Ok(w) if w > 0 => {
                    off += w;
                    forwarded_total += w as u64;
                }
                Ok(_) => {
                    // 写 0 字节：ring 满。让出 CPU 后重试同一段数据。
                    let _ = yield_now();
                }
                Err(e) if e == Error::WouldBlock => {
                    // dsp ring 满 -> 等消费者（intel-hda）取走再继续。
                    let _ = yield_now();
                }
                Err(e) => {
                    logf(format_args!("FAIL: write({}) hard error: {:?}", DSP_PATH, e));
                    return 1;
                }
            }
        }

        // 周期性如实汇报进度（不刷屏；数字取自真实计数，不虚构）。
        if rounds % PROGRESS_EVERY == 0 {
            logf(format_args!(
                "forwarded {} bytes (rounds={} idle={})",
                forwarded_total, rounds, idle_rounds
            ));
        }
    }
}