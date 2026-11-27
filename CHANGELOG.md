# Changelog

## 0.2.0 - 2026-11-27

- **Modular architecture split (`src/dec/`, `src/enc/`, `src/lz/`, `src/opt/`, `src/entropy.rs`)**: refactored all monolithic source files into focused submodules (all 23 source files `<= 767` lines) while preserving `#![forbid(unsafe_code)]`, zero runtime dependencies, and full public API compatibility.
- **Zero-copy parallel multi-frame decoding (`src/mt.rs`, `src/dec/block.rs`, `src/dec/mod.rs`)**: replaced per-frame `Mutex<Vec<u8>>` output slots and post-decode copying with direct `split_at_mut` zero-copy decoding into the pre-allocated output slice (removing dead `Decompressor.mt_slots`). Multi-core Silesia parallel decoding jumped **+45% to +58%**: `4,614.3 MB/s` (L1 `auto`), `4,073.0 MB/s` (L3 `auto`), `4,321.1 / 4,461.2 MB/s` (L9 `auto`/`16t`), and `4,363.9 / 4,539.2 MB/s` (L19 `auto`/`16t`, **4.06×** single-thread C `libzstd`).
- **Sequence decode hot-loop & 12-bit dual-symbol Huffman (`src/dec/seq.rs`, `src/dec/tables.rs`, `src/huff.rs`, `src/fse.rs`)**:
  - Compacted single-base sequence tables, reused batch buffers in `State` without per-block zeroing, and tightened slice invariants in `src/dec/seq.rs` to eliminate inner-loop register spills and redundant bounds checks.
  - Upgraded Huffman decoding to a lazily built **12-bit dual-symbol canonical table** (16 KiB, 4 lookups per 64-bit load, gated by exact `P(l0 + l1 <= 12)` profitability) with specialized FSE weight table building (`build_weights_into`).
  - Single-core Silesia L3 decode broke **906.8 MB/s** (`+3.3%` over `0.1.3`), L19 decode reached `842.6 MB/s` (**81.0%** of C `libzstd`), 64 B short-payload decode surged **+48.4%** to **4,062.6 MB/s** (**1.74×** C `libzstd`), single-core L3 encode rose to **209.4 MB/s** (`+10.1%`), and multi-core L3/L9/L19 encode rose to **893.4 / 183.2 / 37.6 MB/s** (`auto`).
- **Security & robustness hardening (`src/mt.rs`, `src/dec/mod.rs`, `src/dec/block.rs`, `src/fse.rs`, `src/entropy.rs`)**:
  - Added exact RFC 8878 block-header output lower/upper bound accumulation in `scan_frames` and tightened single/multi-frame reservation caps to block forged `Frame_Content_Size` giant pre-allocation DoS inputs before allocating memory.
  - Fixed 32-bit arithmetic overflow on skippable frame length `4 + n` via chained `checked_add`, replaced `as usize` truncation with `usize::try_from`, and added `al` upper-bound guards in `fse::build_into` / `build_weights_into`.
  - Expanded test suite to 51 tests including multi-block tail 32 B boundaries, forged 1 GiB FCS anti-OOM verification, 300-round parallel-vs-single-thread mutation fuzzing, KKT normalization FSE roundtrips, and `OPT_CHUNK = 4096` tail-truncation tests.

## 0.1.3 - 2026-11-27

