# yolowzstd Benchmarks

Complete reproduction harness and raw per-file benchmark outputs on the standard 12-file **Silesia Corpus** (211,938,580 bytes total).

## Test Environment

Recorded in [results/env.txt](results/env.txt):

- **CPU**: Intel(R) Core(TM) i7-10700 @ 2.90 GHz (8 physical cores / 16 threads, 1 NUMA node, 32 KiB L1d / 256 KiB L2 per core, 16 MiB shared L3)
- **RAM**: 15 GiB
- **OS**: Linux 7.0.0-43-generic x86_64 (Ubuntu)
- **Toolchain**: `rustc 1.99.0`, `cargo 1.99.0`, `zstd` CLI `v1.5.7`
- **Build profile**: `[profile.release] lto = "fat"`, `codegen-units = 1`

## Competitors Tested (`benchmarks/compare/Cargo.toml`)

- **C FFI / Reference**: `zstd 0.14.4` (`libzstd 1.5.7`, with `zstdmt` feature enabled for multi-core tests)
- **C-translated / `unsafe` Rust ports**: `rust-zstd 0.4`, `zstd-pure-rs 0.2`
- **Other Rust Zstd crates**: `structured-zstd 0.3`, `zstd-rs 0.1`, `oxiarc-zstd 0.4`, `ruzstd 0.9`
- **Cross-codec**: `libdeflater 1.24`, `zlib-rs 0.6`, `lz4_flex 0.14`

## Running the Benchmarks

Place the 12 standard Silesia files under `benchmarks/corpus/silesia/` (`dickens`, `mozilla`, `mr`, `nci`, `ooffice`, `osdb`, `reymont`, `samba`, `sao`, `webster`, `x-ray`, `xml`), then build and run:

```bash
cd benchmarks/compare
cargo build --release
BIN=./target/release/yolowzstd-compare

# Single-core (pinned to one core with taskset -c 4)
taskset -c 4 $BIN smoke ../corpus/silesia
taskset -c 4 $BIN short ../corpus/silesia 5
taskset -c 4 $BIN decode ../corpus/silesia 3 5
taskset -c 4 $BIN decode ../corpus/silesia 19 5
taskset -c 4 $BIN encode ../corpus/silesia 1 3
taskset -c 4 $BIN encode ../corpus/silesia 3 3
taskset -c 4 $BIN encode ../corpus/silesia 9 2
taskset -c 4 $BIN cross ../corpus/silesia 3

# Whole-machine multi-core (yolowzstd auto/8t/16t vs c_libzstd 8t/16t + parallel decode)
$BIN mt ../corpus/silesia 1 3
$BIN mt ../corpus/silesia 3 3
$BIN mt ../corpus/silesia 9 2
$BIN mt ../corpus/silesia 19 1

# Peak RSS delta (single-process VmHWM on mozilla L3)
taskset -c 4 $BIN rss_one ../corpus/silesia dec yolow mozilla 3
taskset -c 4 $BIN rss_one ../corpus/silesia enc yolow mozilla 3
```

## Saved Result Files

All raw result tables referenced in the main README are stored verbatim under `benchmarks/results/`:

- [final_smoke.txt](results/final_smoke.txt) — bidirectional interoperability check across all 8 Zstd implementations
- [final_mt_1.txt](results/final_mt_1.txt), [final_mt_3.txt](results/final_mt_3.txt), [final_mt_9.txt](results/final_mt_9.txt), [final_mt_19.txt](results/final_mt_19.txt) — multi-core L1/L3/L9/L19 encode & multi-frame decode
- [final_short.txt](results/final_short.txt) — 64 B / 1 KiB / 4 KiB / 64 KiB short payload benchmark
- [final_decode_3.txt](results/final_decode_3.txt), [final_decode_19.txt](results/final_decode_19.txt) — single-core decode across 8 Zstd decoders
- [final_encode_1.txt](results/final_encode_1.txt), [final_encode_3.txt](results/final_encode_3.txt), [final_encode_9.txt](results/final_encode_9.txt) — single-core encode across 7 Zstd encoders
- [final_cross.txt](results/final_cross.txt) — single-core L3 cross-codec comparison (`libdeflater`, `zlib-rs`, `lz4_flex`)
- [final_rss.txt](results/final_rss.txt) — isolated single-process `VmHWM` peak RSS deltas on `mozilla` L3
