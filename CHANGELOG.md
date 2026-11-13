# Changelog

## 0.1.0 - 2026-11-13

- Initial public release of `yolowzstd`: pure-Rust (`#![forbid(unsafe_code)]`, zero runtime dependencies, `edition = "2024"`, MSRV `1.89`) RFC 8878 Zstandard encoder and decoder.
- Full RFC 8878 decoder with Raw/RLE/Compressed blocks, 1/4-stream Huffman with tree reuse, Predefined/RLE/Custom/Repeat FSE tables, repeat offsets, multi-frame & skippable frames, XXH64 checksum verification, and strict output-limit enforcement.
- Adaptive multi-tier encoder (`level <= 0` through `22`) with generational L2-sized Fast/DoubleFast hash tables, hash chains + lazy matching, binary-tree suffix matcher, and block-optimal DP parser.
- Zero-dependency multi-core parallel frame encoding (`CompressOptions::with_threads`) and parallel multi-frame decoding (`DecompressOptions::with_threads`, `Decompressor::decompress_with`, `decompress_with`).
- Hardware cache and SMT topology awareness (`resources.rs`) with compact (`u32`) and wide (`usize`) history engines.
