# yolowzstd

[![Crates.io](https://img.shields.io/crates/v/yolowzstd.svg)](https://crates.io/crates/yolowzstd)
[![Docs.rs](https://docs.rs/yolowzstd/badge.svg)](https://docs.rs/yolowzstd)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/YOLOW92/yolowzstd/actions/workflows/ci.yml/badge.svg)](https://github.com/YOLOW92/yolowzstd/actions/workflows/ci.yml)

English | [简体中文](docs/README.zh-CN.md)

**Pure-Rust, hardware-adaptive Zstandard ([RFC 8878](https://datatracker.ietf.org/doc/html/rfc8878)) codec — zero runtime dependencies, `#![forbid(unsafe_code)]`, single-core L2-cache discipline, and built-in zero-dependency multi-core parallelism.**

Most compression libraries either wrap C `libzstd` through FFI/`unsafe`, ship single-threaded decoders that stall on small messages, or hardcode oversized lookup tables that blow out CPU L1/L2 caches under concurrency. **`yolowzstd`** is built from first principles in 100% safe Rust (`std` only):

- **Zero dependencies & `#![forbid(unsafe_code)]`**: auditable supply chain, pure `std`, works without a C toolchain, MSRV `1.89` (`edition = "2024"`).
- **Zero-dependency multi-core (`with_threads(0)`)**: `std::thread::scope` parallel chunked encoding producing valid concatenated Zstd frames **plus** zero-copy `split_at_mut` parallel multi-frame decompression — beating `libzstd` (`ZSTD_c_nbWorkers`) by **2.3×–11.8×** on L3/L9/L19 encoding and **3.3×–4.1×** on multi-frame decoding (**4.07–4.61 GB/s**).
- **Sub-microsecond small-payload throughput**: generational hash tables (`base`/`next_base` epoch counters, zero per-call `memset`) and pooled reusable contexts achieve **4,063 MB/s** decode and **225 MB/s** encode on 64 B payloads (**1.7×** and **3.7×** faster than C `libzstd`).
- **Principled algorithmic & cache-conscious design**: probes local cache sizes and SMT topology (Linux sysfs/cgroup when available, safe input-scale fallback elsewhere) and combines pure-safe-Rust 8-byte SWAR tag-filtered RowHash (`L5..=11`), exact BT4 suffix trees (`L12..=15`), Q3 entropy-guided L2-chunked optimal parsing (`L16..=22`), MDL/KL-optimal FSE normalization, and 12-bit dual-symbol Huffman decoding.

---

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
yolowzstd = "0.2"
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

- **RFC 8878 Decoder (`src/dec/`, no dictionary support)**:
  - Multi-frame & skippable-frame streams, Raw / RLE / Compressed blocks (returns `Error::Unsupported` on frames requiring a dictionary)
  - Predefined, RLE, Custom, and Repeat FSE sequence tables with compact single-base tables and bounds-elided batch sequence execution (`src/dec/seq.rs`)
  - 1-stream & 4-stream Huffman literals (`src/huff.rs`) with tree reuse across blocks and a lazily built **12-bit dual-symbol canonical table** (Moffat & Turpin 1997, 4 lookups per 64-bit load) automatically gated by exact `P(l0 + l1 <= 12)` tree profitability
  - 3-slot repeat offset state machine, single-pass sequence execution with cold-path outlining
  - `as_chunks::<32>()` XXH64 frame content checksum verification, block-bound `Frame_Content_Size` pre-validation (blocking forged-FCS allocation DoS), and strict logical output-size bounds (`max_out`, no panics; caps decompressed output bytes rather than hard process RSS)
- **Adaptive Multi-Tier Encoder (`level <= 0` through `22`, `src/enc/`)**:
  - `level <= 0`: Raw / RLE fast path
  - `1..=4`: Generation-stamped L2-sized Fast / DoubleFast hash tables (`src/lz/`) with repeat-offset probing, SPRT-style high-entropy step skipping (Wald 1945), and incompressible Huffman skip at L1
  - `5..=11`: **SWAR tag-filtered RowHash** (`src/row.rs`) — 100% safe-Rust 8-byte zero-carry SWAR parallel tag filtering (SwissTable / Zstd 1.5 RowHash / Warren *Hacker's Delight* §6-1, §7-1), paired with a 128 KiB zero-false-positive exact-tagged 4B near-offset cache, 3B repeat-offset probing, and rate-distortion Lazy/Lazy2 lookahead
  - `12..=15`: Exact **BT4 suffix binary search tree** (`src/chain.rs`) + rate-distortion lazy matching (with wide `usize` fallback for `> 4 GiB` inputs)
  - `16..=22`: **Entropy-guided optimal parser** (`src/opt/`) — exact BT4 suffix tree (`src/opt/bt.rs`) + 1/8-bit Q3 fixed-point online Bayesian/EMA entropy model (`src/opt/stats.rs`, Zopfli / Zstd `btultra`), L2-resident chunked DP (`OPT_CHUNK = 4096`) with telescoping literal-length potential and Pareto-dominance single-pass relaxation
  - **Information-theoretic entropy backend (`src/entropy.rs`, `src/henc.rs`, `src/enc/seq_enc.rs`)**: empirical Shannon entropy raw-skip lower bound, KL-divergence Repeat-tree fast decision, Rissanen MDL adaptive FSE table accuracy, 8.8 fixed-point exact cross-entropy mode selection, convex minimum-KL KKT integer quota normalization (Gross 1956 / Duda 2013 ANS) with RFC 8878 `-1` support, and `O(n)` Kraft-truncated length-limited Huffman coding (Moffat & Katajainen 1995)
  - **Multi-core engine (`src/mt.rs`)**: aligns chunks to 128 KiB blocks, emits standard concatenated frames (100% interoperable with every RFC 8878 decoder), and decodes multi-frame streams with `>= 2` frames having validated `Frame_Content_Size` in parallel via **zero-copy `split_at_mut`** directly into the output slice (falling back to single-thread otherwise).

---

## Benchmarks (Silesia Corpus, 211.9 MB / 12 files)

Measured on **Intel Core i7-10700 @ 2.90 GHz** (8 cores / 16 threads, 15 GiB RAM, Ubuntu Linux 7.0, `rustc 1.99.0`, `lto = "fat"`, `codegen-units = 1`), compared against C `libzstd` `1.5.7` (`zstd` crate `0.14.4` with `zstdmt`), Rust wrappers/ports, pure-Rust Zstd crates, and Deflate/LZ4 codecs. Full per-file tables and reproduction commands are in [benchmarks/README.md](benchmarks/README.md).

### 1. Multi-Core Throughput (`auto` / `8t` / `16t`, whole-machine, Silesia 211.9 MB)

Higher is better for speed (`MB/s ↑`) and ratio (`↑`).

| Level | `yolowzstd` auto Encode | `yolowzstd` 8t / 16t Encode | `yolowzstd` Ratio | `c_libzstd` 8t / 16t Encode | `c_libzstd` Ratio | `yolowzstd` Parallel Decode (`auto` / `16t`) | `c_libzstd` Decode (1t) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **L1** | 1,692.1 MB/s | 1,532.0 / **1,722.9 MB/s** | 2.77x | **1,758.1** / 1,499.5 MB/s | **2.89x** | **4,614.3 / 4,415.0 MB/s** | 1,288.5 MB/s |
| **L3** | **893.4 MB/s** | **890.8 / 643.5 MB/s** | 3.08x | 385.9 / 390.7 MB/s | **3.20x** | **4,073.0 / 3,879.0 MB/s** | 1,237.0 MB/s |
| **L9** | **183.2 MB/s** | **194.0 / 185.4 MB/s** | 3.39x | 82.6 / 82.6 MB/s | **3.58x** | **4,321.1 / 4,461.2 MB/s** | 1,228.4 MB/s |
| **L19** | **37.6 MB/s** | **26.9 / 37.5 MB/s** | 3.56x | 3.2 / 3.2 MB/s | **4.01x** | **4,363.9 / 4,539.2 MB/s** | 1,117.9 MB/s |

> `c_libzstd` uses single-frame / streaming `ZSTD_decompress` (1 thread) because RFC 8878 single frames have sequential LZ77 dependencies; `yolowzstd` parallel encoding emits standard concatenated frames, allowing zero-copy parallel decoding when an input has `>= 2` independent frames that all declare validated `Frame_Content_Size` (otherwise falling back to single-thread).

### 2. Short Payloads (`dickens` prefixes, single-core pinned `taskset -c 4`, `MB/s ↑`)

| Payload Size | `yolowzstd` Dec | `c_libzstd` Dec | `rust-zstd` Dec | `structured` Dec | `yolowzstd` Enc (L3) | `c_libzstd` Enc (L3) | `rust-zstd` Enc (L3) | `structured` Enc (L3) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **64 B** | **4,062.6** | 2,336.3 | 1,856.8 | 13.7 | **225.1** | 61.4 | 51.2 | 29.4 |
| **1 KiB** | 414.7 | **583.0** | 524.6 | 144.8 | 103.6 | **168.6** | 148.1 | 137.9 |
| **4 KiB** | 529.2 | **740.5** | 656.5 | 337.2 | 120.0 | **223.6** | 207.7 | 172.9 |
| **64 KiB** | 681.7 | **1,061.2** | 915.2 | 752.9 | 106.0 | **195.9** | 182.8 | 177.0 |

### 3. Single-Core Silesia Decode & Encode (`taskset -c 4`)

| Workload (`MB/s ↑` / Ratio `↑`) | `yolowzstd` | `c_libzstd` | `rust-zstd` | `zstd-pure-rs` | `structured-zstd` | `zstd-rs` | `oxiarc-zstd` | `ruzstd` |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **Decode L3 streams** | **906.8** | 1,328.4 | 1,171.3 | 1,013.5 | 972.0 | 654.9 | 355.8 | 286.0 |
| **Decode L19 streams** | **842.6** | 1,040.7 | 931.5 | 932.9 | 830.6 | 566.5 | 344.6 | 284.8 |
| **Encode L1** | 284.0 (`2.86x`) | 480.0 (`2.89x`) | 438.9 (`2.89x`) | 398.1 (`2.89x`) | 458.6 (`2.89x`) | 104.0 (`2.87x`) | 55.1 (`2.71x`) | — |
| **Encode L3** | 209.4 (`3.13x`) | 251.2 (`3.20x`) | 234.2 (`3.20x`) | 193.6 (`3.20x`) | 238.3 (`3.19x`) | 86.6 (`3.18x`) | 45.5 (`2.88x`) | — |
| **Encode L9** | 41.8 (`3.55x`) | 60.2 (`3.59x`) | 60.3 (`3.59x`) | 47.2 (`3.59x`) | 62.9 (`3.57x`) | 17.3 (`3.52x`) | 29.1 (`2.89x`) | — |

### 4. Cross-Codec & Memory Footprint (Single-Core `taskset -c 4`, Silesia)

- **Cross-Codec L3 Encode/Decode (`MB/s ↑` @ Ratio `↑`)**: `yolowzstd` **207.6 / 836.6 @ 3.13x** vs `libdeflater` 100.0 / 1,210.5 @ 3.14x, `zlib-rs` 81.8 / 841.1 @ 3.08x, `lz4_flex` 650.2 / 3,227.3 @ 2.10x (`yolowzstd` encodes **2.1×** faster than `libdeflater` and **2.5×** faster than `zlib-rs` at comparable ratio).
- **Single-Process Peak RSS Delta (`mozilla` L3, `VmHWM` KiB `↓`)**:
  - Decode delta: `rust-zstd` 49,276 KiB, `zstd-pure-rs` 49,312 KiB, **`yolowzstd` 49,340 KiB**, `c_libzstd` 49,372 KiB, `structured-zstd` 49,756 KiB, `ruzstd` 55,828 KiB.
  - Encode delta: `c_libzstd` 17,972 KiB, `zstd-pure-rs` 18,364 KiB, `rust-zstd` 18,380 KiB, `structured-zstd` 19,428 KiB, **`yolowzstd` 20,676 KiB**.

### Honest Tradeoffs

- `yolowzstd` forbids `unsafe` (`#![forbid(unsafe_code)]`), so on single-core long-stream decode C `libzstd` and `unsafe`-heavy / C-translated Rust crates (`rust-zstd`, `zstd-pure-rs`) remain faster per core (`1,014–1,328 MB/s` vs `906.8 MB/s` at L3); `yolowzstd` leads all `#![forbid(unsafe_code)]` stream decoders (`zstd-rs`, `oxiarc-zstd`, `ruzstd`), surpasses `structured-zstd` on L19 decode (`842.6` vs `830.6 MB/s`, reaching **81.0%** of C `libzstd`) and `zstd-pure-rs` on L3 encode (`209.4` vs `193.6 MB/s`), leads 64 B encode/decode across every implementation tested (**4.06 GB/s** decode), and beats `libzstd` across multi-core L3/L9/L19 encoding and multi-frame decoding (**4.07–4.61 GB/s**).
- Dictionary-compressed frames (`Dictionary_ID != 0`) are not supported yet and return `Error::Unsupported`. `max_out` enforces the logical decoded output byte ceiling (and combined multi-frame budgets), rather than capping process-wide physical RSS.
- Multi-core L19 partitions inputs into independent concatenated frames for parallel decode and throughput (`3.56x` vs `4.01x` for single-frame C `libzstd` at L19) in exchange for **11.8×** higher multi-core L19 encoding throughput (`37.6` vs `3.2 MB/s`) and **4.1×** faster parallel decoding. At L1, fast-skipping favors throughput (`284.0 MB/s`) with a slight ratio tradeoff (`2.86x` vs `2.89x`).

---

## License

Licensed under the [MIT License](LICENSE).
