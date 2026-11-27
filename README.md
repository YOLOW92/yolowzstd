# yolowzstd

[![Crates.io](https://img.shields.io/crates/v/yolowzstd.svg)](https://crates.io/crates/yolowzstd)
[![Docs.rs](https://docs.rs/yolowzstd/badge.svg)](https://docs.rs/yolowzstd)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/YOLOW92/yolowzstd/actions/workflows/ci.yml/badge.svg)](https://github.com/YOLOW92/yolowzstd/actions/workflows/ci.yml)

English | [简体中文](docs/README.zh-CN.md)

**Pure-Rust, hardware-adaptive Zstandard ([RFC 8878](https://datatracker.ietf.org/doc/html/rfc8878)) codec — zero runtime dependencies, `#![forbid(unsafe_code)]`, single-core L2-cache discipline, and built-in zero-dependency multi-core parallelism.**

Most compression libraries either wrap C `libzstd` through FFI/`unsafe`, ship single-threaded decoders that stall on small messages, or hardcode oversized lookup tables that blow out CPU L1/L2 caches under concurrency. **`yolowzstd`** is built from first principles in 100% safe Rust (`std` only):

- **Zero dependencies & `#![forbid(unsafe_code)]`**: auditable supply chain, pure `std`, works without a C toolchain, MSRV `1.89` (`edition = "2024"`).
- **Zero-dependency multi-core (`with_threads(0)`)**: `std::thread::scope` parallel chunked encoding producing valid concatenated Zstd frames **plus** frame-boundary scanning for parallel multi-frame decompression — beating `libzstd` (`ZSTD_c_nbWorkers`) by **2.0×–11.1×** on L3/L9/L19 encoding and **2.2×–2.7×** on multi-frame decoding.
- **Sub-microsecond small-payload throughput**: generational hash tables (`base`/`next_base` epoch counters, zero per-call `memset`) and pooled reusable contexts achieve **2,738 MB/s** decode and **229 MB/s** encode on 64 B payloads (**1.4×** and **3.6×** faster than C `libzstd`).
- **Principled algorithmic & cache-conscious design**: probes local cache sizes and SMT topology (Linux sysfs/cgroup when available, safe input-scale fallback elsewhere) and combines pure-safe-Rust 8-byte SWAR tag-filtered RowHash (`L5..=11`), exact BT4 suffix trees (`L12..=15`), Q3 entropy-guided L2-chunked optimal parsing (`L16..=22`), MDL/KL-optimal FSE normalization, and 11-bit dual-symbol Huffman decoding.

---

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
yolowzstd = "0.1"
```

### Simple one-shot API

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

### Reusable Contexts & Multi-Core Execution

Reuse [`Compressor`] and [`Decompressor`] to eliminate allocation overhead, and enable multi-core compression / multi-frame parallel decompression with `.with_threads(0)` (auto-detect) or an explicit worker count:

```rust
use yolowzstd::{CompressOptions, Compressor, DecompressOptions, Decompressor};

fn main() -> Result<(), yolowzstd::Error> {
    let payload = b"{\"ok\":true,\"codec\":\"zstd\",\"safe\":true}\n".repeat(100_000);

    let mut comp = Compressor::new();
    let copts = CompressOptions::new(3).with_threads(0); // auto multi-core
    let z = comp.compress_with(&payload, &copts);

    let mut dec = Decompressor::new();
    let dopts = DecompressOptions::new().with_threads(0); // auto parallel multi-frame decode
    let out = dec.decompress_with(&z, &dopts, payload.len())?;
    assert_eq!(out, payload);
    Ok(())
}
```

---

## Architecture & Capabilities

- **RFC 8878 Decoder (no dictionary support)**:
  - Multi-frame & skippable-frame streams, Raw / RLE / Compressed blocks (returns `Error::Unsupported` on frames requiring a dictionary)
  - Predefined, RLE, Custom, and Repeat FSE sequence tables with single-register bit cursor
  - 1-stream & 4-stream Huffman literals with tree reuse across blocks and an **11-bit dual-symbol canonical table** (Moffat & Turpin 1997) automatically enabled when `P(L1 + L2 <= 11) >= 42%` (5-lookup-per-load refill)
  - 3-slot repeat offset state machine, single-pass sequence execution with cold-path outlining
  - `as_chunks::<32>()` XXH64 frame content checksum verification and strict logical output-size bounds (`max_out`, no panics; caps decompressed output bytes rather than hard process RSS)
- **Adaptive Multi-Tier Encoder (`level <= 0` through `22`)**:
  - `level <= 0`: Raw / RLE fast path
  - `1..=4`: Generation-stamped L2-sized Fast / DoubleFast hash tables (`src/lz.rs`, `src/compact.rs`) with repeat-offset probing, SPRT-style high-entropy step skipping (Wald 1945), and incompressible Huffman skip at L1
  - `5..=11`: **SWAR tag-filtered RowHash** (`src/row.rs`) — 100% safe-Rust 8-byte zero-carry SWAR parallel tag filtering (SwissTable / Zstd 1.5 RowHash / Warren *Hacker's Delight* §6-1, §7-1), paired with a 128 KiB zero-false-positive exact-tagged 4B near-offset cache, 3B repeat-offset probing, and rate-distortion Lazy/Lazy2 lookahead
  - `12..=15`: Exact **BT4 suffix binary search tree** (`src/chain.rs`) + rate-distortion lazy matching (with wide `usize` fallback for `> 4 GiB` inputs)
  - `16..=22`: **Entropy-guided optimal parser** (`src/opt.rs`) — exact BT4 suffix tree + 1/8-bit Q3 fixed-point online Bayesian/EMA entropy model (Zopfli / Zstd `btultra`), L2-resident chunked DP (`OPT_CHUNK = 4096`) with telescoping literal-length potential and Pareto-dominance single-pass relaxation
  - **Information-theoretic entropy backend (`src/henc.rs`, `src/enc.rs`)**: empirical Shannon entropy raw-skip lower bound, KL-divergence Repeat-tree fast decision, Rissanen MDL adaptive FSE table accuracy, 8.8 fixed-point exact cross-entropy mode selection, convex minimum-KL integer quota normalization (Gross 1956 / Duda 2013 ANS) with RFC 8878 `-1` support, and `O(n)` Kraft-truncated length-limited Huffman coding (Moffat & Katajainen 1995)
  - **Multi-core engine (`src/mt.rs`)**: aligns chunks to 128 KiB blocks, emits standard concatenated frames (100% interoperable with every RFC 8878 decoder), and decodes multi-frame streams with `>= 2` frames having known `Frame_Content_Size` in parallel via atomic work claiming and per-slot `Mutex` output buffers (falling back to single-thread otherwise).

---

## Benchmarks (Silesia Corpus, 211.9 MB / 12 files)

Measured on **Intel Core i7-10700 @ 2.90 GHz** (8 cores / 16 threads, 15 GiB RAM, Ubuntu Linux 7.0, `rustc 1.99.0`, `lto = "fat"`, `codegen-units = 1`), compared against C `libzstd` `1.5.7` (`zstd` crate `0.14.4` with `zstdmt`), Rust wrappers/ports, pure-Rust Zstd crates, and Deflate/LZ4 codecs. Full per-file tables and reproduction commands are in [benchmarks/README.md](benchmarks/README.md).

### 1. Multi-Core Throughput (`auto` / `8t` / `16t`, whole-machine, Silesia 211.9 MB)

Higher is better for speed (`MB/s ↑`) and ratio (`↑`).

| Level | `yolowzstd` auto Encode | `yolowzstd` 8t / 16t Encode | `yolowzstd` Ratio | `c_libzstd` 8t / 16t Encode | `c_libzstd` Ratio | `yolowzstd` Parallel Decode (`auto` / `16t`) | `c_libzstd` Decode (1t) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **L1** | 1,464.0 MB/s | 1,485.8 / **1,566.4 MB/s** | 2.77x | **1,740.8** / 1,381.4 MB/s | **2.89x** | **2,911.8 / 3,054.8 MB/s** | 1,251.8 MB/s |
| **L3** | **766.7 MB/s** | **808.4 / 542.1 MB/s** | 3.08x | 376.2 / 370.8 MB/s | **3.20x** | **2,792.5 / 2,788.0 MB/s** | 1,225.7 MB/s |
| **L9** | **161.8 MB/s** | **174.6 / 158.8 MB/s** | 3.39x | 76.2 / 74.4 MB/s | **3.58x** | **2,657.7 / 2,861.9 MB/s** | 1,202.6 MB/s |
| **L19** | **34.5 MB/s** | **25.5 / 34.2 MB/s** | 3.56x | 3.1 / 3.1 MB/s | **4.01x** | **2,874.3 / 2,986.8 MB/s** | 1,114.6 MB/s |

> `c_libzstd` uses single-frame / streaming `ZSTD_decompress` (1 thread) because RFC 8878 single frames have sequential LZ77 dependencies; `yolowzstd` parallel encoding emits standard concatenated frames, allowing parallel decoding when an input has `>= 2` independent frames that all declare `Frame_Content_Size` (otherwise falling back to single-thread).

### 2. Short Payloads (`dickens` prefixes, single-core pinned `taskset -c 4`, `MB/s ↑`)

| Payload Size | `yolowzstd` Dec | `c_libzstd` Dec | `rust-zstd` Dec | `structured` Dec | `yolowzstd` Enc (L3) | `c_libzstd` Enc (L3) | `rust-zstd` Enc (L3) | `structured` Enc (L3) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **64 B** | **2,737.6** | 2,004.1 | 1,812.1 | 13.4 | **228.7** | 62.8 | 52.1 | 30.0 |
| **1 KiB** | 422.3 | **594.4** | 530.3 | 148.4 | 105.3 | **173.9** | 150.8 | 140.6 |
| **4 KiB** | 534.8 | **752.0** | 668.2 | 342.4 | 120.5 | **234.3** | 214.1 | 183.4 |
| **64 KiB** | 731.3 | **1,135.4** | 998.1 | 816.2 | 109.1 | **200.4** | 184.4 | 180.4 |

### 3. Single-Core Silesia Decode & Encode (`taskset -c 4`)

| Workload (`MB/s ↑` / Ratio `↑`) | `yolowzstd` | `c_libzstd` | `rust-zstd` | `zstd-pure-rs` | `structured-zstd` | `zstd-rs` | `oxiarc-zstd` | `ruzstd` |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **Decode L3 streams** | **878.0** | 1,328.8 | 1,164.8 | 1,002.0 | 962.8 | 658.4 | 357.4 | 287.6 |
| **Decode L19 streams** | **844.7** | 1,131.5 | 1,002.9 | 917.3 | 798.9 | 600.2 | 337.0 | 292.4 |
| **Encode L1** | 284.2 (`2.86x`) | 472.2 (`2.89x`) | 440.0 (`2.89x`) | 392.9 (`2.89x`) | 454.7 (`2.89x`) | 103.0 (`2.87x`) | 56.0 (`2.71x`) | — |
| **Encode L3** | 190.2 (`3.13x`) | 243.6 (`3.20x`) | 217.0 (`3.20x`) | 182.4 (`3.20x`) | 236.8 (`3.19x`) | 85.8 (`3.18x`) | 44.5 (`2.88x`) | — |
| **Encode L9** | 40.6 (`3.55x`) | 56.7 (`3.59x`) | 57.8 (`3.59x`) | 44.1 (`3.59x`) | 62.1 (`3.57x`) | 15.8 (`3.52x`) | 27.4 (`2.89x`) | — |

### 4. Cross-Codec & Memory Footprint (Single-Core `taskset -c 4`, Silesia)

- **Cross-Codec L3 Encode/Decode (`MB/s ↑` @ Ratio `↑`)**: `yolowzstd` **193.3 / 757.5 @ 3.13x** vs `libdeflater` 92.2 / 1,102.2 @ 3.14x, `zlib-rs` 78.7 / 789.1 @ 3.08x, `lz4_flex` 622.4 / 2,816.9 @ 2.10x (`yolowzstd` encodes **2.1×** faster than `libdeflater` and **2.5×** faster than `zlib-rs` at comparable ratio).
- **Single-Process Peak RSS Delta (`mozilla` L3, `VmHWM` KiB `↓`)**:
  - Decode delta: `rust-zstd` 49,276 KiB, `zstd-pure-rs` 49,320 KiB, `c_libzstd` 49,384 KiB, **`yolowzstd` 49,400 KiB**, `structured-zstd` 49,724 KiB, `ruzstd` 55,824 KiB.
  - Encode delta: `c_libzstd` 17,912 KiB, `rust-zstd` 18,348 KiB, `zstd-pure-rs` 18,368 KiB, `structured-zstd` 19,508 KiB, **`yolowzstd` 20,728 KiB**.

### Honest Tradeoffs

- `yolowzstd` forbids `unsafe` (`#![forbid(unsafe_code)]`), so on single-core long-stream decode C `libzstd` and `unsafe`-heavy / C-translated Rust crates (`rust-zstd`, `zstd-pure-rs`) remain faster per core (`1,002–1,329 MB/s` vs `878.0 MB/s` at L3); `yolowzstd` leads all `#![forbid(unsafe_code)]` stream decoders (`zstd-rs`, `oxiarc-zstd`, `ruzstd`), surpasses `structured-zstd` on L19 decode (`844.7` vs `798.9 MB/s`) and `zstd-pure-rs` on L3 encode (`190.2` vs `182.4 MB/s`), leads 64 B encode/decode across every implementation tested, and beats `libzstd` across multi-core L3/L9/L19 encoding and multi-frame decoding.
- Dictionary-compressed frames (`Dictionary_ID != 0`) are not supported yet and return `Error::Unsupported`. `max_out` enforces the logical decoded output byte ceiling (and combined multi-frame budgets), rather than capping process-wide physical RSS.
- Multi-core L19 partitions inputs into independent concatenated frames for parallel decode and throughput (`3.56x` vs `4.01x` for single-frame C `libzstd` at L19) in exchange for **11×** higher multi-core L19 encoding throughput (`34.5` vs `3.1 MB/s`) and **2.6×** faster parallel decoding. At L1, fast-skipping favors throughput (`284.2 MB/s`) with a slight ratio tradeoff (`2.86x` vs `2.89x`).

---

## License

Licensed under the [MIT License](LICENSE).
