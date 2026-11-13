# Changelog

## 0.1.1 - 2026-11-13

- Parallel decompression budget & fallback (`src/mt.rs`): enforce combined frame output budgets within remaining `max_out` and fall back cleanly to single-thread decoding when any frame lacks `Frame_Content_Size`.
- Strict RFC 8878 frame & window validation (`src/dec.rs`, `src/enc.rs`, `src/mt.rs`): verify exact final frame output length against `Frame_Content_Size` (including Raw, RLE, `nseq == 0`, and checksum-less frames), store and enforce `Window_Size` / `Block_Maximum_Size = min(Window_Size, 128 KiB)`, and bound match offsets to `Window_Size`.
- Context reuse & high-watermark bounds (`src/enc.rs`, `src/dec.rs`, `src/chain.rs`, `src/mt.rs`): release error/fallback `mt_slots`, cap retained slot buffers, automatically reclaim idle large working sets on level/thread shifts, and expose `Compressor::shrink_to_fit()` / `Decompressor::shrink_to_fit()`.
- Safe-Rust codec optimizations (`src/dec.rs`, `src/enc.rs`, `src/henc.rs`, `src/lz.rs`, `src/chain.rs`): batch sequence bit-decoding out-of-line, streamline FSE bit-packing and fast/chain hot loops without compromising compression ratio or `#![forbid(unsafe_code)]`.
- Documentation alignment (`README.md`, `docs/README.zh-CN.md`, `src/`): clarify parallel decode `Mutex` slot synchronization, `>= 2` FCS-known frame requirement, dictionary `Error::Unsupported` behavior, heuristic forward DP scope, and `max_out` semantics.

## 0.1.0 - 2026-11-13

- Initial public release of `yolowzstd`: pure-Rust (`#![forbid(unsafe_code)]`, zero runtime dependencies, `edition = "2024"`, MSRV `1.89`) RFC 8878 Zstandard encoder and decoder.
- RFC 8878 decoder (without dictionary support; dictionary frames return `Error::Unsupported`) with Raw/RLE/Compressed blocks, 1/4-stream Huffman with tree reuse, Predefined/RLE/Custom/Repeat FSE tables, repeat offsets, multi-frame & skippable frames, XXH64 checksum verification, and strict logical output-size enforcement (`max_out`).
- Adaptive multi-tier encoder (`level <= 0` through `22`) with generational L2-sized Fast/DoubleFast hash tables, hash chains + lazy matching, binary-tree suffix matcher, heuristic forward bit-cost DP block parser, and parallel multi-frame encoder/decoder (`>= 2` frames with known `Frame_Content_Size`).
- Zero-dependency multi-core parallel frame encoding (`CompressOptions::with_threads`) and parallel multi-frame decoding (`DecompressOptions::with_threads`, `Decompressor::decompress_with`, `decompress_with`).
- Hardware cache and SMT topology awareness (`resources.rs`) with compact (`u32`) and wide (`usize`) history engines.
