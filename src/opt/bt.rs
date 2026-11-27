//! 精确后缀二叉搜索树（BT4）+ Hash3 + Long7 帕累托匹配索引（Pavlov/Collet 子树拼接）。
use crate::lz::{MIN, h_long, mlen, rd, rd4};

const PRIME4: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME3: u32 = 0x1E35_A7BD;
const HASH3_BITS: u32 = 14;
const HASH3_SIZE: usize = 1 << HASH3_BITS;
const HASH3_MASK: usize = HASH3_SIZE - 1;
/// 3 字节短匹配信息论有效偏移上限（超过 4 KiB 时 3B 序列普遍劣于 3 字面量）。
const HASH3_MAX_OFF: usize = 4096;

#[inline(always)]
fn h4(v32: u32, bits: u32) -> usize {
    (((v32 as u64).wrapping_mul(PRIME4)) >> (64 - bits)) as usize
}

#[inline(always)]
fn h3(v24: u32) -> usize {
    ((v24.wrapping_mul(PRIME3)) >> (32 - HASH3_BITS)) as usize & HASH3_MASK
}

pub(super) struct OptBt {
    head: Vec<u32>,
    long: Vec<u32>,
    hash3: Vec<u32>,
    prev: Vec<u32>,
    max_bits: u32,
    active_bits: u32,
    pub(super) win_mask: usize,
    pub(super) base: usize,
    pub(super) next_base: usize,
    pub(super) next_insert: usize,
}

impl OptBt {
    pub(super) fn new() -> Self {
        Self {
            head: Vec::new(),
            long: Vec::new(),
            hash3: Vec::new(),
            prev: Vec::new(),
            max_bits: 0,
            active_bits: 0,
            win_mask: 0,
            base: 0,
            next_base: 0,
            next_insert: 0,
        }
    }

    pub(super) fn prepare(&mut self, data_len: usize, hash_bits: u32, window: usize) {
        let want_bits = hash_bits.max(10);
        let want_win = window.max(1024).next_power_of_two();
        let want_prev = want_win.saturating_mul(2);
        let mut need_zero = false;

        if want_bits > self.max_bits {
            self.max_bits = want_bits;
            self.head.resize(1 << want_bits, 0);
            self.long.resize(1 << want_bits, 0);
            need_zero = true;
        }
        if self.hash3.len() < HASH3_SIZE {
            self.hash3.resize(HASH3_SIZE, 0);
            need_zero = true;
        }
        if want_prev > self.prev.len() {
            self.prev.resize(want_prev, 0);
            need_zero = true;
        } else if self.prev.len() > want_prev.saturating_mul(4).max(4 << 20) {
            self.prev.truncate(want_prev);
            self.prev.shrink_to_fit();
            need_zero = true;
        }

        let overflow = self
            .next_base
            .checked_add(data_len + 2)
            .is_none_or(|n| n > u32::MAX as usize);
        if need_zero || overflow {
            self.head.fill(0);
            self.long.fill(0);
            self.hash3.fill(0);
            self.prev.fill(0);
            self.base = 0;
            self.next_base = data_len + 1;
        } else {
            self.base = self.next_base;
            self.next_base += data_len + 1;
        }

        self.active_bits = want_bits.min(self.max_bits);
        self.win_mask = want_win - 1;
        self.next_insert = 0;
    }

