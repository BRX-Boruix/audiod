# audiod

BORUIX 的音频混音守护进程：把多路音频输入混成一路，写入输出设备。

[English](README.en.md)

由系统初始化进程在启动时拉起，之后常驻运行。

## 它做什么

```
生产者(们) → /devices/audio/stream/0..3 → audiod → /devices/audio/dsp → 硬件驱动 → 硬件
```

每个生产者把 PCM 写入自己的音频流节点，本进程把最多 4 路混成一路：

- 某路没有数据时贡献静音，其余路的增益不变——掉一路只少一个声源，不产生爆音
- 输入采样率与输出不一致时按比例转换，各路速率由系统报告，不假定
- 音量变化按约 5 毫秒的斜坡过渡，不产生阶跃
- 各路的连接状态与供数不足次数记入周期性日志

## 失败时的行为

- 输入或输出节点打不开：报错并退出
- 输入暂时无数据：跳过本轮，不是错误
- 输出缓冲已满：让出 CPU 后重试同一段数据，不丢弃
- 其他错误：报错并退出

所有失败路径都以非零码退出，拉起它的进程能观察到混音器已退出。

## 已知限制

- 输出设备同时只接受一个消费者，需要它的程序按启动顺序约定排列
- 混音路数固定为 4，与系统挂载的流节点数一致，未挂载的路跳过
- 没有外部音量调节接口，音量在混音内部设定

## 构建

```bash
cargo build --release
cargo test
```

混音核心是纯函数，测试在宿主机上运行，不需要音频设备。

## 文件结构

```
audiod/
├── Cargo.toml    # 包定义
├── build.rs      # 注入链接脚本
├── linker.ld     # 用户态段布局
└── src/
    ├── lib.rs    # 混音核心：格式转换、求和、钳位、重采样
    └── main.rs   # 混音循环与失败处理
```

## 相关项目

- [`intel-hda`](https://github.com/BRX-Boruix/intel-hda) —— 输出设备的硬件驱动
- [`audiofile`](https://github.com/BRX-Boruix/audiofile) —— 直接写输出设备的 WAV 播放程序
- [`audioe2e`](https://github.com/BRX-Boruix/audioe2e) —— 音频域端到端验收
- [`libsys`](https://github.com/BRX-Boruix/libsys) —— 用户态系统调用封装

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。
