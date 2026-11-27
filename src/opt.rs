//! 熵引导最优解析器（Entropy-Guided Optimal Parser，L16..=L22）：
//!
//! 参考文献与理论基础：
//! - Pavlov (2001) / Collet (2016)：后缀二叉搜索树（BST）子树拼接与按偏移严格递增的帕累托前沿枚举；
//! - Alakuijala & Vandevenne (2013), *Zopfli* & Collet (2018), *Zstd btultra*：
//!   以 1/8 bit 定点真实熵代价（字面量 + LL + ML + OF 符号代价与额外比特）驱动最短路径 DP，
//!   并在块内/跨块执行在线贝叶斯平滑与指数滑动平均（EMA）自适应更新；
//! - 帕累托支配单次弛豫（Pareto Dominance Relaxation）：按严格递增偏移/长度排序后，
//!   每个匹配长度仅被最小偏移候选弛豫一次，复杂度 $O(L_{\max})$ 且完整覆盖 $[3, L_{\max}]$；
//! - 势能望远镜求和（Telescoping Potential）：在 `OptNode.cost` 中实时维护当前未闭合字面量游程的
//!   `LL` 熵代价，使 DP 中不同 `litlen` 路径完全可比，并严格追踪 Zstd 3-rep 状态机。
use crate::dec::{LL_DEFAULT, ML_DEFAULT, OF_DEFAULT};
use crate::enc::{Seq, ll_code, ml_code};
use crate::henc::log2_fp8;
use crate::lz::{MIN, apply_rep, h_long, mlen, rd, rd4};

const PRIME4: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME3: u32 = 0x1E35_A7BD;
const HASH3_BITS: u32 = 14;
const HASH3_SIZE: usize = 1 << HASH3_BITS;
const HASH3_MASK: usize = HASH3_SIZE - 1;
/// 3 字节短匹配信息论有效偏移上限（超过 4 KiB 时 3B 序列普遍劣于 3 字面量）。
const HASH3_MAX_OFF: usize = 4096;

/// L2 缓存驻留的片段 DP 窗口长度（4096 + 512 节点 × 28 B ≈ 126 KiB < 256 KiB L2）。
const OPT_CHUNK: usize = 4096;
const MAX_SUFFICIENT: usize = 512;
const DP_CAP: usize = OPT_CHUNK + MAX_SUFFICIENT + 2;

#[inline(always)]
fn h4(v32: u32, bits: u32) -> usize {
    (((v32 as u64).wrapping_mul(PRIME4)) >> (64 - bits)) as usize
}

#[inline(always)]
fn h3(v24: u32) -> usize {
    ((v24.wrapping_mul(PRIME3)) >> (32 - HASH3_BITS)) as usize & HASH3_MASK
}

#[inline(always)]
fn apply_rep_u32(rep: &mut [u32; 3], has_ll: bool, off: u32) -> u8 {
    if has_ll {
        if off == rep[0] {
            return 1;
        }
        if off == rep[1] {
            rep.swap(0, 1);
            return 2;
        }
        if off == rep[2] {
            let r = rep[2];
            rep[2] = rep[1];
            rep[1] = rep[0];
            rep[0] = r;
            return 3;
        }
    } else {
        if off == rep[1] {
            rep.swap(0, 1);
            return 1;
        }
        if off == rep[2] {
            let r = rep[2];
            rep[2] = rep[1];
            rep[1] = rep[0];
            rep[0] = r;
            return 2;
        }
        if rep[0] > 1 && off == rep[0] - 1 {
            rep[2] = rep[1];
            rep[1] = rep[0];
            rep[0] = off;
            return 3;
        }
    }
    rep[2] = rep[1];
    rep[1] = rep[0];
    rep[0] = off;
    0
}