    /// 后缀 BST 插入 + 帕累托匹配枚举（Hash3 3B -> BST 4B+ 偏移递增 -> Long 7B）。
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bt_step<const COLLECT: bool>(
        &mut self,
        d: &[u8],
        p: usize,
        end: usize,
        nb_comp: usize,
        sufficient: usize,
        win_limit: usize,
        cands: &mut [(usize, usize)],
        nc: &mut usize,
        max_ml: &mut usize,
    ) {
        let d_len = d.len();
        if p + 3 > end {
            return;
        }
        let base = self.base;
        let bits = self.active_bits;
        let wmask = self.win_mask;
        let win_cap = wmask + 1;
        let new_pv = (base + p + 1) as u32;
        let blk_cap = end - p;

        let v3 = if p + 4 <= d_len {
            rd4(d, p) & 0x00FF_FFFF
        } else {
            (d[p] as u32) | ((d[p + 1] as u32) << 8) | ((d[p + 2] as u32) << 16)
        };
        let hh3 = h3(v3);
        let old3 = self.hash3[hh3] as usize;
        self.hash3[hh3] = new_pv;
        if COLLECT && old3 > base && *max_ml < 3 {
            let c = old3 - base - 1;
            if c < p {
                let off = p - c;
                if off <= win_limit && off <= HASH3_MAX_OFF {
                    let match3 = if p + 4 <= d_len {
                        (rd4(d, c) & 0x00FF_FFFF) == v3
                    } else {
                        d[c] == d[p] && d[c + 1] == d[p + 1] && d[c + 2] == d[p + 2]
                    };
                    if match3 && *nc < cands.len() {
                        *max_ml = 3;
                        cands[*nc] = (3, off);
                        *nc += 1;
                    }
                }
            }
        }

        if p + MIN > end {
            self.next_insert = self.next_insert.max(p + 1);
            return;
        }

        let v32 = rd4(d, p);
        let h = h4(v32, bits);
        let mut raw = self.head[h] as usize;
        self.head[h] = new_pv;
        self.next_insert = self.next_insert.max(p + 1);

        let mut smaller_slot = (p & wmask) * 2;
        let mut larger_slot = smaller_slot + 1;
        let mut common_smaller = 0usize;
        let mut common_larger = 0usize;

        for _ in 0..nb_comp {
            if raw <= base {
                self.prev[smaller_slot] = 0;
                self.prev[larger_slot] = 0;
                break;
            }
            let c = raw - base - 1;
            if c >= p || p - c > win_limit || p - c >= win_cap {
                self.prev[smaller_slot] = 0;
                self.prev[larger_slot] = 0;
                break;
            }
            let m_common = common_smaller.min(common_larger);
            let ml = if m_common == 0 && c + 8 <= d_len && p + 8 <= d_len {
                let diff = rd(d, c) ^ rd(d, p);
                if diff != 0 {
                    (diff.trailing_zeros() >> 3) as usize
                } else {
                    8 + mlen(d, c + 8, p + 8, d_len)
                }
            } else {
                m_common + mlen(d, c + m_common, p + m_common, d_len)
            };
            let blk_ml = ml.min(blk_cap);
            if COLLECT && blk_ml >= MIN && blk_ml > *max_ml {
                *max_ml = blk_ml;
                if *nc < cands.len() {
                    cands[*nc] = (blk_ml, p - c);
                    *nc += 1;
                } else {
                    cands[cands.len() - 1] = (blk_ml, p - c);
                }
            }
            if ml >= sufficient {
                let c_slot = (c & wmask) * 2;
                self.prev[smaller_slot] = self.prev[c_slot];
                self.prev[larger_slot] = self.prev[c_slot + 1];
                smaller_slot = usize::MAX;
                break;
            }
            if p + ml >= d_len {
                break;
            }
            if d[c + ml] < d[p + ml] {
                self.prev[smaller_slot] = raw as u32;
                smaller_slot = (c & wmask) * 2 + 1;
                common_smaller = ml;
                raw = self.prev[smaller_slot] as usize;
            } else {
                self.prev[larger_slot] = raw as u32;
                larger_slot = (c & wmask) * 2;
                common_larger = ml;
                raw = self.prev[larger_slot] as usize;
            }
        }
        if smaller_slot != usize::MAX {
            self.prev[smaller_slot] = 0;
            self.prev[larger_slot] = 0;
        }

        if p + 8 <= d_len {
            let hl = h_long(rd(d, p), bits);
            let old_l = self.long[hl] as usize;
            self.long[hl] = new_pv;
            if COLLECT && old_l > base {
                let c = old_l - base - 1;
                if c < p && p - c <= win_limit && p - c < win_cap {
                    let ml = mlen(d, c, p, end);
                    if ml >= MIN && ml > *max_ml {
                        *max_ml = ml;
                        if *nc < cands.len() {
                            cands[*nc] = (ml, p - c);
                            *nc += 1;
                        } else {
                            cands[cands.len() - 1] = (ml, p - c);
                        }
                    }
                }
            }
        }
    }

    #[inline(always)]
    pub(super) fn insert_match_interior(
        &mut self,
        d: &[u8],
        from: usize,
        stop: usize,
        end: usize,
        sufficient: usize,
        win_limit: usize,
    ) {
        let len = stop.saturating_sub(from);
        let stride = if len <= 24 {
            1
        } else if len <= 96 {
            2
        } else {
            (len / 24).max(3)
        };
        let mut dummy = [(0usize, 0usize); 1];
        let mut p = self.next_insert.max(from);
        let mut ins = 0usize;
        while p < stop && p + 3 <= end && ins < 64 {
            let mut dn = 0;
            let mut dm = 0;
            self.bt_step::<false>(
                d, p, end, 12, sufficient, win_limit, &mut dummy, &mut dn, &mut dm,
            );
            p += stride;
            ins += 1;
        }
        self.next_insert = self.next_insert.max(stop);
    }
}
