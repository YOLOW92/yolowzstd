# yolowzstd

[English](../README.md) | 简体中文

**纯 Rust、自适应硬件缓存的 Zstandard ([RFC 8878](https://datatracker.ietf.org/doc/html/rfc8878)) 编解码库 —— 零运行时依赖、`#![forbid(unsafe_code)]`、单核 L2 缓存纪律与零依赖多核并行。**

大多数压缩库要么通过 FFI / `unsafe` 包装 C 版 `libzstd`，要么只提供单线程解码器在小包场景卡在上下文清零上，要么分配超大哈希表在并发时把 L1/L2 缓存踩穿。**`yolowzstd`** 从协议本质与现代 CPU 缓存拓扑出发，完全使用纯安全 Rust（仅 `std`，`edition = "2024"`，MSRV `1.89`）构建：

- **零依赖与 `#![forbid(unsafe_code)]`**：无第三方依赖、无 C 工具链要求，供应链干净可审计。
- **零依赖多核并行（`with_threads(0)`）**：基于标准库 `std::thread::scope` 实现分块并发编码（产出标准合法的串联 Zstd 帧），并配合零解压开销的帧边界线性扫描解锁多帧并行解压——在 Silesia 上 `L3`/`L9`/`L19` 多核编码速度领先 `c_libzstd`（`ZSTD_c_nbWorkers`）**1.6×–9.8×**，多帧并发解压领先 **2.4×–2.8×**。
- **亚微秒级小包吞吐**：代际戳记哈希表（无需每次清表）+ 可复用上下文，在 **64 B** 短载荷上单核解码达 **3,257 MB/s**、编码达 **264 MB/s**，分别是 C 版 `libzstd` 的 **1.6×** 与 **4.1×**。
- **缓存感知硬件自适应**：运行时读取本地缓存与 SMT 拓扑（Linux sysfs/cgroup，不可用时按输入规模安全退化），在快编档位将活跃表约束在每物理核 `256 KiB` L2 内，同时支持 `u32` 与 `usize` 长历史大窗口。

---

## 快速开始

```toml
[dependencies]
yolowzstd = "0.1"
```

### 基础单帧用法

```rust
use yolowzstd::{compress, decompress};

fn main() -> Result<(), yolowzstd::Error> {
    let data = b"hello yolowzstd! ".repeat(256);
    let compressed = compress(&data, 3);
    let restored = decompress(&compressed, data.len())?;
    assert_eq!(restored, data);
    Ok(())
}
```

### 上下文复用与多核并行

```rust
use yolowzstd::{CompressOptions, Compressor, DecompressOptions, Decompressor};

fn main() -> Result<(), yolowzstd::Error> {
    let payload = b"{\"ok\":true,\"codec\":\"zstd\",\"safe\":true}\n".repeat(100_000);

    let mut comp = Compressor::new();
    let copts = CompressOptions::new(3).with_threads(0); // 自动探测多核并行编码
    let z = comp.compress_with(&payload, &copts);

    let mut dec = Decompressor::new();
    let dopts = DecompressOptions::new().with_threads(0); // 多帧自动并行解码
    let out = dec.decompress_with(&z, &dopts, payload.len())?;
    assert_eq!(out, payload);
    Ok(())
}
```

---

## 基准评测（Silesia 语料库，12 个文件共 211.9 MB）

测试平台：**Intel Core i7-10700 @ 2.90 GHz**（8 核 16 线程，15 GiB 内存，Ubuntu Linux 7.0，`rustc 1.99.0`，`lto = "fat"`，`codegen-units = 1`），详细分文件数据及复现方法见 [benchmarks/README.md](../benchmarks/README.md)。

### 1. 全机多核吞吐（`auto` / `8t` / `16t`，不绑核，Silesia 211.9 MB）

速度（`MB/s ↑`）和压缩率（`↑`）均越高越好。

| 等级 | `yolowzstd` auto 编码 | `yolowzstd` 8t / 16t 编码 | `yolowzstd` 压缩率 | `c_libzstd` 8t / 16t 编码 | `c_libzstd` 压缩率 | `yolowzstd` 并发解码 (`auto` / `16t`) | `c_libzstd` 解码 (1t) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **L1** | **1,528.8 MB/s** | 1,290.2 / **1,450.9 MB/s** | **2.90x** | **1,712.9** / 1,343.0 MB/s | 2.89x | **2,931.4 / 3,019.6 MB/s** | 1,247.4 MB/s |
| **L3** | **912.1 MB/s** | **937.9 / 944.7 MB/s** | 3.09x | 354.2 / 368.4 MB/s | **3.20x** | **3,032.9 / 2,994.4 MB/s** | 1,240.3 MB/s |
| **L9** | **126.4 MB/s** | 103.0 / **130.0 MB/s** | 3.34x | 81.9 / 80.3 MB/s | **3.58x** | **2,981.9 / 3,025.3 MB/s** | 1,167.1 MB/s |
| **L19** | **32.3 MB/s** | 25.3 / **32.6 MB/s** | 3.46x | 3.3 / 3.3 MB/s | **4.01x** | **3,282.7 / 3,360.9 MB/s** | 1,217.7 MB/s |

> 说明：单条连续 Zstd 帧内部存在 LZ77 顺序回溯依赖，官方 `libzstd` 解码默认单核运行；`yolowzstd` 多核压缩写出完全兼容规范的串联多帧，仅当输入包含 ≥ 2 个独立帧且均声明了 `Frame_Content_Size` 时并发解压（通过原子认领帧任务与每槽 `Mutex` 输出缓冲实现，否则安全回退单线程）。

### 2. 短载荷延迟吞吐（`dickens` 前缀，单核 `taskset -c 4`，`MB/s ↑`）

| 载荷大小 | `yolowzstd` 解码 | `c_libzstd` 解码 | `rust-zstd` 解码 | `structured` 解码 | `yolowzstd` L3 编码 | `c_libzstd` L3 编码 | `rust-zstd` L3 编码 | `structured` L3 编码 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **64 B** | **3,257.4** | 2,020.1 | 1,803.1 | 13.7 | **263.8** | 64.1 | 52.8 | 30.3 |
| **1 KiB** | 414.6 | **607.1** | 547.0 | 152.1 | 122.0 | **175.2** | 153.5 | 143.7 |
| **4 KiB** | 491.0 | **779.5** | 693.0 | 358.9 | 126.1 | **232.3** | 217.6 | 179.7 |
| **64 KiB** | 642.2 | **1,130.1** | 985.2 | 805.3 | 95.5 | **204.3** | 191.8 | 187.4 |

### 3. 单核绑核 Silesia 全集对比（`taskset -c 4`）

| 评测项（`MB/s ↑` / 压缩率 `↑`） | `yolowzstd` | `c_libzstd` | `rust-zstd` | `zstd-pure-rs` | `structured-zstd` | `zstd-rs` | `oxiarc-zstd` | `ruzstd` |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **解码 L3 流** | **806.5** | 1,314.9 | 1,179.7 | 1,018.6 | 971.2 | 659.2 | 356.4 | 289.1 |
| **解码 L19 流** | **652.1** | 1,065.1 | 968.9 | 967.7 | 742.6 | 591.6 | 319.3 | 278.3 |
| **编码 L1** | 242.2 (`2.93x`) | 455.7 (`2.89x`) | 431.2 (`2.89x`) | 357.2 (`2.89x`) | 437.8 (`2.89x`) | 94.0 (`2.87x`) | 51.4 (`2.71x`) | — |
| **编码 L3** | 171.1 (`3.12x`) | 249.7 (`3.20x`) | 233.0 (`3.20x`) | 188.4 (`3.20x`) | 228.7 (`3.19x`) | 83.1 (`3.18x`) | 44.4 (`2.88x`) | — |
| **编码 L9** | 17.1 (`3.48x`) | 60.4 (`3.59x`) | 57.1 (`3.59x`) | 46.0 (`3.59x`) | 60.2 (`3.57x`) | 17.4 (`3.52x`) | 27.8 (`2.89x`) | — |

### 4. 跨编解码器横向 & 峰值内存（单核 `taskset -c 4`）

- **跨编解码器 L3（`MB/s ↑` @ 压缩率 `↑`）**：`yolowzstd` **170.7 / 756.4 @ 3.12x** 对比 `libdeflater` 97.4 / 1,204.3 @ 3.14x、`zlib-rs` 85.1 / 848.9 @ 3.08x、`lz4_flex` 651.0 / 3,102.9 @ 2.10x（压缩率相当条件下，编码快于 `libdeflater` **1.75×**、快于 `zlib-rs` **2.0×**）。
- **单进程峰值 RSS 增量（`mozilla` L3，`VmHWM` KiB `↓`）**：
  - 解码增量：`c_libzstd` 49,396、`zstd-pure-rs` 49,396、**`yolowzstd` 49,404**、`rust-zstd` 49,524、`structured-zstd` 49,656、`ruzstd` 55,820 KiB。
  - 编码增量：`c_libzstd` 18,040、`rust-zstd` 18,420、`zstd-pure-rs` 18,420、`structured-zstd` 19,464、**`yolowzstd` 19,700** KiB。

### 客观边界说明

- 由于严守 `#![forbid(unsafe_code)]`，单核长流解码与 `c_libzstd` 及包含大量 `unsafe`/C 直译逻辑的 Rust 移植实现仍有绝对单核速度差距（L3 `806.5` vs `1,019–1,315 MB/s`），但领先同为无 `unsafe` 流的 `zstd-rs` / `oxiarc-zstd` / `ruzstd`，并在 64 B 短载荷与多核 L3/L9/L19 编解码上大幅反超。
- 当前解码器完整实现无字典 RFC 8878 核心规范（检测到 `Dictionary_ID` 将明确返回 `Error::Unsupported`）；`max_out` 严格限制解压后的逻辑输出字节总数（及并行各帧预算之和），而非进程级绝对物理 RSS 硬上限。
- L9/L19 优先控制搜索深度与窗口以换取可用吞吐，其中 L16–22 采用块内近似比特代价前向启发式 DP（非全局精确最优），因此在极高压缩率档位压缩比低于 `c_libzstd`（如 L19 `3.46x` vs `4.01x`），但多核 L19 吞吐达到其近 10 倍。

---

## 许可证

采用 [MIT 许可证](../LICENSE)。