/// 1/8 比特（Q3）定点自适应熵代价模型：零浮点，L1d 驻留（< 2 KiB）。
pub(crate) struct OptStats {
    lit_freq: [u32; 256],
    ll_freq: [u32; 36],
    ml_freq: [u32; 53],
    of_freq: [u32; 32],
    lit_cost: [u16; 256],
    ll_cost: [u16; 36],
    ml_cost: [u16; 53],
    of_cost: [u16; 32],
    /// 预计算短 `LL` (0..64) 与 `ML` (0..=515) 的完整 1/8 bit 总代价（符号 + 额外比特）。
    ll_total_small: [u16; 64],
    ll_delta_small: [i16; 63],
    ml_total_small: [u16; MAX_SUFFICIENT + 4],
    ll0_cost: u32,
    seq_obs: u32,
    has_seq_obs: bool,
}

impl OptStats {
    fn new() -> Self {
        let mut s = Self {
            lit_freq: [1; 256],
            ll_freq: [1; 36],
            ml_freq: [1; 53],
            of_freq: [1; 32],
            lit_cost: [48; 256],
            ll_cost: [32; 36],
            ml_cost: [32; 53],
            of_cost: [32; 32],
            ll_total_small: [32; 64],
            ll_delta_small: [0; 63],
            ml_total_small: [32; MAX_SUFFICIENT + 4],
            ll0_cost: 16,
            seq_obs: 0,
            has_seq_obs: false,
        };
        s.reset_frame();
        s
    }

    pub fn reset_frame(&mut self) {
        self.lit_freq.fill(1);
        for (dst, &p) in self.ll_freq.iter_mut().zip(LL_DEFAULT.iter()) {
            *dst = if p == -1 { 1 } else { (p.max(0) as u32) * 2 }.max(1);
        }
        for (dst, &p) in self.ml_freq.iter_mut().zip(ML_DEFAULT.iter()) {
            *dst = if p == -1 { 1 } else { (p.max(0) as u32) * 2 }.max(1);
        }
        self.of_freq.fill(1);
        for (dst, &p) in self.of_freq.iter_mut().zip(OF_DEFAULT.iter()) {
            *dst = if p == -1 { 1 } else { (p.max(0) as u32) * 2 }.max(1);
        }
        self.seq_obs = 0;
        self.has_seq_obs = false;
        self.rebuild_costs();
    }

    pub fn begin_block(&mut self, blk: &[u8]) {
        if !self.has_seq_obs {
            // 首块使用精确字节直方图初始化字面量熵代价
            self.lit_freq.fill(0);
            for &b in blk {
                self.lit_freq[b as usize] += 1;
            }
        } else {
            // 后续块采用指数滑动平均衰减（>>= 1），平滑追踪非平稳信源
            for f in &mut self.lit_freq {
                *f = (*f >> 1).max(1);
            }
            for f in &mut self.ll_freq {
                *f = (*f >> 1).max(1);
            }
            for f in &mut self.ml_freq {
                *f = (*f >> 1).max(1);
            }
            for f in &mut self.of_freq {
                *f = (*f >> 1).max(1);
            }
            // 混入新块 1/4 字节采样，防止源分布突变（如 mozilla 文本切二进制）
            for &b in blk.iter().step_by(4) {
                self.lit_freq[b as usize] += 1;
            }
        }
        self.seq_obs = 0;
        self.rebuild_costs();
    }

    #[inline(always)]
    fn observe_seq(&mut self, ll: u32, ml: u32, off: u32, rep_code: u8) {
        let (lc, _, _) = ll_code(ll);
        let (mc, _, _) = ml_code(ml);
        let ofv = if rep_code > 0 {
            rep_code as u32
        } else {
            off + 3
        };
        let oc = (31 - ofv.leading_zeros()).min(31);
        self.ll_freq[lc as usize] += 2;
        self.ml_freq[mc as usize] += 2;
        self.of_freq[oc as usize] += 2;
        self.seq_obs += 1;
        self.has_seq_obs = true;
    }

