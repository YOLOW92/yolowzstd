//! 哈希链/二叉树匹配器与前向启发式动态规划解析器：
//! - level 5..=8：浅链 + 1 步前瞻
//! - level 9..=11：深链 + 2 步前瞻
//! - level 12..=15：二叉树后缀匹配器 + 前瞻
//! - level 16..=22：二叉树后缀匹配器 + 块内近似比特代价前向启发式 DP（Zstd 3-rep）
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
    pub optimal: bool,
    pub window_limit: usize,
}

impl ChainCfg {
    pub fn from_budget(b: &Budget, level: i32, window_limit: usize) -> Self {
        let sufficient = match level {
            ..=8 => 64,
            9..=11 => 96,
            12..=15 => 256,
            _ => 256,
        };
        let bt = level >= 12;
        let depth = match level {
            ..=8 => b.depth,
            9..=11 => 16,
            12..=15 => 24,
            16..=18 => 48,
            _ => 64,
        };
        let lazy_steps = if level >= 9 { 2 } else { 1 };
        Self {
            depth,
            sufficient,
            lazy_steps,
            bt,
            optimal: level >= 16,
            window_limit: window_limit.min((u32::MAX - 3) as usize),
        }
    }
}

#[derive(Clone, Copy)]
struct DpNode {
    cost: u32,
    prev: u32,
    mlen: u32,
    off: u32,
    rep_code: u8,
    last_lit: bool,
    rep: [usize; 3],
}

