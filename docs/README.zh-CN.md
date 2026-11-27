# yolowzstd

[English](../README.md) | 简体中文

**纯 Rust、自适应硬件缓存的 Zstandard ([RFC 8878](https://datatracker.ietf.org/doc/html/rfc8878)) 编解码库 —— 零运行时依赖、`#![forbid(unsafe_code)]`、单核 L2 缓存纪律与零依赖多核并行。**

大多数压缩库要么通过 FFI / `unsafe` 包装 C 版 `libzstd`，要么只提供单线程解码器在小包场景卡在上下文清零上，要么分配超大哈希表在并发时把 L1/L2 缓存踩穿。**`yolowzstd`** 从协议本质与现代 CPU 缓存拓扑出发，完全使用纯安全 Rust（仅 `std`，`edition = "2024"`，MSRV `1.89`）构建：

- **零依赖与 `#![forbid(unsafe_code)]`**：无第三方依赖、无 C 工具链要求，供应链干净可审计。
- **零依赖多核并行（`with_threads(0)`）**：基于标准库 `std::thread::scope` 实现分块并发编码（产出标准合法的串联 Zstd 帧），并配合帧边界线性扫描 + `split_at_mut` 零拷贝直写解锁多帧并行解压——在 Silesia 上 `L3`/`L9`/`L19` 多核编码速度领先 `c_libzstd`（`ZSTD_c_nbWorkers`）**2.3×–11.8×**，多帧并发解压达 **4.07–4.61 GB/s**，领先 **3.3×–4.1×**。
- **亚微秒级小包吞吐**：代际戳记哈希表（无需每次清表）+ 可复用上下文，在 **64 B** 短载荷上单核解码达 **4,063 MB/s**、编码达 **225 MB/s**，分别是 C 版 `libzstd` 的 **1.7×** 与 **3.7×**。
- **数学统计驱动 + 缓存感知硬件自适应**：运行时探测缓存与 SMT 拓扑（Linux sysfs/cgroup，不可用时按输入规模安全退化），融合纯 safe Rust 8 字节 SWAR 行哈希（`L5..=11`）、精确 BT4 后缀树（`L12..=15`）、Q3 定点在线熵引导 L2 驻留最优解析（`L16..=22`）、MDL/最小 KL 散度 FSE 归一化与 12-bit 双符号规范 Huffman 解码。

---

## 快速开始

```toml
[dependencies]
yolowzstd = "0.2"
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

- **RFC 8878 解码器（`src/dec/`，无字典支持）**：
  - 支持多帧、跳过帧、Raw / RLE / Compressed 块（含字典帧明确返回 `Error::Unsupported`）
  - 完整支持 Predefined / RLE / Custom / Repeat FSE 序列表，采用紧凑单基址序列表与去冗余边界检查的批解码热路径（`src/dec/seq.rs`）
  - 支持 1 流 / 4 流 Huffman（`src/huff.rs`）与跨块树复用，引入按需延迟构建的 **12-bit 双符号规范 Huffman 展开表**（Moffat & Turpin 1997，每次 64 位装载 4 查表），按树结构精确 `P(l0 + l1 <= 12)` 收益自动启停
  - 三槽位重复偏移状态机、`as_chunks::<32>()` XXH64 帧尾校验、块上下界 `Frame_Content_Size` 预校验（防伪造 FCS 巨额预分配 DoS）与严格 `max_out` 逻辑输出上限保护
- **自适应多档编码器（`level <= 0` 至 `22`，`src/enc/`）**：
  - `level <= 0`：Raw / RLE 直通快路径
  - `1..=4`：无清零代际 L2 哈希表（Fast / DoubleFast，`src/lz/`），支持重复偏移探测与 SPRT 思想高熵自适应跳步（Wald 1945）
  - `5..=11`：**SWAR 标签过滤行哈希（RowHash，`src/row.rs`）**——纯 safe Rust 8 字节零进位 SWAR 并行标签过滤（SwissTable / Zstd 1.5 RowHash / Warren *Hacker's Delight* §6-1, §7-1）+ 128 KiB 零误报精确带标签 4B 近距表 + 3B 重复偏移 + 率失真 Lazy/Lazy2 前瞻
  - `12..=15`：精确 **BT4 后缀二叉搜索树**（`src/chain.rs`）+ 率失真前瞻（> 4 GiB 自动切宽索引）
  - `16..=22`：**熵引导最优解析器（`src/opt/`）**——精确 BT4 后缀树（`src/opt/bt.rs`）+ 1/8 bit Q3 定点在线贝叶斯平滑/EMA 熵代价模型（`src/opt/stats.rs`，Zopfli / Zstd `btultra`）+ L2 驻留片段 DP（`OPT_CHUNK = 4096`）、望远镜 `LL` 势能函数与帕累托支配单次弛豫
  - **信息论熵编码后端（`src/entropy.rs`、`src/henc.rs`、`src/enc/seq_enc.rs`）**：Shannon 经验熵原文快跳、KL 散度 Repeat 树快判、Rissanen MDL 自适应 FSE 精度、8.8 定点精确交叉熵选表、Gross/Duda 凸最小 KL 散度 KKT 整数配额归一化（含 `-1` 极低频支持）与 Moffat & Katajainen `O(n)` Kraft 截断限长 Huffman 编码
  - **零拷贝多核引擎（`src/mt.rs`）**：编码端按 128 KiB 对齐分块产出标准串联帧；解码端对含 `>= 2` 个已校验 `Frame_Content_Size` 独立帧的输入以 `split_at_mut` 零拷贝并行直写目标切片（否则安全回退单线程）

---

## 基准评测（Silesia 语料库，12 个文件共 211.9 MB）

测试平台：**Intel Core i7-10700 @ 2.90 GHz**（8 核 16 线程，15 GiB 内存，Ubuntu Linux 7.0，`rustc 1.99.0`，`lto = "fat"`，`codegen-units = 1`），详细分文件数据及复现方法见 [benchmarks/README.md](../benchmarks/README.md)。

### 1. 全机多核吞吐（`auto` / `8t` / `16t`，不绑核，Silesia 211.9 MB）

速度（`MB/s ↑`）和压缩率（`↑`）均越高越好。

| 等级 | `yolowzstd` auto 编码 | `yolowzstd` 8t / 16t 编码 | `yolowzstd` 压缩率 | `c_libzstd` 8t / 16t 编码 | `c_libzstd` 压缩率 | `yolowzstd` 并发解码 (`auto` / `16t`) | `c_libzstd` 解码 (1t) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **L1** | 1,692.1 MB/s | 1,532.0 / **1,722.9 MB/s** | 2.77x | **1,758.1** / 1,499.5 MB/s | **2.89x** | **4,614.3 / 4,415.0 MB/s** | 1,288.5 MB/s |
| **L3** | **893.4 MB/s** | **890.8 / 643.5 MB/s** | 3.08x | 385.9 / 390.7 MB/s | **3.20x** | **4,073.0 / 3,879.0 MB/s** | 1,237.0 MB/s |
| **L9** | **183.2 MB/s** | **194.0 / 185.4 MB/s** | 3.39x | 82.6 / 82.6 MB/s | **3.58x** | **4,321.1 / 4,461.2 MB/s** | 1,228.4 MB/s |
| **L19** | **37.6 MB/s** | **26.9 / 37.5 MB/s** | 3.56x | 3.2 / 3.2 MB/s | **4.01x** | **4,363.9 / 4,539.2 MB/s** | 1,117.9 MB/s |

> 说明：单条连续 Zstd 帧内部存在 LZ77 顺序回溯依赖，官方 `libzstd` 解码默认单核运行；`yolowzstd` 多核压缩写出完全兼容规范的串联多帧，当输入包含 ≥ 2 个独立帧且均声明了通过块边界预校验的 `Frame_Content_Size` 时，使用 `split_at_mut` 零拷贝并发直写解压（否则安全回退单线程）。

### 2. 短载荷延迟吞吐（`dickens` 前缀，单核 `taskset -c 4`，`MB/s ↑`）

| 载荷大小 | `yolowzstd` 解码 | `c_libzstd` 解码 | `rust-zstd` 解码 | `structured` 解码 | `yolowzstd` L3 编码 | `c_libzstd` L3 编码 | `rust-zstd` L3 编码 | `structured` L3 编码 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **64 B** | **4,062.6** | 2,336.3 | 1,856.8 | 13.7 | **225.1** | 61.4 | 51.2 | 29.4 |
| **1 KiB** | 414.7 | **583.0** | 524.6 | 144.8 | 103.6 | **168.6** | 148.1 | 137.9 |
| **4 KiB** | 529.2 | **740.5** | 656.5 | 337.2 | 120.0 | **223.6** | 207.7 | 172.9 |
| **64 KiB** | 681.7 | **1,061.2** | 915.2 | 752.9 | 106.0 | **195.9** | 182.8 | 177.0 |

### 3. 单核绑核 Silesia 全集对比（`taskset -c 4`）

| 评测项（`MB/s ↑` / 压缩率 `↑`） | `yolowzstd` | `c_libzstd` | `rust-zstd` | `zstd-pure-rs` | `structured-zstd` | `zstd-rs` | `oxiarc-zstd` | `ruzstd` |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **解码 L3 流** | **906.8** | 1,328.4 | 1,171.3 | 1,013.5 | 972.0 | 654.9 | 355.8 | 286.0 |
| **解码 L19 流** | **842.6** | 1,040.7 | 931.5 | 932.9 | 830.6 | 566.5 | 344.6 | 284.8 |
| **编码 L1** | 284.0 (`2.86x`) | 480.0 (`2.89x`) | 438.9 (`2.89x`) | 398.1 (`2.89x`) | 458.6 (`2.89x`) | 104.0 (`2.87x`) | 55.1 (`2.71x`) | — |
| **编码 L3** | 209.4 (`3.13x`) | 251.2 (`3.20x`) | 234.2 (`3.20x`) | 193.6 (`3.20x`) | 238.3 (`3.19x`) | 86.6 (`3.18x`) | 45.5 (`2.88x`) | — |
| **编码 L9** | 41.8 (`3.55x`) | 60.2 (`3.59x`) | 60.3 (`3.59x`) | 47.2 (`3.59x`) | 62.9 (`3.57x`) | 17.3 (`3.52x`) | 29.1 (`2.89x`) | — |

### 4. 跨编解码器横向 & 峰值内存（单核 `taskset -c 4`）

- **跨编解码器 L3（`MB/s ↑` @ 压缩率 `↑`）**：`yolowzstd` **207.6 / 836.6 @ 3.13x** 对比 `libdeflater` 100.0 / 1,210.5 @ 3.14x、`zlib-rs` 81.8 / 841.1 @ 3.08x、`lz4_flex` 650.2 / 3,227.3 @ 2.10x（压缩率相当条件下，编码快于 `libdeflater` **2.1×**、快于 `zlib-rs` **2.5×**）。
- **单进程峰值 RSS 增量（`mozilla` L3，`VmHWM` KiB `↓`）**：
  - 解码增量：`rust-zstd` 49,276、`zstd-pure-rs` 49,312、**`yolowzstd` 49,340**、`c_libzstd` 49,372、`structured-zstd` 49,756、`ruzstd` 55,828 KiB。
  - 编码增量：`c_libzstd` 17,972、`zstd-pure-rs` 18,364、`rust-zstd` 18,380、`structured-zstd` 19,428、**`yolowzstd` 20,676** KiB。

### 客观边界说明

- 由于严守 `#![forbid(unsafe_code)]`，单核长流解码与 `c_libzstd` 及包含大量 `unsafe`/C 直译逻辑的 Rust 移植实现仍有绝对单核速度差距（L3 `906.8` vs `1,014–1,328 MB/s`），但显著领先同为无 `unsafe` 流的 `zstd-rs` / `oxiarc-zstd` / `ruzstd`，并在 L19 单核解码上反超 `structured-zstd`（`842.6` vs `830.6 MB/s`，达 `c_libzstd` 的 **81.0%**）、在 L3 单核编码上反超 `zstd-pure-rs`（`209.4` vs `193.6 MB/s`），同时在 64 B 短载荷（**4.06 GB/s** 解码）与多核 L3/L9/L19 编解码（并发解码 **4.07–4.61 GB/s**）上大幅领先。
- 当前解码器完整实现无字典 RFC 8878 核心规范（检测到 `Dictionary_ID` 将明确返回 `Error::Unsupported`）；`max_out` 严格限制解压后的逻辑输出字节总数（及并行各帧预算之和），而非进程级绝对物理 RSS 硬上限。
- 多核 L19 为支持串联多帧并发解码采用独立分块窗口（`3.56x` vs 单帧 `c_libzstd` `4.01x`），换取了 **11.8×** 更高的多核 L19 编码吞吐（`37.6` vs `3.2 MB/s`）与 **4.1×** 并发解码速度；L1 档位偏向吞吐优化（`284.0 MB/s`），压缩率略低于 `c_libzstd`（`2.86x` vs `2.89x`）。

---

## 许可证

采用 [MIT 许可证](../LICENSE)。