    #[inline(always)]
    fn observe_lits(&mut self, lits: &[u8]) {
        for &b in lits {
            self.lit_freq[b as usize] += 2;
        }
    }

    #[inline]
    fn maybe_refresh(&mut self) {
        if self.seq_obs >= 256 {
            self.seq_obs = 0;
            self.rebuild_costs();
        }
    }

    fn rebuild_table(freq: &[u32], out: &mut [u16], max_q3: u32) {
        let total: u32 = freq.iter().copied().sum::<u32>().max(1);
        let log_tot = log2_fp8(total);
        for (dst, &c) in out.iter_mut().zip(freq.iter()) {
            let log_c = log2_fp8(c.max(1));
            // log2_fp8 为 8.8 定点（256 = 1 bit），右移 5 位得 1/8 bit（8 = 1 bit）
            let q3 = (log_tot.saturating_sub(log_c) >> 5).clamp(4, max_q3);
            *dst = q3 as u16;
        }
    }

    fn rebuild_costs(&mut self) {
        Self::rebuild_table(&self.lit_freq, &mut self.lit_cost, 96);
        Self::rebuild_table(&self.ll_freq, &mut self.ll_cost, 88);
        Self::rebuild_table(&self.ml_freq, &mut self.ml_cost, 88);
        Self::rebuild_table(&self.of_freq, &mut self.of_cost, 80);

        for ll in 0..64 {
            let (c, eb, _) = ll_code(ll as u32);
            self.ll_total_small[ll] = self.ll_cost[c as usize] + ((eb as u16) << 3);
        }
        for ll in 0..63 {
            self.ll_delta_small[ll] =
                self.ll_total_small[ll + 1] as i16 - self.ll_total_small[ll] as i16;
        }
        self.ll0_cost = self.ll_total_small[0] as u32;

        for ml in 3..self.ml_total_small.len() {
            let (c, eb, _) = ml_code(ml as u32);
            self.ml_total_small[ml] = self.ml_cost[c as usize] + ((eb as u16) << 3);
        }
    }

    #[inline(always)]
    fn ll_total_cost(&self, ll: usize) -> u32 {
        if ll < 64 {
            self.ll_total_small[ll] as u32
        } else {
            let (c, eb, _) = ll_code(ll as u32);
            self.ll_cost[c as usize] as u32 + (eb << 3)
        }
    }

    #[inline(always)]
    fn ll_delta(&self, ll: usize) -> i32 {
        if ll < 63 {
            self.ll_delta_small[ll] as i32
        } else {
            self.ll_total_cost(ll + 1) as i32 - self.ll_total_cost(ll) as i32
        }
    }

    #[inline(always)]
    fn ml_total_cost(&self, ml: usize) -> u32 {
        if ml < self.ml_total_small.len() {
            self.ml_total_small[ml] as u32
        } else {
            let (c, eb, _) = ml_code(ml as u32);
            self.ml_cost[c as usize] as u32 + (eb << 3)
        }
    }

    #[inline(always)]
    fn of_rep_cost(&self, rep_code: u8) -> u32 {
        if rep_code == 1 {
            self.of_cost[0] as u32
        } else {
            self.of_cost[1] as u32 + 8
        }
    }