impl DpNode {
    const INF: Self = Self {
        cost: u32::MAX,
        prev: 0,
        mlen: 0,
        off: 0,
        rep_code: 0,
        last_lit: false,
        rep: [1, 4, 8],
    };
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
    dp: Vec<DpNode>,
    dp_idle: u8,
    back_ops: Vec<(u32, u32, u8)>, // (pos_or_lit_len, off, rep_code) 逆序复用缓冲
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
            dp: Vec::new(),
            dp_idle: 0,
            back_ops: Vec::new(),
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
        if cfg.optimal && range.1 - range.0 >= 64 {
            self.dp_idle = 0;
            self.parse_opt(d, range, cfg, rep, seqs, lits)
        } else {
            if self.dp.capacity() > 0 {
                self.dp_idle = self.dp_idle.saturating_add(1);
                if self.dp_idle >= 4 {
                    self.dp = Vec::new();
                    self.dp_idle = 0;
                }
            }
            if cfg.bt {
                self.parse_bt_lazy(d, range, cfg, rep, seqs, lits)
            } else {
                self.parse_lazy(d, range, cfg, rep, seqs, lits)
            }
        }
    }

    /// 将位置 `p` 插入二叉搜索树并同步收集后缀匹配（`< p` 的严格左/右子树，无自环与越界风险）。
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
        let base = self.base;
        let bits = self.active_bits;
        let wmask = self.win_mask;
        let win_cap = wmask + 1;
        let new_pv = P::from_raw(base + p + 1);
        if p + 8 <= end {
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
                return;
            }
            let c = raw - base - 1;
            if c >= p || p - c > win_limit || p - c >= win_cap {
                self.prev[smaller_slot] = P::EMPTY;
                self.prev[larger_slot] = P::EMPTY;
                return;
            }
            let m_common = common_smaller.min(common_larger);
            let ml = if m_common == 0 && c + 8 <= end && p + 8 <= end {
                let diff = rd(d, c) ^ rd(d, p);
                if diff != 0 {
                    (diff.trailing_zeros() >> 3) as usize
                } else {
                    8 + mlen(d, c + 8, p + 8, end)
                }
            } else {
                m_common + mlen(d, c + m_common, p + m_common, end)
            };
            if COLLECT && ml >= MIN && ml > *max_ml && *nc < cands.len() {
                *max_ml = ml;
                cands[*nc] = (ml, p - c, 0);
                *nc += 1;
            }
            if p + ml >= end || ml >= sufficient {
                self.prev[smaller_slot] = P::EMPTY;
                self.prev[larger_slot] = P::EMPTY;
                return;
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
        self.prev[smaller_slot] = P::EMPTY;
        self.prev[larger_slot] = P::EMPTY;
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
        let d = &d[..end];
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
                    64,
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

    /// 块内前向启发式比特代价 DP 解析（近似估算字面量与序列比特代价并追踪 3 个重复偏移，匹配段超过 `sufficient` 时直接跨跳）。
    #[inline(never)]
    fn parse_opt(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        cfg: ChainCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        let n = end - start;
        let blk = &d[start..end];
        let mut freq = [0u32; 256];
        for &b in blk {
            freq[b as usize] += 1;
        }
        let mut lit_cost = [24u32; 256]; // 以 1/4 比特为单位
        let nf = n as f32;
        for (s, &c) in freq.iter().enumerate() {
            let bits = (nf / c.max(1) as f32).log2().clamp(1.0, 8.0);
            lit_cost[s] = (bits * 4.0).round() as u32;
        }

        let mut dp_buf = std::mem::take(&mut self.dp);
        if dp_buf.len() <= n {
            dp_buf.resize(n + 1, DpNode::INF);
        }
        let dp = &mut dp_buf[..=n];
        dp.fill(DpNode::INF);
        dp[0] = DpNode {
            cost: 0,
            prev: 0,
            mlen: 0,
            off: 0,
            rep_code: 0,
            last_lit: false,
            rep: *rep,
        };

        let win = (self.win_mask).min(cfg.window_limit);
        let _ = self.local_span;

        #[inline(always)]
        fn seq_cost(ml: usize, off: usize, rep_code: u8) -> u32 {
            let off_bits = match rep_code {
                1 => 4,
                2 => 10,
                3 => 14,
                _ => ((31 - (off as u32).saturating_add(3).leading_zeros()) << 2) + 8,
            };
            let ml_bits = if ml >= 43 {
                10
            } else if ml >= 19 {
                6
            } else {
                3
            };
            20 + ml_bits + off_bits
        }

        // 记录收集到的帕累托匹配候选 (mlen, off, rep_code)
        let mut cands = [(0usize, 0usize, 0u8); 24];
        let mut dummy = [(0usize, 0usize, 0u8); 1];
        let mut idx = 0usize;
        while idx < n {
            let cur_node = dp[idx];
            if cur_node.cost == u32::MAX {
                idx += 1;
                continue;
            }
            let pos = start + idx;

            // 1) 字面量转移 idx -> idx + 1
            let next_lit_cost = cur_node.cost + lit_cost[blk[idx] as usize];
            if next_lit_cost < dp[idx + 1].cost {
                dp[idx + 1] = DpNode {
                    cost: next_lit_cost,
                    prev: idx as u32,
                    mlen: 0,
                    off: 0,
                    rep_code: 0,
                    last_lit: true,
                    rep: cur_node.rep,
                };
            }

            if pos + MIN > end {
                idx += 1;
                continue;
            }

            // 2) 收集本地位置的所有有效重复与二叉搜索树帕累托匹配候选
            let mut nc = 0usize;
            let mut max_ml = 0usize;
            let rep_list: [(usize, u8); 3] = if cur_node.last_lit {
                [
                    (cur_node.rep[0], 1),
                    (cur_node.rep[1], 2),
                    (cur_node.rep[2], 3),
                ]
            } else {
                [
                    (cur_node.rep[1], 1),
                    (cur_node.rep[2], 2),
                    (cur_node.rep[0].saturating_sub(1), 3),
                ]
            };
            let v32 = rd4(d, pos);
            for &(ro, rcode) in &rep_list {
                if ro >= 1 && ro <= pos && ro <= win && rd4(d, pos - ro) == v32 {
                    let ml = 4 + mlen(d, pos - ro + 4, pos + 4, end);
                    cands[nc] = (ml, ro, rcode);
                    nc += 1;
                    if ml > max_ml {
                        max_ml = ml;
                    }
                }
            }
            if pos >= self.next {
                self.bt_step::<true>(
                    d,
                    pos,
                    end,
                    cfg.depth,
                    cfg.sufficient,
                    cfg.window_limit,
                    &mut cands,
                    &mut nc,
                    &mut max_ml,
                );
            }

            if nc == 0 {
                idx += 1;
                continue;
            }

            // 足够长的匹配（>= sufficient）：直接采用并跨越整个区间，避免 O(N^2) 弛豫。
            if max_ml >= cfg.sufficient && pos + max_ml <= end {
                let &(best_ml, best_off, _) =
                    cands[..nc].iter().max_by_key(|x| x.0).expect("nonempty");
                let mut next_rep = cur_node.rep;
                let rc = apply_rep(&mut next_rep, usize::from(cur_node.last_lit), best_off);
                let sc = cur_node.cost + seq_cost(best_ml, best_off, rc);
                let dst = idx + best_ml;
                if sc < dp[dst].cost {
                    dp[dst] = DpNode {
                        cost: sc,
                        prev: idx as u32,
                        mlen: best_ml as u32,
                        off: best_off as u32,
                        rep_code: rc,
                        last_lit: false,
                        rep: next_rep,
                    };
                }
                let stop_pos = pos + best_ml;
                let stride = (best_ml / 12).max(1);
                let mut p = self.next.max(pos + 1);
                while p + MIN <= end && p < stop_pos {
                    let mut dn = 0;
                    let mut dm = 0;
                    self.bt_step::<false>(
                        d,
                        p,
                        end,
                        8,
                        64,
                        cfg.window_limit,
                        &mut dummy,
                        &mut dn,
                        &mut dm,
                    );
                    p += stride;
                }
                self.next = self.next.max(stop_pos);
                idx = dst;
                continue;
            }

            // 按每个候选弛豫完整长度及短前缀（最多到 16），确保相邻匹配与重叠截断最优。
            for &(cand_ml, off, _) in &cands[..nc] {
                let mut next_rep = cur_node.rep;
                let rc = apply_rep(&mut next_rep, usize::from(cur_node.last_lit), off);
                let prefix_end = cand_ml.min(16);
                for len in MIN..=prefix_end {
                    let dst = idx + len;
                    let sc = cur_node.cost + seq_cost(len, off, rc);
                    if sc < dp[dst].cost {
                        dp[dst] = DpNode {
                            cost: sc,
                            prev: idx as u32,
                            mlen: len as u32,
                            off: off as u32,
                            rep_code: rc,
                            last_lit: false,
                            rep: next_rep,
                        };
                    }
                }
                if cand_ml > 16 {
                    let dst = idx + cand_ml;
                    let sc = cur_node.cost + seq_cost(cand_ml, off, rc);
                    if sc < dp[dst].cost {
                        dp[dst] = DpNode {
                            cost: sc,
                            prev: idx as u32,
                            mlen: cand_ml as u32,
                            off: off as u32,
                            rep_code: rc,
                            last_lit: false,
                            rep: next_rep,
                        };
                    }
                }
            }
            idx += 1;
        }

        // 回溯还原操作序列（可能末尾含若干字面量，留给调用方按 anchor..end 输出）
        self.back_ops.clear();
        let mut cur_idx = n;
        let mut trailing_lits = 0usize;
        while cur_idx > 0 && dp[cur_idx].mlen == 0 {
            trailing_lits += 1;
            cur_idx = dp[cur_idx].prev as usize;
        }
        let anchor = end - trailing_lits;
        while cur_idx > 0 {
            let node = dp[cur_idx];
            if node.mlen == 0 {
                self.back_ops.push((0, 0, 0));
            } else {
                self.back_ops.push((node.mlen, node.off, node.rep_code));
            }
            cur_idx = node.prev as usize;
        }
        self.dp = dp_buf;
        let mut pos = start;
        let mut cur_anchor = start;
        for &(ml, off, _) in self.back_ops.iter().rev() {
            if ml == 0 {
                pos += 1;
            } else {
                let ll = pos - cur_anchor;
                lits.extend_from_slice(&d[cur_anchor..pos]);
                let rc = apply_rep(rep, ll, off as usize);
                seqs.push(Seq {
                    ll: ll as u32,
                    ml,
                    off,
                    rep_code: rc,
                });
                pos += ml as usize;
                cur_anchor = pos;
            }
        }
        debug_assert_eq!(cur_anchor, anchor);
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
