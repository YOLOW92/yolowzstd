# yolowzstd

[English](../README.md) | 简体中文

**纯 Rust、自适应硬件缓存的 Zstandard ([RFC 8878](https://datatracker.ietf.org/doc/html/rfc8878)) 编解码库 —— 零运行时依赖、`#![forbid(unsafe_code)]`、单核 L2 缓存纪律与零依赖多核并行。**

大多数压缩库要么通过 FFI / `unsafe` 包装 C 版 `libzstd`，要么只提供单线程解码器在小包场景卡在上下文清零上，要么分配超大哈希表在并发时把 L1/L2 缓存踩穿。**`yolowzstd`** 从协议本质与现代 CPU 缓存拓扑出发，完全使用纯安全 Rust（仅 `std`，`edition = "2024"`，MSRV `1.89`）构建：

- **零依赖与 `#![forbid(unsafe_code)]`**：无第三方依赖、无 C 工具链要求，供应链干净可审计。
- **零依赖多核并行（`with_threads(0)`）**：基于标准库 `std::thread::scope` 实现分块并发编码（产出标准合法的串联 Zstd 帧），并配合零解压开销的帧边界线性扫描解锁多帧并行解压——在 Silesia 上 `L3`/`L9`/`L19` 多核编码速度领先 `c_libzstd`（`ZSTD_c_nbWorkers`）**2.0×–11.1×**，多帧并发解压领先 **2.2×–2.7×**。
- **亚微秒级小包吞吐**：代际戳记哈希表（无需每次清表）+ 可复用上下文，在 **64 B** 短载荷上单核解码达 **2,738 MB/s**、编码达 **229 MB/s**，分别是 C 版 `libzstd` 的 **1.4×** 与 **3.6×**。
- **数学统计驱动 + 缓存感知硬件自适应**：运行时探测缓存与 SMT 拓扑（Linux sysfs/cgroup，不可用时按输入规模安全退化），融合纯 safe Rust 8 字节 SWAR 行哈希（`L5..=11`）、精确 BT4 后缀树（`L12..=15`）、Q3 定点在线熵引导 L2 驻留最优解析（`L16..=22`）、MDL/最小 KL 散度 FSE 归一化与 11-bit 双符号规范 Huffman 解码。

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

## 架构与核心算法

- **RFC 8878 解码器（无字典支持）**：
  - 支持多帧、跳过帧、Raw / RLE / Compressed 块（含字典帧明确返回 `Error::Unsupported`）
  - 完整支持 Predefined / RLE / Custom / Repeat FSE 序列表与单寄存器位游标
  - 支持 1 流 / 4 流 Huffman 与跨块树复用，引入 **11-bit 双符号规范 Huffman 展开表**（Moffat & Turpin 1997），当 `P(L1 + L2 <= 11) >= 42%` 时自动启用并配合每次装填 5 查表
  - 三槽位重复偏移状态机、`as_chunks::<32>()` XXH64 帧尾校验与严格 `max_out` 逻辑输出上限保护
- **自适应多档编码器（`level <= 0` 至 `22`）**：
  - `level <= 0`：Raw / RLE 直通快路径
  - `1..=4`：无清零代际 L2 哈希表（Fast / DoubleFast，`src/lz.rs`、`src/compact.rs`），支持重复偏移探测与 SPRT 思想高熵自适应跳步（Wald 1945）
  - `5..=11`：**SWAR 标签过滤行哈希（RowHash，`src/row.rs`）**——纯 safe Rust 8 字节零进位 SWAR 并行标签过滤（SwissTable / Zstd 1.5 RowHash / Warren *Hacker's Delight* §6-1, §7-1）+ 128 KiB 零误报精确带标签 4B 近距表 + 3B 重复偏移 + 率失真 Lazy/Lazy2 前瞻
  - `12..=15`：精确 **BT4 后缀二叉搜索树**（`src/chain.rs`）+ 率失真前瞻（> 4 GiB 自动切宽索引）
  - `16..=22`：**熵引导最优解析器（`src/opt.rs`）**——精确 BT4 后缀树 + 1/8 bit Q3 定点在线贝叶斯平滑/EMA 熵代价模型（Zopfli / Zstd `btultra`）+ L2 驻留片段 DP（`OPT_CHUNK = 4096`）、望远镜 `LL` 势能函数与帕累托支配单次弛豫
  - **信息论熵编码后端（`src/henc.rs`、`src/enc.rs`）**：Shannon 经验熵原文快跳、KL 散度 Repeat 树快判、Rissanen MDL 自适应 FSE 精度、8.8 定点精确交叉熵选表、Gross/Duda 凸最小 KL 散度整数配额归一化（含 `-1` 极低频支持）与 Moffat & Katajainen `O(n)` Kraft 截断限长 Huffman 编码

---

## 基准评测（Silesia 语料库，12 个文件共 211.9 MB）

测试平台：**Intel Core i7-10700 @ 2.90 GHz**（8 核 16 线程，15 GiB 内存，Ubuntu Linux 7.0，`rustc 1.99.0`，`lto = "fat"`，`codegen-units = 1`），详细分文件数据及复现方法见 [benchmarks/README.md](../benchmarks/README.md)。

### 1. 全机多核吞吐（`auto` / `8t` / `16t`，不绑核，Silesia 211.9 MB）

速度（`MB/s ↑`）和压缩率（`↑`）均越高越好。

| 等级 | `yolowzstd` auto 编码 | `yolowzstd` 8t / 16t 编码 | `yolowzstd` 压缩率 | `c_libzstd` 8t / 16t 编码 | `c_libzstd` 压缩率 | `yolowzstd` 并发解码 (`auto` / `16t`) | `c_libzstd` 解码 (1t) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **L1** | 1,464.0 MB/s | 1,485.8 / **1,566.4 MB/s** | 2.77x | **1,740.8** / 1,381.4 MB/s | **2.89x** | **2,911.8 / 3,054.8 MB/s** | 1,251.8 MB/s |
| **L3** | **766.7 MB/s** | **808.4 / 542.1 MB/s** | 3.08x | 376.2 / 370.8 MB/s | **3.20x** | **2,792.5 / 2,788.0 MB/s** | 1,225.7 MB/s |
| **L9** | **161.8 MB/s** | **174.6 / 158.8 MB/s** | 3.39x | 76.2 / 74.4 MB/s | **3.58x** | **2,657.7 / 2,861.9 MB/s** | 1,202.6 MB/s |
| **L19** | **34.5 MB/s** | **25.5 / 34.2 MB/s** | 3.56x | 3.1 / 3.1 MB/s | **4.01x** | **2,874.3 / 2,986.8 MB/s** | 1,114.6 MB/s |

> 说明：单条连续 Zstd 帧内部存在 LZ77 顺序回溯依赖，官方 `libzstd` 解码默认单核运行；`yolowzstd` 多核压缩写出完全兼容规范的串联多帧，仅当输入包含 ≥ 2 个独立帧且均声明了 `Frame_Content_Size` 时并发解压（通过原子认领帧任务与每槽 `Mutex` 输出缓冲实现，否则安全回退单线程）。

### 2. 短载荷延迟吞吐（`dickens` 前缀，单核 `taskset -c 4`，`MB/s ↑`）

| 载荷大小 | `yolowzstd` 解码 | `c_libzstd` 解码 | `rust-zstd` 解码 | `structured` 解码 | `yolowzstd` L3 编码 | `c_libzstd` L3 编码 | `rust-zstd` L3 编码 | `structured` L3 编码 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **64 B** | **2,737.6** | 2,004.1 | 1,812.1 | 13.4 | **228.7** | 62.8 | 52.1 | 30.0 |
| **1 KiB** | 422.3 | **594.4** | 530.3 | 148.4 | 105.3 | **173.9** | 150.8 | 140.6 |
| **4 KiB** | 534.8 | **752.0** | 668.2 | 342.4 | 120.5 | **234.3** | 214.1 | 183.4 |
| **64 KiB** | 731.3 | **1,135.4** | 998.1 | 816.2 | 109.1 | **200.4** | 184.4 | 180.4 |

### 3. 单核绑核 Silesia 全集对比（`taskset -c 4`）

| 评测项（`MB/s ↑` / 压缩率 `↑`） | `yolowzstd` | `c_libzstd` | `rust-zstd` | `zstd-pure-rs` | `structured-zstd` | `zstd-rs` | `oxiarc-zstd` | `ruzstd` |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **解码 L3 流** | **878.0** | 1,328.8 | 1,164.8 | 1,002.0 | 962.8 | 658.4 | 357.4 | 287.6 |
| **解码 L19 流** | **844.7** | 1,131.5 | 1,002.9 | 917.3 | 798.9 | 600.2 | 337.0 | 292.4 |
| **编码 L1** | 284.2 (`2.86x`) | 472.2 (`2.89x`) | 440.0 (`2.89x`) | 392.9 (`2.89x`) | 454.7 (`2.89x`) | 103.0 (`2.87x`) | 56.0 (`2.71x`) | — |
| **编码 L3** | 190.2 (`3.13x`) | 243.6 (`3.20x`) | 217.0 (`3.20x`) | 182.4 (`3.20x`) | 236.8 (`3.19x`) | 85.8 (`3.18x`) | 44.5 (`2.88x`) | — |
| **编码 L9** | 40.6 (`3.55x`) | 56.7 (`3.59x`) | 57.8 (`3.59x`) | 44.1 (`3.59x`) | 62.1 (`3.57x`) | 15.8 (`3.52x`) | 27.4 (`2.89x`) | — |

### 4. 跨编解码器横向 & 峰值内存（单核 `taskset -c 4`）

- **跨编解码器 L3（`MB/s ↑` @ 压缩率 `↑`）**：`yolowzstd` **193.3 / 757.5 @ 3.13x** 对比 `libdeflater` 92.2 / 1,102.2 @ 3.14x、`zlib-rs` 78.7 / 789.1 @ 3.08x、`lz4_flex` 622.4 / 2,816.9 @ 2.10x（压缩率相当条件下，编码快于 `libdeflater` **2.1×**、快于 `zlib-rs` **2.5×**）。
- **单进程峰值 RSS 增量（`mozilla` L3，`VmHWM` KiB `↓`）**：
  - 解码增量：`rust-zstd` 49,276、`zstd-pure-rs` 49,320、`c_libzstd` 49,384、**`yolowzstd` 49,400**、`structured-zstd` 49,724、`ruzstd` 55,824 KiB。
  - 编码增量：`c_libzstd` 17,912、`rust-zstd` 18,348、`zstd-pure-rs` 18,368、`structured-zstd` 19,508、**`yolowzstd` 20,728** KiB。

### 客观边界说明

- 由于严守 `#![forbid(unsafe_code)]`，单核长流解码与 `c_libzstd` 及包含大量 `unsafe`/C 直译逻辑的 Rust 移植实现仍有绝对单核速度差距（L3 `878.0` vs `1,002–1,329 MB/s`），但显著领先同为无 `unsafe` 流的 `zstd-rs` / `oxiarc-zstd` / `ruzstd`，并在 L19 单核解码上反超 `structured-zstd`（`844.7` vs `798.9 MB/s`）、在 L3 单核编码上反超 `zstd-pure-rs`（`190.2` vs `182.4 MB/s`），同时在 64 B 短载荷与多核 L3/L9/L19 编解码上大幅领先。
- 当前解码器完整实现无字典 RFC 8878 核心规范（检测到 `Dictionary_ID` 将明确返回 `Error::Unsupported`）；`max_out` 严格限制解压后的逻辑输出字节总数（及并行各帧预算之和），而非进程级绝对物理 RSS 硬上限。
- 多核 L19 为支持串联多帧并发解码采用独立分块窗口（`3.56x` vs 单帧 `c_libzstd` `4.01x`），换取了 **11×** 更高的多核 L19 编码吞吐（`34.5` vs `3.1 MB/s`）与 **2.6×** 并发解码速度；L1 档位偏向吞吐优化（`284.2 MB/s`），压缩率略低于 `c_libzstd`（`2.86x` vs `2.89x`）。

---

## 许可证

采用 [MIT 许可证](../LICENSE)。
