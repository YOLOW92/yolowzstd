//! SWAR 标签过滤组相联行哈希匹配器（Set-Associative Tag-Filtered Row Hash，L5..=L11）：
//!
//! 参考文献与理论基础：
//! - Kulukundis (2017), Google Abseil SwissTable：以独立 1 字节元数据标签行并行过滤非匹配槽位；
//! - Collet & Terrell (2021), Zstandard 1.5 Row-Based Hash：以 16/32/64 路循环行替代指针追逐哈希链；
//! - Warren, *Hacker's Delight* §7-1：SWAR 8 字节并行等值掩码与零进位乘法位压缩（`0x0002_0408_1020_4081`）；
//! - 信息论率失真前瞻（Lazy / Lazy2）：4 bit/byte 边际压缩率权重 + 3 字节重复偏移匹配。
use crate::enc::Seq;
use crate::lz::{MIN, apply_rep, mlen, rd, rd4};

const PRIME: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME5: u64 = 0x0000_080F_76A3_9B53;
/// 1-路精确带标签 4B 近距表：16384 槽 × 8B = 128 KiB（0% 误报，命中时免读原文 `d[c]`）。
const NEAR4_BITS: u32 = 14;
const NEAR4_SIZE: usize = 1 << NEAR4_BITS;
const NEAR4_MASK: usize = NEAR4_SIZE - 1;
const NEAR4_MAX_OFF: usize = 256 * 1024;

/// 8 字节 SWAR 并行标签匹配并压缩为低 8 位位掩码（Warren §6-1 零跨字节借位/进位精确公式，100% safe Rust）。
/// 位 `k`（0..8）为 1 当且仅当 `bytes[k] == tag`。
#[inline(always)]
fn tag_mask_8(bytes: &[u8; 8], splat: u64) -> u64 {
    let x = u64::from_le_bytes(*bytes) ^ splat;
    let t = (x & 0x7F7F_7F7F_7F7F_7F7F).wrapping_add(0x7F7F_7F7F_7F7F_7F7F);
    let m = !(t | x) & 0x8080_8080_8080_8080;
    m.wrapping_mul(0x0002_0408_1020_4081) >> 56
}

/// 5 字节前缀行哈希 + 不相交 8 位标签：消除 4-gram Zipf 饱和碰撞，保留 0 为空槽位。
#[inline(always)]
fn hash5_row_tag(v64: u64, row_bits: u32) -> (usize, u8) {
    let hv = (v64 << 24).wrapping_mul(PRIME5);
    let row = (hv >> (64 - row_bits)) as usize;
    let raw_tag = (hv >> (64 - row_bits - 8)) as u8;
    let tag = if raw_tag == 0 { 1 } else { raw_tag };
    (row, tag)
}

#[inline(always)]
fn hash4_near(v4: u32) -> usize {
    (((v4 as u64).wrapping_mul(PRIME)) >> (64 - NEAR4_BITS)) as usize
}

/// 率失真（Rate-Distortion）匹配收益评分：以约 4 bit/byte 的典型边际压缩率权衡长度与偏移编码位数。
#[inline(always)]
fn rd_gain(len: usize, off: usize, rep_code: u8) -> isize {
    let off_bits = match rep_code {
        1 => 0,
        2 => 2,
        3 => 3,
        _ => (31 - (off as u32).saturating_add(3).leading_zeros()) as isize + 3,
    };
    (len as isize * 4) - off_bits
}

/// 探测 Zstd 三重复偏移，支持最小 3 字节重复偏移匹配（3B rep 匹配净省 8..15 bit）。
/// 在长字面量游程中（`check_all == false`）仅测主重复偏移 `rep[0]`，消除高熵段额外访存流。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn probe_reps3(
    d: &[u8],
    i: usize,
    end: usize,
    v4: u32,
    has_ll: bool,
    check_all: bool,
    win: usize,
    rep: &[usize; 3],
) -> (isize, usize, usize, u8) {
    let mut best = (isize::MIN, 0usize, 0usize, 0u8);
    let v3 = v4 & 0x00FF_FFFF;
    let r0 = if has_ll { rep[0] } else { rep[1] };
    if r0 >= 1 && r0 <= i && r0 <= win {
        let c = i - r0;
        let cv4 = rd4(d, c);
        if cv4 == v4 {
            let len = 4 + mlen(d, c + 4, i + 4, end);
            best = (rd_gain(len, r0, 1), r0, len, 1);
        } else if (cv4 & 0x00FF_FFFF) == v3 {
            best = (rd_gain(3, r0, 1), r0, 3, 1);
        }
    }
    if !check_all {
        return best;
    }
    let rest = if has_ll {
        [(rep[1], 2u8), (rep[2], 3)]
    } else {
        [(rep[2], 2u8), (rep[0].saturating_sub(1), 3)]
    };
    for &(ro, rcode) in &rest {
        if ro >= 1 && ro <= i && ro <= win {
            let c = i - ro;
            let cv4 = rd4(d, c);
            if cv4 == v4 {
                let len = 4 + mlen(d, c + 4, i + 4, end);
                let g = rd_gain(len, ro, rcode);
                if g > best.0 {
                    best = (g, ro, len, rcode);
                }
            } else if (cv4 & 0x00FF_FFFF) == v3 {
                let g = rd_gain(3, ro, rcode);
                if g > best.0 {
                    best = (g, ro, 3, rcode);
                }
            }
        }
    }
    best
}

