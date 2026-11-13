//! 缓存驻留输入保留紧凑位置表（支持代际戳记与三重复偏移），不为标签增加额外空间占用。
use crate::enc::Seq;
use crate::lz::{
    FastCfg, MIN, Position, apply_rep, h_long, h_short, match_score, mlen, probe_reps, rd,
};

pub(crate) struct Compact<P> {
    long: Vec<P>,
    short: Vec<P>,
    max_bits: u32,
    active_bits: u32,
    base: usize,
    next_base: usize,
    window_limit: usize,
}

impl<P: Position> Compact<P> {
    pub fn new(dual: bool, bits: u32) -> Self {
        let bits = bits.max(1);
        Self {
            long: vec![P::EMPTY; if dual { 1 << bits } else { 0 }],
            short: vec![P::EMPTY; 1 << bits],
            max_bits: bits,
            active_bits: bits,
            base: 0,
            next_base: 0,
            window_limit: (u32::MAX - 3) as usize,
        }
    }

    pub fn prepare(&mut self, data_len: usize, active_bits: u32, dual: bool, window_limit: usize) {
        let want_bits = active_bits.max(1);
        if want_bits > self.max_bits {
            self.max_bits = want_bits;
            self.short.resize(1 << want_bits, P::EMPTY);
            if dual {
                self.long.resize(1 << want_bits, P::EMPTY);
            }
            self.short.fill(P::EMPTY);
            self.long.fill(P::EMPTY);
            self.base = 0;
            self.next_base = data_len + 1;
        } else if dual && self.long.len() < (1 << self.max_bits) {
            self.long.resize(1 << self.max_bits, P::EMPTY);
            self.long.fill(P::EMPTY);
            self.short.fill(P::EMPTY);
            self.base = 0;
            self.next_base = data_len + 1;
        } else {
            let overflow = if P::WIDE {
                self.next_base.checked_add(data_len + 2).is_none()
            } else {
                self.next_base
                    .checked_add(data_len + 2)
                    .is_none_or(|n| n > u32::MAX as usize)
            };
            if overflow {
                self.short.fill(P::EMPTY);
                self.long.fill(P::EMPTY);
                self.base = 0;
                self.next_base = data_len + 1;
            } else {
                self.base = self.next_base;
                self.next_base += data_len + 1;
            }
        }
        self.active_bits = want_bits.min(self.max_bits);
        self.window_limit = window_limit.min((u32::MAX - 3) as usize);
    }

    #[inline(always)]
    fn put<const DUAL: bool>(&mut self, d: &[u8], p: usize) {
        if p + 8 <= d.len() {
            let v = rd(d, p);
            let raw = P::from_raw(self.base + p + 1);
            self.short[h_short(v, self.active_bits)] = raw;
            if DUAL {
                self.long[h_long(v, self.active_bits)] = raw;
            }
        }
    }

    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn best<const DUAL: bool>(
        &mut self,
        d: &[u8],
        i: usize,
        end: usize,
        anchor: usize,
        rep: &[usize; 3],
        cfg: FastCfg,
        try_rep: bool,
    ) -> (isize, usize, usize) {
        let v = rd(d, i);
        let hs = h_short(v, self.active_bits);
        let cs = self.short[hs].to_raw();
        let (mut hl, mut cl) = (0, 0);
        if DUAL {
            hl = h_long(v, self.active_bits);
            cl = self.long[hl].to_raw();
        }
        let pv = P::from_raw(self.base + i + 1);
        self.short[hs] = pv;
        if DUAL {
            self.long[hl] = pv;
        }
        let mut best = if try_rep {
            probe_reps(
                d,
                i,
                end,
                v as u32,
                i > anchor,
                self.window_limit,
                rep,
                cfg.all_reps,
            )
        } else {
            (isize::MIN, 0, 0, 0)
        };
        let base = self.base;
        let win = self.window_limit;
        if DUAL && cl > base {
            let c = cl - base - 1;
            if c < i {
                let off = i - c;
                if off <= win {
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
        if (!DUAL || best.2 < 24) && cs > base {
            let c = cs - base - 1;
            if c < i {
                let off = i - c;
                if off <= win {
                    let len = mlen(d, c, i, end);
                    if len >= cfg.min_match {
                        let s = match_score(len, off, 0);
                        if s > best.0 {
                            best = (s, off, len, 0);
                        }
                    }
                }
            }
        }
        (best.0, best.1, best.2)
    }

    pub fn parse(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        cfg: FastCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if cfg.dual && !self.long.is_empty() {
            self.parse_inner::<true>(d, (start, end), cfg, rep, seqs, lits)
        } else {
            self.parse_inner::<false>(d, (start, end), cfg, rep, seqs, lits)
        }
    }

    #[inline(never)]
    fn parse_inner<const DUAL: bool>(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        cfg: FastCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if end > d.len() {
            return start;
        }
        let d = &d[..end];
        let (mut i, mut anchor, mut miss) = (start, start, 0);
        while i + 8 <= end {
            let mut cur = self.best::<DUAL>(d, i, end, anchor, rep, cfg, miss <= 1);
            if cur.2 < MIN {
                i += 1 + (miss >> cfg.miss_shift);
                miss += 1;
                continue;
            }
            miss = 0;
            let mut did_lazy = false;
            if DUAL && cur.2 < cfg.lazy_limit && i + 9 <= end {
                let next = self.best::<DUAL>(d, i + 1, end, anchor, rep, cfg, true);
                if next.0 > cur.0 + 6 {
                    i += 1;
                    cur = next;
                }
                did_lazy = true;
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
            if !did_lazy && cfg.extra_puts >= 2 {
                self.put::<DUAL>(d, i + 1);
            }
            if len > 4 {
                self.put::<DUAL>(d, i + len - 2);
                if cfg.extra_puts >= 2 {
                    self.put::<DUAL>(d, i + len - 1);
                }
            }
            i += len;
            anchor = i;
        }
        anchor
    }
}
