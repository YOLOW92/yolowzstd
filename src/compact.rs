//! 缓存驻留输入保留紧凑位置表（支持代际戳记与三重复偏移），不为标签增加额外空间占用。
use crate::enc::Seq;
use crate::lz::{
    FastCfg, MIN, Position, apply_rep, h_4, h_long, h_short, match_first, match_score, mlen,
    probe_reps, rd, rd4,
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
        if !cfg.dual && !cfg.all_reps && cfg.lazy_limit == 0 && cfg.min_match == 5 {
            self.parse_fast_l1(d, (start, end), cfg, rep, seqs, lits)
        } else if cfg.dual && cfg.lazy_limit == 0 && !self.long.is_empty() {
            if cfg.min_match >= 5 {
                self.parse_dfast::<true>(d, (start, end), cfg, rep, seqs, lits)
            } else {
                self.parse_dfast::<false>(d, (start, end), cfg, rep, seqs, lits)
            }
        } else if cfg.dual && !self.long.is_empty() {
            self.parse_inner::<true>(d, (start, end), cfg, rep, seqs, lits)
        } else {
            self.parse_inner::<false>(d, (start, end), cfg, rep, seqs, lits)
        }
    }

    #[inline(never)]
    fn parse_fast_l1(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        _cfg: FastCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if end > d.len() {
            return start;
        }
        let d = &d[..end];
        let base = self.base;
        let win = self.window_limit;
        let bits = self.active_bits;
        let mask = (1usize << bits) - 1;
        let short = &mut self.short[..=mask];
        let (mut i, mut anchor) = (start, start);

        while i + 8 <= end {
            let mut off = 0usize;
            let mut len = 0usize;
            let mut is_rep_next = false;

            {
                let r0 = rep[0];
                if r0 >= 1 && r0 <= i + 1 && r0 <= win && rd4(d, i + 1 - r0) == rd4(d, i + 1) {
                    off = r0;
                    len = 4 + mlen(d, i + 1 - r0 + 4, i + 5, end);
                    is_rep_next = true;
                }
            }

            if len == 0 {
                let mut step = 1usize;
                let mut next_bump = i + 8;
                while i + 8 <= end {
                    let v = rd(d, i);
                    let hs = h_short(v, bits) & mask;
                    let raw = short[hs].to_raw();
                    short[hs] = P::from_raw(base + i + 1);
                    if raw > base {
                        let c = raw - base - 1;
                        if c < i && i - c <= win {
                            let diff = rd(d, c) ^ v;
                            if (diff & 0x00FF_FFFF_FFFF) == 0 {
                                off = i - c;
                                let tail = ((diff >> 40) as u32).trailing_zeros() >> 3;
                                len = if tail < 3 {
                                    5 + tail as usize
                                } else {
                                    8 + mlen(d, c + 8, i + 8, end)
                                };
                                break;
                            }
                        }
                    }
                    i += step;
                    if i >= next_bump {
                        step = 2 + ((i - anchor) >> 4);
                        next_bump = i + 16;
                    }
                }
                if len == 0 {
                    break;
                }
            }

            if is_rep_next {
                i += 1;
                while i > anchor + 1 && off < i && d[i - 1] == d[i - off - 1] {
                    i -= 1;
                    len += 1;
                }
            } else {
                while i > anchor && off < i && d[i - 1] == d[i - off - 1] {
                    i -= 1;
                    len += 1;
                }
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
            let next_i = i + len;
            if len > 4 && next_i >= 2 && next_i + 6 <= end {
                let vp = rd(d, next_i - 2);
                short[h_short(vp, bits) & mask] = P::from_raw(base + next_i - 1);
            }
            i = next_i;
            anchor = i;
            while i + 8 <= end {
                let r1 = rep[1];
                if r1 >= 1 && r1 <= i && r1 <= win && rd4(d, i - r1) == rd4(d, i) {
                    let rlen = 4 + mlen(d, i - r1 + 4, i + 4, end);
                    let rcode = apply_rep(rep, 0, r1);
                    seqs.push(Seq {
                        ll: 0,
                        ml: rlen as u32,
                        off: r1 as u32,
                        rep_code: rcode,
                    });
                    let ni = i + rlen;
                    if ni >= 2 && ni + 6 <= end {
                        let vp = rd(d, ni - 2);
                        short[h_short(vp, bits) & mask] = P::from_raw(base + ni - 1);
                    }
                    i = ni;
                    anchor = i;
                } else {
                    break;
                }
            }
        }
        anchor
    }

    #[inline(never)]
    fn parse_dfast<const SHORT5: bool>(
        &mut self,
        d: &[u8],
        (start, end): (usize, usize),
        _cfg: FastCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        if end > d.len() {
            return start;
        }
        let d = &d[..end];
        let base = self.base;
        let win = self.window_limit;
        let bits = self.active_bits;
        let mask = (1usize << bits) - 1;
        let short = &mut self.short[..=mask];
        let long = &mut self.long[..=mask];
        let (mut i, mut anchor) = (start, start);

        while i + 8 <= end {
            let mut off = 0usize;
            let mut len = 0usize;
            let mut is_rep_next = false;

            {
                let r0 = rep[0];
                if r0 >= 1 && r0 <= i + 1 && r0 <= win && rd4(d, i + 1 - r0) == rd4(d, i + 1) {
                    off = r0;
                    len = 4 + mlen(d, i + 1 - r0 + 4, i + 5, end);
                    is_rep_next = true;
                }
            }

            if len == 0 {
                let mut step = 1usize;
                let mut next_bump = i + 64;
                while i + 8 <= end {
                    let v = rd(d, i);
                    let v4 = v as u32;
                    let hl = h_long(v, bits) & mask;
                    let hs = if SHORT5 {
                        h_short(v, bits) & mask
                    } else {
                        h_4(v4, bits) & mask
                    };
                    let cl = long[hl].to_raw();
                    let cs = short[hs].to_raw();
                    let pv = P::from_raw(base + i + 1);
                    long[hl] = pv;
                    short[hs] = pv;

                    if cl > base {
                        let c = cl - base - 1;
                        if c < i && i - c <= win {
                            let l = match_first(d, c, i, end, v);
                            if l >= MIN {
                                off = i - c;
                                len = l;
                                break;
                            }
                        }
                    }

                    if cs > base {
                        let c = cs - base - 1;
                        if c < i && i - c <= win {
                            let l = match_first(d, c, i, end, v);
                            let o = i - c;
                            let ok = if SHORT5 {
                                l >= 5
                            } else {
                                l >= 5 || (l == 4 && o < (1 << 17))
                            };
                            if ok {
                                off = o;
                                len = l;
                                if i + 9 <= end {
                                    let v_nx = rd(d, i + 1);
                                    let hl2 = h_long(v_nx, bits) & mask;
                                    let cl2 = long[hl2].to_raw();
                                    let p2 = P::from_raw(base + i + 2);
                                    long[hl2] = p2;
                                    let mut take_nx = false;
                                    if cl2 > base {
                                        let c2 = cl2 - base - 1;
                                        if c2 <= i && (i + 1 - c2) <= win {
                                            let l2 = match_first(d, c2, i + 1, end, v_nx);
                                            let o2 = i + 1 - c2;
                                            if l2 > len + 1
                                                || (l2 == len + 1 && o2 < off.saturating_mul(8))
                                                || (l2 == len && o2.saturating_mul(16) < off)
                                            {
                                                i += 1;
                                                off = o2;
                                                len = l2;
                                                take_nx = true;
                                            }
                                        }
                                    }
                                    if !take_nx && len < 12 {
                                        let hs2 = if SHORT5 {
                                            h_short(v_nx, bits) & mask
                                        } else {
                                            h_4(v_nx as u32, bits) & mask
                                        };
                                        let cs2 = short[hs2].to_raw();
                                        short[hs2] = p2;
                                        if cs2 > base {
                                            let c2 = cs2 - base - 1;
                                            if c2 <= i && (i + 1 - c2) <= win {
                                                let l2 = match_first(d, c2, i + 1, end, v_nx);
                                                let o2 = i + 1 - c2;
                                                if l2 > len + 1
                                                    || (l2 == len + 1 && o2 < off.saturating_mul(8))
                                                    || (l2 == len && o2.saturating_mul(16) < off)
                                                {
                                                    i += 1;
                                                    off = o2;
                                                    len = l2;
                                                }
                                            }
                                        }
                                    }
                                }
                                break;
                            }
                        }
                    }

                    i += step;
                    if i >= next_bump {
                        step += 1;
                        next_bump = i + 64;
                    }
                }
                if len == 0 {
                    break;
                }
            }

            if is_rep_next {
                i += 1;
                while i > anchor + 1 && off < i && d[i - 1] == d[i - off - 1] {
                    i -= 1;
                    len += 1;
                }
            } else {
                while i > anchor && off < i && d[i - 1] == d[i - off - 1] {
                    i -= 1;
                    len += 1;
                }
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
            let next_i = i + len;
            if i + 9 <= end {
                let v1 = rd(d, i + 1);
                let p1 = P::from_raw(base + i + 2);
                let hs1 = if SHORT5 {
                    h_short(v1, bits) & mask
                } else {
                    h_4(v1 as u32, bits) & mask
                };
                short[hs1] = p1;
                long[h_long(v1, bits) & mask] = p1;
            }
            if len > 4 && next_i >= 2 && next_i + 6 <= end {
                let vp = rd(d, next_i - 2);
                let pp = P::from_raw(base + next_i - 1);
                let hsp = if SHORT5 {
                    h_short(vp, bits) & mask
                } else {
                    h_4(vp as u32, bits) & mask
                };
                short[hsp] = pp;
                long[h_long(vp, bits) & mask] = pp;
            }
            i = next_i;
            anchor = i;
            while i + 8 <= end {
                let r1 = rep[1];
                if r1 >= 1 && r1 <= i && r1 <= win && rd4(d, i - r1) == rd4(d, i) {
                    let rlen = 4 + mlen(d, i - r1 + 4, i + 4, end);
                    let rcode = apply_rep(rep, 0, r1);
                    seqs.push(Seq {
                        ll: 0,
                        ml: rlen as u32,
                        off: r1 as u32,
                        rep_code: rcode,
                    });
                    let ni = i + rlen;
                    if ni >= 2 && ni + 6 <= end {
                        let vp = rd(d, ni - 2);
                        let pp = P::from_raw(base + ni - 1);
                        let hsp = if SHORT5 {
                            h_short(vp, bits) & mask
                        } else {
                            h_4(vp as u32, bits) & mask
                        };
                        short[hsp] = pp;
                        long[h_long(vp, bits) & mask] = pp;
                    }
                    i = ni;
                    anchor = i;
                } else {
                    break;
                }
            }
        }
        anchor
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
                if cfg.extra_puts >= 3 {
                    self.put::<DUAL>(d, i + len - 1);
                }
            }
            i += len;
            anchor = i;
        }
        anchor
    }
}
