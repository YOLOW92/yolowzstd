//! 1/8 比特（Q3）定点在线自适应熵代价模型：贝叶斯平滑、指数滑动平均（EMA）与望远镜 `LL` 势能表。
use crate::dec::{LL_DEFAULT, ML_DEFAULT, OF_DEFAULT};
use crate::enc::{ll_code, ml_code};
use crate::entropy::log2_fp8;

pub(super) const MAX_SUFFICIENT: usize = 512;

/// 1/8 比特（Q3）定点自适应熵代价模型：零浮点，L1d 驻留（< 2 KiB）。
pub(crate) struct OptStats {
    lit_freq: [u32; 256],
    ll_freq: [u32; 36],
    ml_freq: [u32; 53],
    of_freq: [u32; 32],
    pub(super) lit_cost: [u16; 256],
    ll_cost: [u16; 36],
    ml_cost: [u16; 53],
    of_cost: [u16; 32],
    /// 预计算短 `LL` (0..64) 与 `ML` (0..=515) 的完整 1/8 bit 总代价（符号 + 额外比特）。
    ll_total_small: [u16; 64],
    ll_delta_small: [i16; 63],
    ml_total_small: [u16; MAX_SUFFICIENT + 4],
    pub(super) ll0_cost: u32,
    seq_obs: u32,
    has_seq_obs: bool,
}

impl OptStats {
    pub(super) fn new() -> Self {
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

    pub(super) fn reset_frame(&mut self) {
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

    pub(super) fn begin_block(&mut self, blk: &[u8]) {
        if !self.has_seq_obs {
            self.lit_freq.fill(0);
            for &b in blk {
                self.lit_freq[b as usize] += 1;
            }
        } else {
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
            for &b in blk.iter().step_by(4) {
                self.lit_freq[b as usize] += 1;
            }
        }
        self.seq_obs = 0;
        self.rebuild_costs();
    }

    #[inline(always)]
    pub(super) fn observe_seq(&mut self, ll: u32, ml: u32, off: u32, rep_code: u8) {
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
    pub(super) fn observe_lits(&mut self, lits: &[u8]) {
        for &b in lits {
            self.lit_freq[b as usize] += 2;
        }
    }

    #[inline]
    pub(super) fn maybe_refresh(&mut self) {
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
    pub(super) fn ll_total_cost(&self, ll: usize) -> u32 {
        if ll < 64 {
            self.ll_total_small[ll] as u32
        } else {
            let (c, eb, _) = ll_code(ll as u32);
            self.ll_cost[c as usize] as u32 + (eb << 3)
        }
    }

    #[inline(always)]
    pub(super) fn ll_delta(&self, ll: usize) -> i32 {
        if ll < 63 {
            self.ll_delta_small[ll] as i32
        } else {
            self.ll_total_cost(ll + 1) as i32 - self.ll_total_cost(ll) as i32
        }
    }

    #[inline(always)]
    pub(super) fn ml_total_cost(&self, ml: usize) -> u32 {
        if ml < self.ml_total_small.len() {
            self.ml_total_small[ml] as u32
        } else {
            let (c, eb, _) = ml_code(ml as u32);
            self.ml_cost[c as usize] as u32 + (eb << 3)
        }
    }

    #[inline(always)]
    pub(super) fn of_rep_cost(&self, rep_code: u8) -> u32 {
        if rep_code == 1 {
            self.of_cost[0] as u32
        } else {
            self.of_cost[1] as u32 + 8
        }
    }

    #[inline(always)]
    pub(super) fn of_total_cost(&self, off: usize, rep_code: u8) -> u32 {
        if rep_code > 0 {
            self.of_rep_cost(rep_code)
        } else {
            let ofv = (off as u32).saturating_add(3);
            let oc = (31 - ofv.leading_zeros()).min(31);
            self.of_cost[oc as usize] as u32 + (oc << 3)
        }
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
}
