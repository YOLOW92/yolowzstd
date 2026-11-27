//! 哈希链与后缀二叉搜索树（BST）匹配器：
//! - level 12..=15（及超大输入回退路径）：后缀二叉搜索树（Pavlov/Collet 子树拼接 + 跨块字典序不变量）+ 2 步率失真前瞻；
//! - level 16..=22 的熵代价最优解析器见 [`crate::opt`]。
use crate::enc::Seq;
use crate::lz::{MIN, Position, apply_rep, h_long, match_score, mlen, probe_reps, rd, rd4};
use crate::resources::Budget;

#[inline(always)]
fn h4(v32: u32, bits: u32) -> usize {
    (((v32 as u64).wrapping_mul(0x9E37_79B1_85EB_CA87)) >> (64 - bits)) as usize
}

#[derive(Clone, Copy)]
pub(crate) struct ChainCfg {
    pub depth: usize,
    pub sufficient: usize,
    pub lazy_steps: u8,
    pub bt: bool,
    pub window_limit: usize,
}

impl ChainCfg {
    pub fn from_budget(b: &Budget, level: i32, window_limit: usize) -> Self {
        let sufficient = match level {
            ..=8 => 64,
            9..=11 => 96,
            _ => 256,
        };
        let bt = level >= 12;
        let depth = match level {
            ..=8 => b.depth,
            9..=11 => 16,
            12..=15 => 32,
            _ => 48,
        };
        let lazy_steps = if level >= 9 { 2 } else { 1 };
        Self {
            depth,
            sufficient,
            lazy_steps,
            bt,
            window_limit: window_limit.min((u32::MAX - 3) as usize),
        }
    }
}

pub(crate) struct Chain<P> {
    head: Vec<P>,
    long: Vec<P>,
    prev: Vec<P>,
    prev_bt: bool,
    max_bits: u32,
    active_bits: u32,
    win_mask: usize,
    base: usize,
    next_base: usize,
    local_span: usize,
    next: usize,
}

impl<P: Position> Chain<P> {
    pub fn new(budget: &Budget) -> Self {
        let bits = budget.hash_bits.max(1);
        let win = budget.window.max(1).next_power_of_two();
        Self {
            head: vec![P::EMPTY; 1 << bits],
            long: vec![P::EMPTY; 1 << bits],
            prev: vec![P::EMPTY; win],
            prev_bt: false,
            max_bits: bits,
            active_bits: bits,
            win_mask: win - 1,
            base: 0,
            next_base: 0,
            local_span: budget.local_span.max(1),
            next: 0,
        }
    }

    pub fn prepare(&mut self, data_len: usize, budget: &Budget, bt: bool) {
        let want_bits = budget.hash_bits.max(1);
        let want_win = budget.window.max(1).next_power_of_two();
        let want_prev = if bt {
            want_win.saturating_mul(2)
        } else {
            want_win
        };
        let mut need_zero = self.prev_bt != bt;
        if want_bits > self.max_bits {
            self.max_bits = want_bits;
            self.head.resize(1 << want_bits, P::EMPTY);
            self.long.resize(1 << want_bits, P::EMPTY);
            need_zero = true;
        }
        if want_prev > self.prev.len() {
            self.prev.resize(want_prev, P::EMPTY);
            need_zero = true;
        } else if self.prev.len() > want_prev.saturating_mul(4).max(4 << 20) {
            self.prev.truncate(want_prev);
            self.prev.shrink_to_fit();
            need_zero = true;
        }
        let overflow = if P::WIDE {
            self.next_base.checked_add(data_len + 2).is_none()
        } else {
            self.next_base
                .checked_add(data_len + 2)
                .is_none_or(|n| n > u32::MAX as usize)
        };
        if need_zero || overflow {
            self.head.fill(P::EMPTY);
            self.long.fill(P::EMPTY);
            self.prev.fill(P::EMPTY);
            self.base = 0;
            self.next_base = data_len + 1;
        } else {
            self.base = self.next_base;
            self.next_base += data_len + 1;
        }
        self.prev_bt = bt;
        self.active_bits = want_bits.min(self.max_bits);
        self.win_mask = want_win - 1;
        self.local_span = budget.local_span.max(1);
        self.next = 0;
    }