- **SWAR tag-filtered RowHash matcher for L5..=L11 (`src/row.rs`, `src/enc.rs`)**: replaced linked hash chains with a 100% safe-Rust 8-byte zero-carry SWAR tag-filtered set-associative row hash (inspired by SwissTable / Zstd 1.5 RowHash / Warren *Hacker's Delight* §6-1, §7-1), paired with a 128 KiB zero-false-positive exact-tagged 4-byte near-offset table, 3-byte repeat-offset probing, and rate-distortion Lazy/Lazy2 lookahead. Single-core Silesia L9 throughput jumped **2.37×** (`17.1 → 40.6 MB/s`) while improving compression ratio (`3.48x → 3.55x`), and multi-core L9 rose from `126.4` to `161.8 MB/s` (`auto`) / `174.6 MB/s` (`8t`).
- **Entropy-guided optimal parser for L16..=L22 (`src/opt.rs`, `src/chain.rs`)**: added a 1/8-bit Q3 fixed-point adaptive entropy model (online Bayesian smoothing + EMA, inspired by Zopfli / Zstd `btultra`), L2-resident chunked DP (`OPT_CHUNK = 4096`) with telescoping literal-length potential and Pareto-dominance single-pass relaxation over exact BT4 suffix tree matches (`src/chain.rs` also upgraded for L12..=L15 exact BT4 + lazy matching). Multi-core L19 improved in both speed (`32.3 → 34.5 MB/s` `auto`) and ratio (`3.46x → 3.56x`, 11× faster than C `libzstd` `zstdmt`).
- **Fast / DoubleFast & adaptive skipping for L1..=L4 (`src/lz.rs`, `src/compact.rs`)**: refined DoubleFast hash probing, aligned `Fast` and `Compact` extra-put policies across all L1..=L4 levels, and added SPRT-style high-entropy adaptive step skipping. Single-core Silesia L1 encode rose `242.2 → 284.2 MB/s` (`+17.3%`) and L3 encode rose `171.1 → 190.2 MB/s` (`+11.2%`) at `3.13x` (`+0.01x`, overtaking `zstd-pure-rs`).
- **Information-theoretic entropy coding & 11-bit dual-symbol Huffman decode (`src/henc.rs`, `src/huff.rs`, `src/dec.rs`, `src/fse.rs`, `src/xxh.rs`)**:
  - Added empirical Shannon entropy raw-skip lower bound, KL-divergence Repeat-tree fast decision, Rissanen MDL adaptive FSE table accuracy (`opt_fse_al`), 8.8 fixed-point exact cross-entropy FSE mode selection, convex minimum-KL integer quota normalization (Gross/Duda) with RFC 8878 `-1` support, and `O(n)` Kraft-truncated length-limited Huffman coding (Moffat & Katajainen).
  - Upgraded Huffman decoding (`src/huff.rs`) with an 11-bit dual-symbol canonical lookup table (Moffat & Turpin) gated automatically by `P(L1 + L2 <= 11) >= 42%` and 5-lookup-per-load refill, plus `as_chunks::<32>()` XXH64 (`src/xxh.rs`) and streamlined FSE decoding (`src/dec.rs`, `src/fse.rs`). Single-core Silesia L3 decode improved `806.5 → 878.0 MB/s` (`+8.9%`) and L19 decode improved `652.1 → 844.7 MB/s` (`+29.5%`, overtaking `structured-zstd`).
  - Hardened RFC 8878 §4.2.1.2 Huffman FSE weight encoding to reject single-weight distributions (`active_w <= 1`) and enforce `nb > 0` on the terminating state transition, preventing undecodable weight streams.
- **Adversarial & boundary test suite (`tests/comprehensive.rs`, unit tests)**: added 10+ regression tests covering single-weight/skewed Huffman FSE weights, deep Fibonacci Kraft truncation, `u32::MAX` epoch wraparound in `RowMatcher`/`OptParser`, `window_log = 10..=11` across L0..=L22, `OPT_CHUNK` overflow, `> 32,512` 3-byte sequences, and bidirectional `zstd` CLI interoperability.

## 0.1.2 - 2026-11-13

- Fix `src/dec.rs` block error rollback ordering: validate sequence bitstream header (`RevCursor::new(seq_data)?`) before resizing `out` to `block_end_cap + 32`, preventing incomplete block padding or `out.len() > max_out` leakage when `decompress_into` fails on truncated sequence streams.

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