    #[inline(always)]
    fn of_total_cost(&self, off: usize, rep_code: u8) -> u32 {
        if rep_code > 0 {
            self.of_rep_cost(rep_code)
        } else {
            let ofv = (off as u32).saturating_add(3);
            let oc = (31 - ofv.leading_zeros()).min(31);
            self.of_cost[oc as usize] as u32 + (oc << 3)
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct OptCfg {
    pub depth: usize,
    pub sufficient: usize,
    pub hash_bits: u32,
    pub window: usize,
    pub window_limit: usize,
}

impl OptCfg {
    pub fn for_level(
        level: i32,
        data_len: usize,
        budget_window: usize,
        window_limit: usize,
    ) -> Self {
        let (depth, sufficient, max_hbits, max_win) = match level {
            ..=16 => (24usize, 96usize, 17u32, 4usize << 20),
            17..=18 => (36, 128, 18, 8 << 20),
            19 => (48, 128, 18, 8 << 20),
            _ => (96, 256, 19, 16 << 20),
        };
        let eff_limit = window_limit.min(max_win).min((u32::MAX - 3) as usize);
        let want_win = data_len
            .max(1024)
            .next_power_of_two()
            .min(budget_window.max(1024))
            .min(eff_limit.next_power_of_two())
            .min(max_win);
        let data_bits = (usize::BITS - 1 - data_len.max(1024).leading_zeros()).clamp(10, max_hbits);
        Self {
            depth,
            sufficient: sufficient.min(MAX_SUFFICIENT),
            hash_bits: data_bits,
            window: want_win.max(1024).next_power_of_two(),
            window_limit: eff_limit,
        }
    }
}

#[derive(Clone, Copy)]
struct OptNode {
    cost: u32,
    off: u32,
    rep: [u32; 3],
    prev: u16,
    mlen: u16,
    litlen: u16,
}

impl OptNode {
    const INF: Self = Self {
        cost: u32::MAX,
        off: 0,
        rep: [1, 4, 8],
        prev: 0,
        mlen: 0,
        litlen: 0,
    };
}

pub(crate) struct OptParser {
    head: Vec<u32>,
    long: Vec<u32>,
    hash3: Vec<u32>,
    prev: Vec<u32>,
    max_bits: u32,
    active_bits: u32,
    win_mask: usize,
    base: usize,
    next_base: usize,
    next_insert: usize,
    dp: Vec<OptNode>,
    back_ops: Vec<(u16, u32)>,
    stats: OptStats,
}

impl OptParser {
    pub fn new() -> Self {
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
            dp: vec![OptNode::INF; DP_CAP],
            back_ops: Vec::with_capacity(512),
            stats: OptStats::new(),
        }
    }

    pub fn prepare(&mut self, data_len: usize, cfg: OptCfg) {
        let want_bits = cfg.hash_bits.max(10);
        let want_win = cfg.window.max(1024).next_power_of_two();
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
        self.stats.reset_frame();
    }

    /// 后缀 BST 插入 + 帕累托匹配枚举（Hash3 3B -> BST 4B+ 偏移递增 -> Long 7B）。
    ///
    /// 关键不变量：
    /// 1. 字典序比较始终基于完整帧 `d`，块内返回长度钳制到 `end - p`，保证跨块 BST 严格有序；
    /// 2. 命中 `ml >= sufficient` 时精确拼接 `c` 的左右子树（Pavlov/Collet），不丢弃任何深层分支；
    /// 3. 返回的 `cands[..nc]` 按 `ml` 严格递增，可直接执行单次帕累托支配弛豫。
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

        // 1) Hash3：捕获近距 3 字节匹配（与 BST 的 4B+ 完全互补）
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

        // 2) BST 4B+ 帕累托枚举（按偏移严格递增遍历，绝不被 7B 长表遮蔽）
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

        // 3) 7B 长表置于 BST 之后，仅补充因搜索深度受限未触及的超长匹配
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
    fn probe_reps3(
        d: &[u8],
        pos: usize,
        end: usize,
        has_ll: bool,
        win: usize,
        rep: [u32; 3],
        out: &mut [(usize, u32, u8); 3],
    ) -> (usize, usize) {
        if pos + 3 > end {
            return (0, 0);
        }
        let list: [(u32, u8); 3] = if has_ll {
            [(rep[0], 1), (rep[1], 2), (rep[2], 3)]
        } else {
            [(rep[1], 1), (rep[2], 2), (rep[0].saturating_sub(1), 3)]
        };
        let mut n = 0usize;
        let mut max_ml = 0usize;
        if pos + 4 <= end {
            let v4 = rd4(d, pos);
            let v3 = v4 & 0x00FF_FFFF;
            for &(ro, rcode) in &list {
                let ro_u = ro as usize;
                if ro_u >= 1 && ro_u <= pos && ro_u <= win {
                    let c = pos - ro_u;
                    let cv4 = rd4(d, c);
                    if cv4 == v4 {
                        let ml = 4 + mlen(d, c + 4, pos + 4, end);
                        out[n] = (ml, ro, rcode);
                        n += 1;
                        if ml > max_ml {
                            max_ml = ml;
                        }
                    } else if (cv4 & 0x00FF_FFFF) == v3 {
                        out[n] = (3, ro, rcode);
                        n += 1;
                        if 3 > max_ml {
                            max_ml = 3;
                        }
                    }
                }
            }
        } else {
            for &(ro, rcode) in &list {
                let ro_u = ro as usize;
                if ro_u >= 1 && ro_u <= pos && ro_u <= win {
                    let c = pos - ro_u;
                    if d[c] == d[pos] && d[c + 1] == d[pos + 1] && d[c + 2] == d[pos + 2] {
                        out[n] = (3, ro, rcode);
                        n += 1;
                        max_ml = 3;
                    }
                }
            }
        }
        (n, max_ml)
    }

    #[inline(always)]
    fn insert_match_interior(
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

    /// 对单个块 `[start, end)` 运行 L2 驻留片段式熵最优 DP 解析。
    #[inline(never)]
    pub fn parse(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        cfg: OptCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if end > d.len() || end.saturating_sub(start) < 3 {
            return start;
        }
        self.stats.begin_block(&d[start..end]);
        let win = self.win_mask.min(cfg.window_limit);
        let mut dp_buf = std::mem::take(&mut self.dp);
        if dp_buf.len() < DP_CAP {
            dp_buf.resize(DP_CAP, OptNode::INF);
        }

        let mut cands = [(0usize, 0usize); 32];
        let mut rep_cands = [(0usize, 0u32, 0u8); 3];
        let mut cur = start;
        let mut anchor = start;

        while cur + 3 <= end {
            // 1) 在 cur 探测首步候选：无匹配时直接零 DP 推进字面量
            let cur_rep32 = [rep[0] as u32, rep[1] as u32, rep[2] as u32];
            let has_ll0 = cur > anchor;
            let (nrep0, rep_max0) =
                Self::probe_reps3(d, cur, end, has_ll0, win, cur_rep32, &mut rep_cands);
            let mut nc0 = 0usize;
            let mut bst_max0 = 0usize;
            if cur >= self.next_insert {
                self.bt_step::<true>(
                    d,
                    cur,
                    end,
                    cfg.depth,
                    cfg.sufficient,
                    cfg.window_limit,
                    &mut cands,
                    &mut nc0,
                    &mut bst_max0,
                );
            }
            let max0 = rep_max0.max(bst_max0);
            if max0 < 3 {
                cur += 1;
                continue;
            }

            // 超长匹配（>= sufficient）：立即发射并稀疏补插 BST，避免进入 DP
            if max0 >= cfg.sufficient {
                let (best_ml, best_off) = if rep_max0 >= bst_max0 {
                    let &(ml, ro, _) = rep_cands[..nrep0]
                        .iter()
                        .max_by_key(|x| x.0)
                        .expect("nonempty");
                    (ml, ro as usize)
                } else {
                    cands[nc0 - 1]
                };
                let ll = cur - anchor;
                if ll > 0 {
                    let ls = &d[anchor..cur];
                    lits.extend_from_slice(ls);
                    self.stats.observe_lits(ls);
                }
                let rc = apply_rep(rep, ll, best_off);
                seqs.push(Seq {
                    ll: ll as u32,
                    ml: best_ml as u32,
                    off: best_off as u32,
                    rep_code: rc,
                });
                self.stats
                    .observe_seq(ll as u32, best_ml as u32, best_off as u32, rc);
                self.stats.maybe_refresh();
                let stop = cur + best_ml;
                self.insert_match_interior(d, cur + 1, stop, end, cfg.sufficient, cfg.window_limit);
                cur = stop;
                anchor = stop;
                continue;
            }

            // 2) 启动 L1d/L2 驻留片段最优 DP（从 cur 起，在自然切点 last_pos 或 OPT_CHUNK 结束）
            let init_ll = (cur - anchor).min(u16::MAX as usize);
            dp_buf[0] = OptNode {
                cost: self.stats.ll_total_cost(init_ll),
                off: 0,
                rep: cur_rep32,
                prev: 0,
                mlen: 0,
                litlen: init_ll as u16,
            };
            let mut cleared_to = 0usize;
            let mut last_pos = max0;
            let mut forced_tail: Option<(usize, usize, usize)> = None;

            macro_rules! ensure_cleared {
                ($upto:expr) => {{
                    let target = $upto;
                    if target > cleared_to {
                        dp_buf[cleared_to + 1..=target].fill(OptNode::INF);
                        cleared_to = target;
                    }
                }};
            }

            ensure_cleared!(last_pos);

            // 在 idx = 0 复用已收集的首步候选（避免重复搜索 BST）
            let mut idx = 0usize;
            loop {
                let pos = cur + idx;
                let cur_node = dp_buf[idx];

                // 字面量转移 idx -> idx + 1（望远镜势能精确计入 LL 增量代价）
                if pos < end {
                    ensure_cleared!(idx + 1);
                    let ll = cur_node.litlen as usize;
                    let delta_ll = self.stats.ll_delta(ll);
                    let lit_c = self.stats.lit_cost[d[pos] as usize] as i32;
                    let next_cost = (cur_node.cost as i32 + lit_c + delta_ll)
                        .max(cur_node.cost as i32 + 1) as u32;
                    if next_cost < dp_buf[idx + 1].cost {
                        dp_buf[idx + 1] = OptNode {
                            cost: next_cost,
                            off: 0,
                            rep: cur_node.rep,
                            prev: idx as u16,
                            mlen: 0,
                            litlen: (ll + 1).min(u16::MAX as usize) as u16,
                        };
                    }
                }

                // 收集当前位置候选（idx == 0 直接复用，idx > 0 现场查 rep + BST）
                let (nrep, rep_max, nc, bst_max) = if idx == 0 {
                    (nrep0, rep_max0, nc0, bst_max0)
                } else if pos + 3 <= end {
                    let has_ll = cur_node.litlen > 0;
                    let (nr, rm) =
                        Self::probe_reps3(d, pos, end, has_ll, win, cur_node.rep, &mut rep_cands);
                    let mut n_bst = 0usize;
                    let mut b_max = 0usize;
                    if pos >= self.next_insert {
                        self.bt_step::<true>(
                            d,
                            pos,
                            end,
                            cfg.depth,
                            cfg.sufficient,
                            cfg.window_limit,
                            &mut cands,
                            &mut n_bst,
                            &mut b_max,
                        );
                    }
                    (nr, rm, n_bst, b_max)
                } else {
                    (0, 0, 0, 0)
                };

                let max_here = rep_max.max(bst_max);
                if max_here >= cfg.sufficient {
                    let (best_ml, best_off) = if rep_max >= bst_max {
                        let &(ml, ro, _) = rep_cands[..nrep]
                            .iter()
                            .max_by_key(|x| x.0)
                            .expect("nonempty");
                        (ml, ro as usize)
                    } else {
                        cands[nc - 1]
                    };
                    forced_tail = Some((idx, best_ml, best_off));
                    break;
                }

                if max_here >= 3 {
                    let reach = idx + max_here;
                    ensure_cleared!(reach);
                    if reach > last_pos {
                        last_pos = reach;
                    }
                    let has_ll = cur_node.litlen > 0;
                    let ll0 = self.stats.ll0_cost;

                    // A) 弛豫重复偏移候选（<= 3 个）
                    let mut rep1_ml = 0usize;
                    for &(rml, roff, rcode) in &rep_cands[..nrep] {
                        let mut next_rep = cur_node.rep;
                        let _ = apply_rep_u32(&mut next_rep, has_ll, roff);
                        let base_c = cur_node.cost + self.stats.of_rep_cost(rcode) + ll0;
                        if rcode == 1 && rml > rep1_ml {
                            rep1_ml = rml;
                        }
                        for len in 3..=rml {
                            let dst = idx + len;
                            let sc = base_c + self.stats.ml_total_cost(len);
                            if sc < dp_buf[dst].cost {
                                dp_buf[dst] = OptNode {
                                    cost: sc,
                                    off: roff,
                                    rep: next_rep,
                                    prev: idx as u16,
                                    mlen: len as u16,
                                    litlen: 0,
                                };
                            }
                        }
                    }

                    // B) 帕累托支配单次弛豫：cands[..nc] 按长度与偏移严格递增，每长度仅弛豫一次
                    // rep1 偏移代价严格最低，非重复候选无需重复弛豫 <= rep1_ml 的前缀长度
                    let mut start_len = (rep1_ml + 1).max(3);
                    for &(cand_ml, off) in &cands[..nc] {
                        if cand_ml < start_len {
                            continue;
                        }
                        let mut next_rep = cur_node.rep;
                        let rc = apply_rep_u32(&mut next_rep, has_ll, off as u32);
                        let base_c = cur_node.cost + self.stats.of_total_cost(off, rc) + ll0;
                        for len in start_len..=cand_ml {
                            let dst = idx + len;
                            let sc = base_c + self.stats.ml_total_cost(len);
                            if sc < dp_buf[dst].cost {
                                dp_buf[dst] = OptNode {
                                    cost: sc,
                                    off: off as u32,
                                    rep: next_rep,
                                    prev: idx as u16,
                                    mlen: len as u16,
                                    litlen: 0,
                                };
                            }
                        }
                        start_len = cand_ml + 1;
                    }
                }

                idx += 1;
                if idx >= last_pos || idx >= OPT_CHUNK || cur + idx >= end {
                    break;
                }
            }

            // 3) 回溯发射本片段最优操作序列
            let end_idx = match forced_tail {
                Some((t_idx, _, _)) => t_idx,
                None => last_pos.min(end - cur),
            };
            self.back_ops.clear();
            let mut bt = end_idx;
            while bt > 0 {
                let node = dp_buf[bt];
                self.back_ops.push((node.mlen, node.off));
                bt = node.prev as usize;
            }
            let mut pos = cur;
            for &(ml, off) in self.back_ops.iter().rev() {
                if ml == 0 {
                    pos += 1;
                } else {
                    let ll = pos - anchor;
                    if ll > 0 {
                        let ls = &d[anchor..pos];
                        lits.extend_from_slice(ls);
                        self.stats.observe_lits(ls);
                    }
                    let rc = apply_rep(rep, ll, off as usize);
                    seqs.push(Seq {
                        ll: ll as u32,
                        ml: ml as u32,
                        off,
                        rep_code: rc,
                    });
                    self.stats.observe_seq(ll as u32, ml as u32, off, rc);
                    pos += ml as usize;
                    anchor = pos;
                }
            }
            debug_assert_eq!(pos, cur + end_idx);

            if let Some((_, big_ml, big_off)) = forced_tail {
                let ll = pos - anchor;
                if ll > 0 {
                    let ls = &d[anchor..pos];
                    lits.extend_from_slice(ls);
                    self.stats.observe_lits(ls);
                }
                let rc = apply_rep(rep, ll, big_off);
                seqs.push(Seq {
                    ll: ll as u32,
                    ml: big_ml as u32,
                    off: big_off as u32,
                    rep_code: rc,
                });
                self.stats
                    .observe_seq(ll as u32, big_ml as u32, big_off as u32, rc);
                let stop = pos + big_ml;
                self.insert_match_interior(d, pos + 1, stop, end, cfg.sufficient, cfg.window_limit);
                cur = stop;
                anchor = stop;
            } else {
                let stop = cur + end_idx;
                if stop > self.next_insert {
                    self.insert_match_interior(
                        d,
                        self.next_insert,
                        stop,
                        end,
                        cfg.sufficient,
                        cfg.window_limit,
                    );
                }
                cur = stop;
            }
            self.stats.maybe_refresh();
        }

        self.dp = dp_buf;
        anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_stats_telescoping_ll_potential_is_exact() {
        let mut stats = OptStats::new();
        stats.begin_block(b"aaabbbcccddd0123456789abcdefghijklmnopqrstuvwxyz");
        for l0 in [0usize, 1, 5, 16, 62, 63, 64, 100] {
            let mut acc = stats.ll_total_cost(l0) as i32;
            for step in 0..80 {
                acc += stats.ll_delta(l0 + step);
            }
            assert_eq!(
                acc as u32,
                stats.ll_total_cost(l0 + 80),
                "telescoping LL mismatch for l0={l0}"
            );
        }
    }

    #[test]
    fn bst_splicing_preserves_exact_matches_with_long_shared_prefixes() {
        // 构造大量共享 70..300B 公共前缀但尾部不同字节的记录，覆盖 interior 插入与主搜索共享 sufficient 阈值的不变量
        let mut data = Vec::new();
        let stem = [b'A'; 180];
        for i in 0..400u32 {
            data.extend_from_slice(&stem[..(68 + (i as usize * 37) % 112)]);
            data.push(b'0' + (i % 10) as u8);
            data.push(b'a' + ((i / 10) % 26) as u8);
            data.extend_from_slice(&stem[..40]);
            data.push(b'Z' - (i % 20) as u8);
        }
        for &lvl in &[12, 16, 19] {
            let z = crate::compress(&data, lvl);
            let dec = crate::decompress(&z, data.len()).unwrap();
            assert_eq!(dec, data, "BST splicing roundtrip failed at L{lvl}");
        }
    }

    #[test]
    fn opt_parser_handles_u32_next_base_wraparound_and_tiny_window() {
        let mut op = OptParser::new();
        let data = b"abcde12345abcde67890_opt_wrap_".repeat(200);
        let cfg = OptCfg::for_level(19, data.len(), 1024, 1024);
        op.prepare(data.len(), cfg);
        let (mut seqs, mut lits, mut rep) = (Vec::new(), Vec::new(), [1, 4, 8]);
        let a1 = op.parse(&data, (0, 1024), cfg, &mut rep, &mut seqs, &mut lits);
        lits.extend_from_slice(&data[a1..1024]);
        assert_eq!(
            lits.len() + seqs.iter().map(|s| s.ml as usize).sum::<usize>(),
            1024
        );
        assert!(seqs.iter().all(|s| (s.off as usize) <= 1024));

        // 模拟 next_base 接近 u32::MAX 回绕
        op.next_base = u32::MAX as usize - 16;
        op.prepare(data.len(), cfg);
        assert_eq!(op.base, 0);
        seqs.clear();
        lits.clear();
        rep = [1, 4, 8];
        let mut pos = 0;
        while pos < data.len() {
            let end = (pos + 1024).min(data.len());
            let a = op.parse(&data, (pos, end), cfg, &mut rep, &mut seqs, &mut lits);
            lits.extend_from_slice(&data[a..end]);
            pos = end;
        }
        assert_eq!(
            lits.len() + seqs.iter().map(|s| s.ml as usize).sum::<usize>(),
            data.len()
        );
        assert!(seqs.iter().all(|s| (s.off as usize) <= 1024));
    }
}