    #[inline(always)]
    fn insert_one(&mut self, d: &[u8], p: usize) {
        if p + MIN <= d.len() {
            let pv = P::from_raw(self.base + p + 1);
            let v32 = u32::from_le_bytes(d[p..p + 4].try_into().expect("4 字节"));
            let h = h4(v32, self.active_bits);
            self.prev[p & self.win_mask] = self.head[h];
            self.head[h] = pv;
            if p + 8 <= d.len() {
                let hl = h_long(rd(d, p), self.active_bits);
                self.long[hl] = pv;
            }
        }
    }

    #[inline(always)]
    fn find(
        &self,
        d: &[u8],
        i: usize,
        end: usize,
        has_ll: bool,
        rep: &[usize; 3],
        cfg: ChainCfg,
    ) -> (isize, usize, usize) {
        let win = (self.win_mask).min(cfg.window_limit);
        let target4 = &d[i..i + MIN];
        let v32 = u32::from_le_bytes(target4.try_into().expect("4 字节"));
        let mut best = probe_reps(d, i, end, v32, has_ll, win, rep, true);
        let base = self.base;
        if i + 8 <= d.len() {
            let v64 = rd(d, i);
            let raw = self.long[h_long(v64, self.active_bits)].to_raw();
            if raw > base {
                let c = raw - base - 1;
                if c < i && i - c <= win {
                    let off = i - c;
                    let len = mlen(d, c, i, end);
                    if len >= MIN {
                        let s = match_score(len, off, 0);
                        if s > best.0 {
                            best = (s, off, len, 0);
                        }
                    }
                }
            }
        }
        if best.2 >= cfg.sufficient {
            return (best.0, best.1, best.2);
        }
        let mut raw = self.head[h4(v32, self.active_bits)].to_raw();
        let mut spent = 0usize;
        let max_len = (end - i).min(cfg.sufficient);
        for _ in 0..cfg.depth {
            if raw <= base {
                break;
            }
            let c = raw - base - 1;
            if c >= i || i - c > win || best.2 >= max_len {
                break;
            }
            let off = i - c;
            spent += 1 + (off >> 18).min(2);
            if spent > cfg.depth {
                break;
            }
            // 同等或更大偏移时，尾哨兵 byte 必须匹配；更小偏移允许同长替代以降低偏移位数。
            let can_beat =
                best.2 == 0 || off < best.1 || i + best.2 >= end || d[c + best.2] == d[i + best.2];
            if can_beat && rd4(d, c) == v32 {
                let len = 4 + mlen(d, c + 4, i + 4, end);
                let s = match_score(len, off, 0);
                if s > best.0 {
                    best = (s, off, len, 0);
                }
            }
            raw = self.prev[c & self.win_mask].to_raw();
        }
        (best.0, best.1, best.2)
    }

    pub fn parse(
        &mut self,
        d: &[u8],
        range: (usize, usize),
        cfg: ChainCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if cfg.bt {
            self.parse_bt_lazy(d, range, cfg, rep, seqs, lits)
        } else {
            self.parse_lazy(d, range, cfg, rep, seqs, lits)
        }
    }