#[derive(Clone, Copy)]
pub(crate) struct RowCfg {
    pub row_bits: u32,
    pub row_width: u8, // 16, 32, or 64
    pub lazy_steps: u8,
    pub sufficient: usize,
    pub window_limit: usize,
    pub miss_shift: u32,
}

impl RowCfg {
    pub fn for_level(level: i32, data_len: usize, window_limit: usize) -> Self {
        // L9: 5B 行哈希 row_bits=15, W=16 (524K 槽, 仅 2 次 SWAR u64) + 128 KiB 精确标签 4B 近距表
        let (row_width, lazy_steps, sufficient, max_win, max_row_bits, miss_shift) = match level {
            ..=6 => (16u8, 1u8, 64usize, 4usize << 20, 14u32, 7u32),
            7..=8 => (16, 1, 96, 4 << 20, 15, 7),
            9 => (16, 2, 128, 8 << 20, 15, 7),
            _ => (32, 2, 192, 8 << 20, 15, 8),
        };
        let min_bits = if data_len <= 16384 {
            10
        } else if data_len <= 65536 {
            12
        } else if data_len <= 262144 {
            13
        } else {
            max_row_bits
        };
        let row_bits = min_bits.min(max_row_bits);
        Self {
            row_bits,
            row_width,
            lazy_steps,
            sufficient,
            window_limit: window_limit.min(max_win).min((u32::MAX - 3) as usize),
            miss_shift,
        }
    }
}

/// 双尺度组相联行哈希匹配器：
/// - 远距 5+ 字节匹配：`tags` + `pos` 分离存放的 5B SWAR 行哈希；
/// - 近距 4 字节匹配：`near4` 128 KiB 精确带标签 `(v4 << 32) | pos` 表（0% 误报，无匹配零原文访存）。
pub(crate) struct RowMatcher {
    tags: Vec<u8>,
    pos: Vec<u32>,
    head: Vec<u8>,
    near4: Vec<u64>,
    row_bits: u32,
    row_width: usize,
    base: usize,
    next_base: usize,
    next_insert: usize,
}

impl RowMatcher {
    pub fn new() -> Self {
        Self {
            tags: Vec::new(),
            pos: Vec::new(),
            head: Vec::new(),
            near4: Vec::new(),
            row_bits: 0,
            row_width: 0,
            base: 0,
            next_base: 0,
            next_insert: 0,
        }
    }

    pub fn prepare(&mut self, data_len: usize, cfg: RowCfg) {
        let rows = 1usize << cfg.row_bits;
        let width = cfg.row_width as usize;
        let total_slots = rows * width;

        let shape_changed = self.row_bits != cfg.row_bits || self.row_width != width;
        if self.tags.len() < total_slots {
            self.tags.resize(total_slots, 0);
        }
        if self.pos.len() < total_slots {
            self.pos.resize(total_slots, 0);
        }
        if self.head.len() < rows {
            self.head.resize(rows, 0);
        }
        if self.near4.len() < NEAR4_SIZE {
            self.near4.resize(NEAR4_SIZE, 0);
        }

        let overflow = self
            .next_base
            .checked_add(data_len + 2)
            .is_none_or(|n| n > u32::MAX as usize);
        if shape_changed || overflow || data_len >= 65536 {
            self.tags[..total_slots].fill(0);
            self.head[..rows].fill(0);
            if shape_changed || overflow {
                self.pos[..total_slots].fill(0);
                self.near4.fill(0);
                self.base = 0;
                self.next_base = data_len + 1;
            } else {
                self.base = self.next_base;
                self.next_base += data_len + 1;
            }
        } else {
            self.base = self.next_base;
            self.next_base += data_len + 1;
        }
        self.row_bits = cfg.row_bits;
        self.row_width = width;
        self.next_insert = 0;
    }

