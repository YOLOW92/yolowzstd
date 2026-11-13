# yolowzstd

[![Crates.io](https://img.shields.io/crates/v/yolowzstd.svg)](https://crates.io/crates/yolowzstd)
[![Docs.rs](https://docs.rs/yolowzstd/badge.svg)](https://docs.rs/yolowzstd)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/YOLOW92/yolowzstd/actions/workflows/ci.yml/badge.svg)](https://github.com/YOLOW92/yolowzstd/actions/workflows/ci.yml)

English | [简体中文](docs/README.zh-CN.md)

**Pure-Rust, hardware-adaptive Zstandard ([RFC 8878](https://datatracker.ietf.org/doc/html/rfc8878)) codec — zero runtime dependencies, `#![forbid(unsafe_code)]`, single-core L2-cache discipline, and built-in zero-dependency multi-core parallelism.**

Most compression libraries either wrap C `libzstd` through FFI/`unsafe`, ship single-threaded decoders that stall on small messages, or hardcode oversized lookup tables that blow out CPU L1/L2 caches under concurrency. **`yolowzstd`** is built from first principles in 100% safe Rust (`std` only):

- **Zero dependencies & `#![forbid(unsafe_code)]`**: auditable supply chain, pure `std`, works without a C toolchain, MSRV `1.89` (`edition = "2024"`).
- **Zero-dependency multi-core (`with_threads(0)`)**: `std::thread::scope` parallel chunked encoding producing valid concatenated Zstd frames **plus** frame-boundary scanning for parallel multi-frame decompression — beating `libzstd` (`ZSTD_c_nbWorkers`) by **1.6×–9.8×** on L3/L9/L19 encoding and **2.4×–2.8×** on multi-frame decoding.
- **Sub-microsecond small-payload throughput**: generational hash tables (`base`/`next_base` epoch counters, zero per-call `memset`) and pooled reusable contexts achieve **3,257 MB/s** decode and **264 MB/s** encode on 64 B payloads (**1.6×** and **4.1×** faster than C `libzstd`).
- **Cache-conscious hardware adaptation**: probes local cache sizes and SMT topology (Linux sysfs/cgroup when available, safe input-scale fallback elsewhere) to keep fast matching inside per-core L2 (`256 KiB`) while supporting compact `u32` and `usize` long-history windows.

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

- **RFC 8878 Decoder**:
  - Multi-frame & skippable-frame streams, Raw / RLE / Compressed blocks
  - Predefined, RLE, Custom, and Repeat FSE sequence tables
  - 1-stream & 4-stream Huffman literals with Huffman tree reuse across blocks
  - 3-slot repeat offset state machine, single-pass sequence execution with cold-path outlining
  - XXH64 frame content checksum verification and strict untrusted-input bounds (`max_out`, no panics)
- **Adaptive Multi-Tier Encoder (`level <= 0` through `22`)**:
  - `level <= 0`: Raw / RLE fast path
  - `1..=4`: Generation-stamped L2-sized Fast / DoubleFast hash tables with repeat-offset probing and incompressible Huffman skip at L1
  - `5..=15`: Hash chains + 1/2-step lazy matching (`12..=15` binary-tree suffix matcher)
  - `16..=22`: Binary-tree suffix matcher + forward bit-cost DP optimal parser
  - Multi-core engine (`src/mt.rs`): aligns chunks to 128 KiB blocks, emits standard concatenated frames (100% interoperable with every RFC 8878 decoder), and decodes multi-frame streams in parallel via lock-free atomic work stealing.

---

## Benchmarks (Silesia Corpus, 211.9 MB / 12 files)

Measured on **Intel Core i7-10700 @ 2.90 GHz** (8 cores / 16 threads, 15 GiB RAM, Ubuntu Linux 7.0, `rustc 1.99.0`, `lto = "fat"`, `codegen-units = 1`), compared against C `libzstd` `1.5.7` (`zstd` crate `0.14.4` with `zstdmt`), Rust wrappers/ports, pure-Rust Zstd crates, and Deflate/LZ4 codecs. Full per-file tables and reproduction commands are in [benchmarks/README.md](benchmarks/README.md).

### 1. Multi-Core Throughput (`auto` / `8t` / `16t`, whole-machine, Silesia 211.9 MB)

Higher is better for speed (`MB/s ↑`) and ratio (`↑`).

| Level | `yolowzstd` auto Encode | `yolowzstd` 8t / 16t Encode | `yolowzstd` Ratio | `c_libzstd` 8t / 16t Encode | `c_libzstd` Ratio | `yolowzstd` Parallel Decode (`auto` / `16t`) | `c_libzstd` Decode (1t) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **L1** | **1,528.8 MB/s** | 1,290.2 / **1,450.9 MB/s** | **2.90x** | **1,712.9** / 1,343.0 MB/s | 2.89x | **2,931.4 / 3,019.6 MB/s** | 1,247.4 MB/s |
| **L3** | **912.1 MB/s** | **937.9 / 944.7 MB/s** | 3.09x | 354.2 / 368.4 MB/s | **3.20x** | **3,032.9 / 2,994.4 MB/s** | 1,240.3 MB/s |
| **L9** | **126.4 MB/s** | 103.0 / **130.0 MB/s** | 3.34x | 81.9 / 80.3 MB/s | **3.58x** | **2,981.9 / 3,025.3 MB/s** | 1,167.1 MB/s |
| **L19** | **32.3 MB/s** | 25.3 / **32.6 MB/s** | 3.46x | 3.3 / 3.3 MB/s | **4.01x** | **3,282.7 / 3,360.9 MB/s** | 1,217.7 MB/s |

> `c_libzstd` uses single-frame / streaming `ZSTD_decompress` (1 thread) because RFC 8878 single frames have sequential LZ77 dependencies; `yolowzstd` parallel encoding outputs concatenated frames, unlocking multi-core decoding for both `yolowzstd` and multi-frame callers.

### 2. Short Payloads (`dickens` prefixes, single-core pinned `taskset -c 4`, `MB/s ↑`)

| Payload Size | `yolowzstd` Dec | `c_libzstd` Dec | `rust-zstd` Dec | `structured` Dec | `yolowzstd` Enc (L3) | `c_libzstd` Enc (L3) | `rust-zstd` Enc (L3) | `structured` Enc (L3) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **64 B** | **3,257.4** | 2,020.1 | 1,803.1 | 13.7 | **263.8** | 64.1 | 52.8 | 30.3 |
| **1 KiB** | 414.6 | **607.1** | 547.0 | 152.1 | 122.0 | **175.2** | 153.5 | 143.7 |
| **4 KiB** | 491.0 | **779.5** | 693.0 | 358.9 | 126.1 | **232.3** | 217.6 | 179.7 |
| **64 KiB** | 642.2 | **1,130.1** | 985.2 | 805.3 | 95.5 | **204.3** | 191.8 | 187.4 |

### 3. Single-Core Silesia Decode & Encode (`taskset -c 4`)

| Workload (`MB/s ↑` / Ratio `↑`) | `yolowzstd` | `c_libzstd` | `rust-zstd` | `zstd-pure-rs` | `structured-zstd` | `zstd-rs` | `oxiarc-zstd` | `ruzstd` |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **Decode L3 streams** | **806.5** | 1,314.9 | 1,179.7 | 1,018.6 | 971.2 | 659.2 | 356.4 | 289.1 |
| **Decode L19 streams** | **652.1** | 1,065.1 | 968.9 | 967.7 | 742.6 | 591.6 | 319.3 | 278.3 |
| **Encode L1** | 242.2 (`2.93x`) | 455.7 (`2.89x`) | 431.2 (`2.89x`) | 357.2 (`2.89x`) | 437.8 (`2.89x`) | 94.0 (`2.87x`) | 51.4 (`2.71x`) | — |
| **Encode L3** | 171.1 (`3.12x`) | 249.7 (`3.20x`) | 233.0 (`3.20x`) | 188.4 (`3.20x`) | 228.7 (`3.19x`) | 83.1 (`3.18x`) | 44.4 (`2.88x`) | — |
| **Encode L9** | 17.1 (`3.48x`) | 60.4 (`3.59x`) | 57.1 (`3.59x`) | 46.0 (`3.59x`) | 60.2 (`3.57x`) | 17.4 (`3.52x`) | 27.8 (`2.89x`) | — |

### 4. Cross-Codec & Memory Footprint (Single-Core `taskset -c 4`, Silesia)

- **Cross-Codec L3 Encode/Decode (`MB/s ↑` @ Ratio `↑`)**: `yolowzstd` **170.7 / 756.4 @ 3.12x** vs `libdeflater` 97.4 / 1,204.3 @ 3.14x, `zlib-rs` 85.1 / 848.9 @ 3.08x, `lz4_flex` 651.0 / 3,102.9 @ 2.10x (`yolowzstd` encodes **1.75×** faster than `libdeflater` and **2.0×** faster than `zlib-rs` at comparable ratio).
- **Single-Process Peak RSS Delta (`mozilla` L3, `VmHWM` KiB `↓`)**:
  - Decode delta: `c_libzstd` 49,396 KiB, `zstd-pure-rs` 49,396 KiB, **`yolowzstd` 49,404 KiB**, `rust-zstd` 49,524 KiB, `structured-zstd` 49,656 KiB, `ruzstd` 55,820 KiB.
  - Encode delta: `c_libzstd` 18,040 KiB, `rust-zstd` 18,420 KiB, `zstd-pure-rs` 18,420 KiB, `structured-zstd` 19,464 KiB, **`yolowzstd` 19,700 KiB**.

### Honest Tradeoffs

- `yolowzstd` forbids `unsafe` (`#![forbid(unsafe_code)]`), so on single-core long-stream decode C `libzstd` and `unsafe`-heavy / C-translated Rust crates (`rust-zstd`, `zstd-pure-rs`) remain faster per core (`1,019–1,315 MB/s` vs `806.5 MB/s` at L3); `yolowzstd` leads all `#![forbid(unsafe_code)]` stream decoders (`zstd-rs`, `oxiarc-zstd`, `ruzstd`), leads 64 B encode/decode across every implementation tested, and beats `libzstd` across multi-core L3/L9/L19 encoding and multi-frame decoding.
- Single-core L9/L19 and multi-core L19 trade lower compression ratio (`3.46x` vs `4.01x` at L19) for bounded window/search depth and nearly 10× higher multi-core L19 speed (`32.6` vs `3.3 MB/s`).

---

## License

Licensed under the [MIT License](LICENSE).