    /// 将位置 `p` 插入二叉搜索树并同步收集后缀匹配（Pavlov/Collet 子树拼接 + 跨块完整字典序比较）。
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn bt_step<const COLLECT: bool>(
        &mut self,
        d: &[u8],
        p: usize,
        end: usize,
        nb_comp: usize,
        sufficient: usize,
        win_limit: usize,
        cands: &mut [(usize, usize, u8)],
        nc: &mut usize,
        max_ml: &mut usize,
    ) {
        if p + MIN > end {
            return;
        }
        let d_len = d.len();
        let base = self.base;
        let bits = self.active_bits;
        let wmask = self.win_mask;
        let win_cap = wmask + 1;
        let new_pv = P::from_raw(base + p + 1);
        let blk_cap = end - p;

        let v32 = rd4(d, p);
        let h = h4(v32, bits);
        let mut raw = self.head[h].to_raw();
        self.head[h] = new_pv;
        self.next = self.next.max(p + 1);

        let mut smaller_slot = (p & wmask) * 2;
        let mut larger_slot = smaller_slot + 1;
        let mut common_smaller = 0usize;
        let mut common_larger = 0usize;

        for _ in 0..nb_comp {
            if raw <= base {
                self.prev[smaller_slot] = P::EMPTY;
                self.prev[larger_slot] = P::EMPTY;
                break;
            }
            let c = raw - base - 1;
            if c >= p || p - c > win_limit || p - c >= win_cap {
                self.prev[smaller_slot] = P::EMPTY;
                self.prev[larger_slot] = P::EMPTY;
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
            if COLLECT && blk_ml >= MIN && blk_ml > *max_ml && *nc < cands.len() {
                *max_ml = blk_ml;
                cands[*nc] = (blk_ml, p - c, 0);
                *nc += 1;
            }
            if ml >= sufficient {
                // Pavlov/Collet 精确子树拼接：所有 bt_step 共享同一 sufficient 阈值，保留 c 的左右子树
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
                self.prev[smaller_slot] = P::from_raw(raw);
                smaller_slot = (c & wmask) * 2 + 1;
                common_smaller = ml;
                raw = self.prev[smaller_slot].to_raw();
            } else {
                self.prev[larger_slot] = P::from_raw(raw);
                larger_slot = (c & wmask) * 2;
                common_larger = ml;
                raw = self.prev[larger_slot].to_raw();
            }
        }
        if smaller_slot != usize::MAX {
            self.prev[smaller_slot] = P::EMPTY;
            self.prev[larger_slot] = P::EMPTY;
        }

        // 7B 长表放于 BST 之后查询：绝不遮蔽 BST 收集到的更近距帕累托候选
        if p + 8 <= d_len {
            let hl = h_long(rd(d, p), bits);
            let old_l = self.long[hl].to_raw();
            self.long[hl] = new_pv;
            if COLLECT && old_l > base {
                let c = old_l - base - 1;
                if c < p && p - c <= win_limit && p - c < win_cap {
                    let ml = mlen(d, c, p, end);
                    if ml >= MIN && ml > *max_ml && *nc < cands.len() {
                        *max_ml = ml;
                        cands[*nc] = (ml, p - c, 0);
                        *nc += 1;
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn bt_best(
        &mut self,
        d: &[u8],
        p: usize,
        end: usize,
        has_ll: bool,
        rep: &[usize; 3],
        cfg: ChainCfg,
    ) -> (isize, usize, usize) {
        let win = (self.win_mask).min(cfg.window_limit);
        let v32 = rd4(d, p);
        let mut best = probe_reps(d, p, end, v32, has_ll, win, rep, true);
        let mut cands = [(0usize, 0usize, 0u8); 16];
        let mut nc = 0usize;
        let mut max_ml = 0usize;
        self.bt_step::<true>(
            d,
            p,
            end,
            cfg.depth,
            cfg.sufficient,
            cfg.window_limit,
            &mut cands,
            &mut nc,
            &mut max_ml,
        );
        for &(ml, off, _) in &cands[..nc] {
            let s = match_score(ml, off, 0);
            if s > best.0 {
                best = (s, off, ml, 0);
            }
        }
        (best.0, best.1, best.2)
    }

    #[inline(never)]
    fn parse_bt_lazy(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        cfg: ChainCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if end > d.len() {
            return start;
        }
        let (mut i, mut anchor, mut miss) = (start, start, 0usize);
        let mut dummy = [(0usize, 0usize, 0u8); 1];
        while i + MIN <= end {
            let mut cur = self.bt_best(d, i, end, i > anchor, rep, cfg);
            if cur.2 < MIN {
                i += 1 + (miss >> 5);
                miss += 1;
                continue;
            }
            miss = 0;
            let mut steps = 0u8;
            while steps < cfg.lazy_steps && cur.2 < cfg.sufficient && i + 1 + MIN <= end {
                let next = self.bt_best(d, i + 1, end, true, rep, cfg);
                if next.0 <= cur.0 + 6 {
                    break;
                }
                i += 1;
                cur = next;
                steps += 1;
            }
            let (_, off, mut len) = cur;
            while i > anchor && off < i && d[i - 1] == d[i - off - 1] {
                i -= 1;
                len += 1;
            }
            let ll = i - anchor;
            lits.extend_from_slice(&d[anchor..i]);
            let rep_code = apply_rep(rep, ll, off);
            seqs.push(Seq {
                ll: ll as u32,
                ml: len as u32,
                off: off as u32,
                rep_code,
            });
            let stop = i + len;
            let stride = (len / 12).max(1);
            let mut p = self.next.max(i + 1);
            while p + MIN <= end && p < stop {
                let mut nc = 0;
                let mut ml = 0;
                self.bt_step::<false>(
                    d,
                    p,
                    end,
                    8,
                    cfg.sufficient,
                    cfg.window_limit,
                    &mut dummy,
                    &mut nc,
                    &mut ml,
                );
                p += stride;
            }
            self.next = self.next.max(stop);
            i = stop;
            anchor = i;
        }
        anchor
    }

    #[inline(never)]
    fn parse_lazy(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        cfg: ChainCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if start > self.next {
            self.next = start;
        }
        let (mut i, mut anchor, mut miss) = (start, start, 0usize);
        while i + MIN <= end {
            let mut cur = self.find(d, i, end, i > anchor, rep, cfg);
            self.insert_one(d, i);
            self.next = self.next.max(i + 1);
            if cur.2 < MIN {
                i += 1 + (miss >> 5);
                miss += 1;
                continue;
            }
            miss = 0;
            let mut steps = 0u8;
            while steps < cfg.lazy_steps && cur.2 < cfg.sufficient && i + 1 + MIN <= end {
                let next = self.find(d, i + 1, end, true, rep, cfg);
                self.insert_one(d, i + 1);
                self.next = self.next.max(i + 2);
                if next.0 <= cur.0 + 6 {
                    break;
                }
                i += 1;
                cur = next;
                steps += 1;
            }
            let (_, off, mut len) = cur;
            while i > anchor && off < i && d[i - 1] == d[i - off - 1] {
                i -= 1;
                len += 1;
            }
            let ll = i - anchor;
            lits.extend_from_slice(&d[anchor..i]);
            let rep_code = apply_rep(rep, ll, off);
            seqs.push(Seq {
                ll: ll as u32,
                ml: len as u32,
                off: off as u32,
                rep_code,
            });
            let stop = i + len;
            let stride = (len / (cfg.depth.ilog2() as usize + 1)).max(1);
            let mut p = self.next.max(i + 2);
            while p < stop && p + MIN <= end {
                self.insert_one(d, p);
                p += stride;
            }
            self.next = self.next.max(stop);
            i = stop;
            anchor = i;
        }
        anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<P: Position>() -> Vec<Seq> {
        let mut state = 0x123456789abcdefu64;
        let first: Vec<u8> = (0..100_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect();
        let data = first.repeat(4);
        let budget = Budget {
            tagged: false,
            hash_bits: 14,
            window: 1 << 19,
            depth: 27,
            local_span: 16 << 10,
        };
        let cfg = ChainCfg::from_budget(&budget, 9, usize::MAX);
        let mut chain = Chain::<P>::new(&budget);
        chain.prepare(data.len(), &budget, cfg.bt);
        let (mut seqs, mut lits, mut rep) = (Vec::new(), Vec::new(), [1, 4, 8]);
        for start in (0..data.len()).step_by(128 << 10) {
            let end = (start + (128 << 10)).min(data.len());
            let anchor = chain.parse(&data, (start, end), cfg, &mut rep, &mut seqs, &mut lits);
            lits.extend_from_slice(&data[anchor..end]);
        }
        assert!(seqs.iter().any(|s| s.off > 65536));
        assert!(lits.len() < first.len() + 5000);
        assert_eq!(
            lits.len() + seqs.iter().map(|s| s.ml as usize).sum::<usize>(),
            data.len()
        );
        seqs
    }

    #[test]
    fn long_history_compact_and_wide_positions() {
        assert_eq!(check::<u32>(), check::<usize>());
    }
}