    #[inline(always)]
    fn insert_pos<const W: usize>(&mut self, d: &[u8], p: usize, row_mask: usize) {
        if p + 8 > d.len() {
            return;
        }
        let v64 = rd(d, p);
        let v4 = v64 as u32;
        let pv = (self.base + p + 1) as u32;
        self.near4[hash4_near(v4) & NEAR4_MASK] = ((v4 as u64) << 32) | (pv as u64);
        let (row_raw, tag) = hash5_row_tag(v64, self.row_bits);
        let row = row_raw & row_mask;
        let h = (self.head[row].wrapping_sub(1)) & (W as u8 - 1);
        self.head[row] = h;
        let slot = row * W + h as usize;
        self.tags[slot] = tag;
        self.pos[slot] = pv;
    }

    #[inline(always)]
    fn match_row_mask<const W: usize>(tags: &[u8], row_base: usize, tag: u8) -> u64 {
        let splat = (tag as u64).wrapping_mul(0x0101_0101_0101_0101);
        let slice = &tags[row_base..row_base + W];
        let (chunks, _) = slice.as_chunks::<8>();
        match W {
            16 => {
                let m0 = tag_mask_8(&chunks[0], splat);
                let m1 = tag_mask_8(&chunks[1], splat);
                m0 | (m1 << 8)
            }
            32 => {
                let m0 = tag_mask_8(&chunks[0], splat);
                let m1 = tag_mask_8(&chunks[1], splat);
                let m2 = tag_mask_8(&chunks[2], splat);
                let m3 = tag_mask_8(&chunks[3], splat);
                m0 | (m1 << 8) | (m2 << 16) | (m3 << 24)
            }
            _ => {
                let m0 = tag_mask_8(&chunks[0], splat);
                let m1 = tag_mask_8(&chunks[1], splat);
                let m2 = tag_mask_8(&chunks[2], splat);
                let m3 = tag_mask_8(&chunks[3], splat);
                let m4 = tag_mask_8(&chunks[4], splat);
                let m5 = tag_mask_8(&chunks[5], splat);
                let m6 = tag_mask_8(&chunks[6], splat);
                let m7 = tag_mask_8(&chunks[7], splat);
                m0 | (m1 << 8)
                    | (m2 << 16)
                    | (m3 << 24)
                    | (m4 << 32)
                    | (m5 << 40)
                    | (m6 << 48)
                    | (m7 << 56)
            }
        }
    }

    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn find_and_insert<const W: usize>(
        &mut self,
        d: &[u8],
        i: usize,
        end: usize,
        has_ll: bool,
        check_all_reps: bool,
        rep: &[usize; 3],
        cfg: RowCfg,
        row_mask: usize,
    ) -> (isize, usize, usize) {
        let win = cfg.window_limit;
        let base = self.base;
        let pv = (base + i + 1) as u32;
        if i + 8 > end {
            let v4 = rd4(d, i);
            let best = probe_reps3(d, i, end, v4, has_ll, check_all_reps, win, rep);
            return (best.0, best.1, best.2);
        }

        let v64 = rd(d, i);
        let v4 = v64 as u32;
        let mut best = probe_reps3(d, i, end, v4, has_ll, check_all_reps, win, rep);

        // 1) 5B 组相联 SWAR 行哈希：全窗口 5+ 字节高收益匹配，避开 4-gram Zipf 饱和碰撞
        let (row_raw, tag) = hash5_row_tag(v64, self.row_bits);
        let row = row_raw & row_mask;
        let row_base = row * W;
        let h = self.head[row] as usize;

        if best.2 < cfg.sufficient {
            let raw_mask = Self::match_row_mask::<W>(&self.tags, row_base, tag);
            if raw_mask != 0 {
                let mut ordered: u64 = match W {
                    16 => (raw_mask as u16).rotate_right(h as u32) as u64,
                    32 => (raw_mask as u32).rotate_right(h as u32) as u64,
                    _ => raw_mask.rotate_right(h as u32),
                };
                let max_len = (end - i).min(cfg.sufficient);
                let pos_row = &self.pos[row_base..row_base + W];
                let mut min_new_len = if best.2 >= 5 { best.2 } else { 4 };
                let mut end_byte = if i + min_new_len < end {
                    d[i + min_new_len]
                } else {
                    0
                };
                while ordered != 0 {
                    let tz = ordered.trailing_zeros() as usize;
                    ordered &= ordered - 1;
                    let slot = (h + tz) & (W - 1);
                    let raw = pos_row[slot] as usize;
                    if raw <= base {
                        continue;
                    }
                    let c = raw - base - 1;
                    if c >= i {
                        continue;
                    }
                    let off = i - c;
                    if off > win {
                        break;
                    }
                    let need_strict = best.2 >= 5 && off >= best.1;
                    if need_strict && i + min_new_len < end && d[c + min_new_len] != end_byte {
                        continue;
                    }
                    let diff = rd(d, c) ^ v64;
                    if (diff & 0x0000_00FF_FFFF_FFFF) == 0 {
                        let len = if diff != 0 {
                            (diff.trailing_zeros() >> 3) as usize
                        } else {
                            8 + mlen(d, c + 8, i + 8, end)
                        };
                        let g = rd_gain(len, off, 0);
                        if g > best.0 {
                            best = (g, off, len, 0);
                            if len > min_new_len {
                                min_new_len = len;
                                if i + min_new_len < end {
                                    end_byte = d[i + min_new_len];
                                }
                            }
                            if len >= max_len {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // 2) 精确带标签 4B 近距表（0% 误报，无匹配零原文访存；仅在未找到 5+ 字节匹配时免读原文采纳 4B 候选）
        let h4 = hash4_near(v4) & NEAR4_MASK;
        let old4 = self.near4[h4];
        self.near4[h4] = ((v4 as u64) << 32) | (pv as u64);
        if best.2 < 5 && (old4 >> 32) as u32 == v4 {
            let raw4 = old4 as u32 as usize;
            if raw4 > base {
                let c = raw4 - base - 1;
                if c < i {
                    let off = i - c;
                    if off <= win && off <= NEAR4_MAX_OFF {
                        let g4 = rd_gain(4, off, 0);
                        if g4 > best.0 {
                            best = (g4, off, 4, 0);
                        }
                    }
                }
            }
        }

        let new_h = (h.wrapping_sub(1)) & (W - 1);
        self.head[row] = new_h as u8;
        let new_slot = row_base + new_h;
        self.tags[new_slot] = tag;
        self.pos[new_slot] = pv;
        self.next_insert = self.next_insert.max(i + 1);

        (best.0, best.1, best.2)
    }

    pub fn parse(
        &mut self,
        d: &[u8],
        range: (usize, usize),
        cfg: RowCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        match cfg.row_width {
            16 => self.parse_inner::<16>(d, range, cfg, rep, seqs, lits),
            32 => self.parse_inner::<32>(d, range, cfg, rep, seqs, lits),
            _ => self.parse_inner::<64>(d, range, cfg, rep, seqs, lits),
        }
    }

    #[inline(never)]
    fn parse_inner<const W: usize>(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        cfg: RowCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if end > d.len() {
            return start;
        }
        let d = &d[..end];
        if start > self.next_insert {
            self.next_insert = start;
        }
        let row_mask = (1usize << self.row_bits) - 1;
        let (mut i, mut anchor) = (start, start);

        while i + MIN <= end {
            let ll_dist = i - anchor;
            let mut cur =
                self.find_and_insert::<W>(d, i, end, ll_dist > 0, ll_dist < 4, rep, cfg, row_mask);
            if cur.2 < 3 {
                i += 1 + (ll_dist >> cfg.miss_shift);
                continue;
            }

            // 完整 Lazy / Lazy2 率失真前瞻（精确净收益门槛 +3 / +7 bit）
            let mut shifts = 0u8;
            while shifts < cfg.lazy_steps && cur.2 < cfg.sufficient && i + 1 + MIN <= end {
                let next1 =
                    self.find_and_insert::<W>(d, i + 1, end, true, true, rep, cfg, row_mask);
                if next1.0 > cur.0 + 3 {
                    i += 1;
                    cur = next1;
                    shifts += 1;
                    continue;
                }
                if cfg.lazy_steps >= 2 && cur.2 < 32 && i + 2 + MIN <= end {
                    let next2 =
                        self.find_and_insert::<W>(d, i + 2, end, true, true, rep, cfg, row_mask);
                    if next2.0 > cur.0 + 7 {
                        i += 2;
                        cur = next2;
                        shifts += 1;
                        continue;
                    }
                }
                break;
            }

            let (_, off, mut len) = cur;
            while i > anchor && off < i && d[i - 1] == d[i - off - 1] {
                i -= 1;
                len += 1;
            }
            let ll = i - anchor;
            if ll > 0 {
                lits.extend_from_slice(&d[anchor..i]);
            }
            let rep_code = apply_rep(rep, ll, off);
            seqs.push(Seq {
                ll: ll as u32,
                ml: len as u32,
                off: off as u32,
                rep_code,
            });

            let stop = i + len;
            let stride = if len <= 24 {
                1
            } else if len <= 64 {
                2
            } else {
                (len >> 5).max(3)
            };
            let mut p = self.next_insert.max(i + 1);
            let mut ins = 0usize;
            while p < stop && p + MIN <= end && ins < 32 {
                self.insert_pos::<W>(d, p, row_mask);
                p += stride;
                ins += 1;
            }
            self.next_insert = self.next_insert.max(stop);
            i = stop;
            anchor = i;
        }
        anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swar_tag_mask_matches_scalar_exhaustively() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..2000 {
            let mut arr = [0u8; 8];
            for b in &mut arr {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                *b = (s >> 40) as u8;
            }
            let tag = arr[0];
            let splat = (tag as u64).wrapping_mul(0x0101_0101_0101_0101);
            let got = tag_mask_8(&arr, splat);
            let mut want = 0u64;
            for (k, &b) in arr.iter().enumerate() {
                if b == tag {
                    want |= 1u64 << k;
                }
            }
            assert_eq!(got, want, "arr={arr:?} tag={tag}");
        }
        // 全相等极端边界（0xFF 饱和无进位）
        let all_ff = [0xFFu8; 8];
        assert_eq!(tag_mask_8(&all_ff, u64::MAX), 0xFF);
        let all_zero = [0u8; 8];
        assert_eq!(tag_mask_8(&all_zero, 0), 0xFF);
        assert_eq!(tag_mask_8(&all_zero, 0x0101_0101_0101_0101), 0);
    }

    #[test]
    fn row_matcher_handles_u32_next_base_wraparound_and_shape_switch() {
        let mut rm = RowMatcher::new();
        let small = b"alpha beta gamma alpha beta gamma delta ".repeat(20);
        let big = b"0123456789abcdef_row_matcher_wrap_test_\n".repeat(2500);

        let cfg_big = RowCfg::for_level(10, big.len(), usize::MAX);
        rm.prepare(big.len(), cfg_big);
        let (mut seqs, mut lits, mut rep) = (Vec::new(), Vec::new(), [1, 4, 8]);
        let a = rm.parse(
            &big,
            (0, big.len()),
            cfg_big,
            &mut rep,
            &mut seqs,
            &mut lits,
        );
        lits.extend_from_slice(&big[a..]);
        assert_eq!(
            lits.len() + seqs.iter().map(|s| s.ml as usize).sum::<usize>(),
            big.len()
        );

        // 模拟 next_base 逼近 u32::MAX 触发溢出重置，再切小帧与大帧
        rm.next_base = u32::MAX as usize - 64;
        let cfg_small = RowCfg::for_level(9, small.len(), 1024);
        rm.prepare(small.len(), cfg_small);
        assert_eq!(rm.base, 0);
        seqs.clear();
        lits.clear();
        rep = [1, 4, 8];
        let a2 = rm.parse(
            &small,
            (0, small.len()),
            cfg_small,
            &mut rep,
            &mut seqs,
            &mut lits,
        );
        lits.extend_from_slice(&small[a2..]);
        assert_eq!(
            lits.len() + seqs.iter().map(|s| s.ml as usize).sum::<usize>(),
            small.len()
        );
        assert!(seqs.iter().all(|s| (s.off as usize) <= 1024));

        // 再次强制同形状溢出回绕（不触发 shape_changed，仅触发 overflow）
        rm.next_base = u32::MAX as usize - 10;
        rm.prepare(small.len(), cfg_small);
        assert_eq!(rm.base, 0);
        seqs.clear();
        lits.clear();
        rep = [1, 4, 8];
        let a3 = rm.parse(
            &small,
            (0, small.len()),
            cfg_small,
            &mut rep,
            &mut seqs,
            &mut lits,
        );
        lits.extend_from_slice(&small[a3..]);
        assert_eq!(
            lits.len() + seqs.iter().map(|s| s.ml as usize).sum::<usize>(),
            small.len()
        );
    }
}
