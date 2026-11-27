//! 熵引导最优解析器（Entropy-Guided Optimal Parser，L16..=L22）：
//! 结合精确 BT4 后缀树（`bt`）、1/8 bit Q3 在线熵模型（`stats`）与 L2 驻留片段 DP（帕累托支配单次弛豫）。
mod bt;
mod stats;

use crate::enc::Seq;
use crate::lz::{apply_rep, mlen, rd4};

use bt::OptBt;
use stats::{MAX_SUFFICIENT, OptStats};

/// L2 缓存驻留的片段 DP 窗口长度（4096 + 512 节点 × 28 B ≈ 126 KiB < 256 KiB L2）。
const OPT_CHUNK: usize = 4096;
const DP_CAP: usize = OPT_CHUNK + MAX_SUFFICIENT + 2;

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
    bt: OptBt,
    dp: Vec<OptNode>,
    back_ops: Vec<(u16, u32)>,
    stats: OptStats,
}

impl OptParser {
    pub fn new() -> Self {
        Self {
            bt: OptBt::new(),
            dp: vec![OptNode::INF; DP_CAP],
            back_ops: Vec::with_capacity(512),
            stats: OptStats::new(),
        }
    }

    pub fn prepare(&mut self, data_len: usize, cfg: OptCfg) {
        self.bt.prepare(data_len, cfg.hash_bits, cfg.window);
        self.stats.reset_frame();
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
        let win = self.bt.win_mask.min(cfg.window_limit);
        let mut dp_buf = std::mem::take(&mut self.dp);
        if dp_buf.len() < DP_CAP {
            dp_buf.resize(DP_CAP, OptNode::INF);
        }

        let mut cands = [(0usize, 0usize); 32];
        let mut rep_cands = [(0usize, 0u32, 0u8); 3];
        let mut cur = start;
        let mut anchor = start;

        while cur + 3 <= end {
            let cur_rep32 = [rep[0] as u32, rep[1] as u32, rep[2] as u32];
            let has_ll0 = cur > anchor;
            let (nrep0, rep_max0) =
                Self::probe_reps3(d, cur, end, has_ll0, win, cur_rep32, &mut rep_cands);
            let mut nc0 = 0usize;
            let mut bst_max0 = 0usize;
            if cur >= self.bt.next_insert {
                self.bt.bt_step::<true>(
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
                self.bt.insert_match_interior(
                    d,
                    cur + 1,
                    stop,
                    end,
                    cfg.sufficient,
                    cfg.window_limit,
                );
                cur = stop;
                anchor = stop;
                continue;
            }

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

            let mut idx = 0usize;
            loop {
                let pos = cur + idx;
                let cur_node = dp_buf[idx];

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

                let (nrep, rep_max, nc, bst_max) = if idx == 0 {
                    (nrep0, rep_max0, nc0, bst_max0)
                } else if pos + 3 <= end {
                    let has_ll = cur_node.litlen > 0;
                    let (nr, rm) =
                        Self::probe_reps3(d, pos, end, has_ll, win, cur_node.rep, &mut rep_cands);
                    let mut n_bst = 0usize;
                    let mut b_max = 0usize;
                    if pos >= self.bt.next_insert {
                        self.bt.bt_step::<true>(
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

            // 若于 OPT_CHUNK 处截断，直接在已完全弛豫的边界 idx（= OPT_CHUNK）切出，避免被迫采纳越界的最长单一路径。
            let end_idx = match forced_tail {
                Some((t_idx, _, _)) => t_idx,
                None => idx.min(last_pos).min(end - cur),
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
                self.bt.insert_match_interior(
                    d,
                    pos + 1,
                    stop,
                    end,
                    cfg.sufficient,
                    cfg.window_limit,
                );
                cur = stop;
                anchor = stop;
            } else {
                let stop = cur + end_idx;
                if stop > self.bt.next_insert {
                    self.bt.insert_match_interior(
                        d,
                        self.bt.next_insert,
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
    fn bst_splicing_preserves_exact_matches_with_long_shared_prefixes() {
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

        op.bt.next_base = u32::MAX as usize - 16;
        op.prepare(data.len(), cfg);
        assert_eq!(op.bt.base, 0);
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
