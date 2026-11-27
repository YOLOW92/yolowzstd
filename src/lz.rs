//! 快速匹配器：level 1 到 4 使用带代际戳记的紧凑/标签哈希表（零跨次清零开销）。
//! 支持完整的 Zstd 三重复偏移、位代价评分与可选一步前瞻。
use crate::enc::Seq;

pub(crate) const MIN: usize = 4;
const PRIME: u64 = 0x9E37_79B1_85EB_CA87;

pub(crate) trait Position: Copy {
    type Candidate: Copy;
    const EMPTY: Self;
    const WIDE: bool;
    fn from_raw(p: usize) -> Self;
    fn to_raw(self) -> usize;
    fn candidate(pos_val: usize, first: u32) -> Self::Candidate;
    fn unpack(candidate: Self::Candidate) -> (usize, u32);
}

impl Position for u32 {
    type Candidate = u64;
    const EMPTY: Self = 0;
    const WIDE: bool = false;
    #[inline(always)]
    fn from_raw(p: usize) -> Self {
        p as u32
    }
    #[inline(always)]
    fn to_raw(self) -> usize {
        self as usize
    }
    #[inline(always)]
    fn candidate(pos_val: usize, first: u32) -> u64 {
        (pos_val as u32 as u64) | ((first as u64) << 32)
    }
    #[inline(always)]
    fn unpack(candidate: u64) -> (usize, u32) {
        (candidate as u32 as usize, (candidate >> 32) as u32)
    }
}

impl Position for usize {
    type Candidate = (usize, u32);
    const EMPTY: Self = 0;
    const WIDE: bool = true;
    #[inline(always)]
    fn from_raw(p: usize) -> Self {
        p
    }
    #[inline(always)]
    fn to_raw(self) -> usize {
        self
    }
    #[inline(always)]
    fn candidate(pos_val: usize, first: u32) -> Self::Candidate {
        (pos_val, first)
    }
    #[inline(always)]
    fn unpack(candidate: Self::Candidate) -> (usize, u32) {
        candidate
    }
}

#[inline(always)]
pub(crate) fn rd(d: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(d[p..p + 8].try_into().expect("8 字节"))
}

#[inline(always)]
pub(crate) fn h_long(v: u64, bits: u32) -> usize {
    ((v << 8).wrapping_mul(PRIME) >> (64 - bits)) as usize
}

#[inline(always)]
pub(crate) fn h_short(v: u64, bits: u32) -> usize {
    ((v << 24).wrapping_mul(PRIME) >> (64 - bits)) as usize
}

#[inline(always)]
pub(crate) fn h_4(v4: u32, bits: u32) -> usize {
    (((v4 as u64).wrapping_mul(PRIME)) >> (64 - bits)) as usize
}

#[inline(always)]
pub(crate) fn rd4(d: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(d[p..p + 4].try_into().expect("4 字节"))
}

#[inline(never)]
fn mlen_tail(d: &[u8], a: usize, b: usize, end: usize) -> usize {
    if a >= b || end > d.len() {
        return 0;
    }
    let d = &d[..end];
    let mut l = 0;
    while b + l + 8 <= end {
        let x = rd(d, a + l) ^ rd(d, b + l);
        if x != 0 {
            return l + (x.trailing_zeros() >> 3) as usize;
        }
        l += 8;
    }
    while b + l < end && d[a + l] == d[b + l] {
        l += 1;
    }
    l
}

#[inline(always)]
pub(crate) fn mlen(d: &[u8], a: usize, b: usize, end: usize) -> usize {
    if a >= b || end > d.len() {
        return 0;
    }
    if b + 8 <= end {
        let x = rd(d, a) ^ rd(d, b);
        if x != 0 {
            return (x.trailing_zeros() >> 3) as usize;
        }
        return 8 + mlen_tail(d, a + 8, b + 8, end);
    }
    let d = &d[..end];
    let mut l = 0;
    while b + l < end && d[a + l] == d[b + l] {
        l += 1;
    }
    l
}

#[inline(always)]
pub(crate) fn match_first(d: &[u8], a: usize, b: usize, end: usize, first: u64) -> usize {
    if a >= b || end > d.len() || b + 8 > end {
        return 0;
    }
    let x = rd(d, a) ^ first;
    if x == 0 {
        8 + mlen(d, a + 8, b + 8, end)
    } else {
        (x.trailing_zeros() / 8) as usize
    }
}

/// 按 Zstd 规则根据当前 `(ll, off)` 更新三重复偏移表，返回 `rep_code`（0 = 普通偏移，1..=3 = 重复偏移码）。
#[inline(always)]
pub(crate) fn apply_rep(rep: &mut [usize; 3], ll: usize, off: usize) -> u8 {
    if ll > 0 {
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

/// 约 1 比特为单位的匹配净收益评分：权衡匹配长度与偏移编码位数。
#[inline(always)]
pub(crate) fn match_score(len: usize, off: usize, rep_code: u8) -> isize {
    let off_cost = match rep_code {
        1 => 0,
        2 => 2,
        3 => 3,
        _ => (31 - ((off as u32).saturating_add(3).leading_zeros())) as isize + 2,
    };
    (len as isize * 7) - off_cost
}

/// 先按已加载的 4 字节前缀 `v4` 快速过滤，再对命中的 `rep` 候选执行扩展比较。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn probe_reps(
    d: &[u8],
    i: usize,
    end: usize,
    v4: u32,
    has_ll: bool,
    window: usize,
    rep: &[usize; 3],
    all_reps: bool,
) -> (isize, usize, usize, u8) {
    if i + 4 > end {
        return (isize::MIN, 0, 0, 0);
    }
    let r0 = if has_ll { rep[0] } else { rep[1] };
    let mut best = (isize::MIN, 0usize, 0usize, 0u8);
    if r0 >= 1 && r0 <= i && r0 <= window && rd4(d, i - r0) == v4 {
        let len = 4 + mlen(d, i - r0 + 4, i + 4, end);
        best = (match_score(len, r0, 1), r0, len, 1);
    }
    if all_reps {
        let (r1, r2) = if has_ll {
            (rep[1], rep[2])
        } else {
            (rep[2], rep[0].saturating_sub(1))
        };
        if r1 >= 1 && r1 <= i && r1 <= window && rd4(d, i - r1) == v4 {
            let len = 4 + mlen(d, i - r1 + 4, i + 4, end);
            let s = match_score(len, r1, 2);
            if s > best.0 {
                best = (s, r1, len, 2);
            }
        }
        if r2 >= 1 && r2 <= i && r2 <= window && rd4(d, i - r2) == v4 {
            let len = 4 + mlen(d, i - r2 + 4, i + 4, end);
            let s = match_score(len, r2, 3);
            if s > best.0 {
                best = (s, r2, len, 3);
            }
        }
    }
    best
}

pub(crate) fn entry_bytes(wide: bool) -> usize {
    if wide {
        std::mem::size_of::<<usize as Position>::Candidate>()
    } else {
        std::mem::size_of::<<u32 as Position>::Candidate>()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct FastCfg {
    pub dual: bool,
    pub min_match: usize,
    pub lazy_limit: usize,
    pub miss_shift: u32,
    pub extra_puts: u8,
    pub all_reps: bool,
}

impl FastCfg {
    pub fn for_level(level: i32) -> Self {
        match level {
            ..=1 => Self {
                dual: false,
                min_match: 5,
                lazy_limit: 0,
                miss_shift: 4,
                extra_puts: 1,
                all_reps: false,
            },
            2 => Self {
                dual: false,
                min_match: 4,
                lazy_limit: 0,
                miss_shift: 6,
                extra_puts: 2,
                all_reps: false,
            },
            3 => Self {
                dual: true,
                min_match: 4,
                lazy_limit: 0,
                miss_shift: 6,
                extra_puts: 2,
                all_reps: false,
            },
            _ => Self {
                dual: true,
                min_match: 4,
                lazy_limit: 16,
                miss_shift: 7,
                extra_puts: 2,
                all_reps: true,
            },
        }
    }
}

pub(crate) enum FastMode {
    Compact(crate::compact::Compact<u32>),
    Tagged(Fast<u32>),
    WideCompact(crate::compact::Compact<usize>),
    WideTagged(Fast<usize>),
}

impl FastMode {
    pub fn new(dual: bool, max_bits: u32, tagged: bool, wide: bool) -> Self {
        match (tagged, wide) {
            (false, false) => Self::Compact(crate::compact::Compact::new(dual, max_bits)),
            (true, false) => Self::Tagged(Fast::new(dual, max_bits)),
            (false, true) => Self::WideCompact(crate::compact::Compact::new(dual, max_bits)),
            (true, true) => Self::WideTagged(Fast::new(dual, max_bits)),
        }
    }

    pub fn prepare(&mut self, data_len: usize, active_bits: u32, dual: bool, window_limit: usize) {
        match self {
            Self::Compact(f) => f.prepare(data_len, active_bits, dual, window_limit),
            Self::Tagged(f) => f.prepare(data_len, active_bits, dual, window_limit),
            Self::WideCompact(f) => f.prepare(data_len, active_bits, dual, window_limit),
            Self::WideTagged(f) => f.prepare(data_len, active_bits, dual, window_limit),
        }
    }

    pub fn parse(
        &mut self,
        d: &[u8],
        range: (usize, usize),
        cfg: FastCfg,
        rep: &mut [usize; 3],
        seqs: &mut Vec<Seq>,
        lits: &mut Vec<u8>,
    ) -> usize {
        match self {
            Self::Compact(f) => f.parse(d, range, cfg, rep, seqs, lits),
            Self::Tagged(f) => f.parse(d, range, cfg, rep, seqs, lits),
            Self::WideCompact(f) => f.parse(d, range, cfg, rep, seqs, lits),
            Self::WideTagged(f) => f.parse(d, range, cfg, rep, seqs, lits),
        }
    }
}

pub(crate) struct Fast<P: Position> {
    long: Vec<P::Candidate>,
    short: Vec<P::Candidate>,
    max_bits: u32,
    active_bits: u32,
    base: usize,
    next_base: usize,
    window_limit: usize,
}

impl<P: Position> Fast<P> {
    pub fn new(dual: bool, bits: u32) -> Self {
        let bits = bits.max(1);
        let empty = P::candidate(0, 0);
        Self {
            long: vec![empty; if dual { 1 << bits } else { 0 }],
            short: vec![empty; 1 << bits],
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
            self.short.resize(1 << want_bits, P::candidate(0, 0));
            if dual {
                self.long.resize(1 << want_bits, P::candidate(0, 0));
            }
            // 新增扩容区间内容可能旧过或等于 0；安全起见重设基址，只需在极罕见的扩容时发生一次。
            self.short.fill(P::candidate(0, 0));
            self.long.fill(P::candidate(0, 0));
            self.base = 0;
            self.next_base = data_len + 1;
        } else {
            if dual && self.long.len() < (1 << self.max_bits) {
                self.long.resize(1 << self.max_bits, P::candidate(0, 0));
                self.long.fill(P::candidate(0, 0));
                self.short.fill(P::candidate(0, 0));
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
                    self.short.fill(P::candidate(0, 0));
                    self.long.fill(P::candidate(0, 0));
                    self.base = 0;
                    self.next_base = data_len + 1;
                } else {
                    self.base = self.next_base;
                    self.next_base += data_len + 1;
                }
            }
        }
        self.active_bits = want_bits.min(self.max_bits);
        self.window_limit = window_limit.min((u32::MAX - 3) as usize);
    }

    #[inline(always)]
    fn put<const DUAL: bool>(&mut self, d: &[u8], p: usize) {
        if p + 8 <= d.len() {
            let v = rd(d, p);
            let candidate = P::candidate(self.base + p + 1, v as u32);
            self.short[h_short(v, self.active_bits)] = candidate;
            if DUAL {
                self.long[h_long(v, self.active_bits)] = candidate;
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
        let v4 = v as u32;
        let hs = h_short(v, self.active_bits);
        let cs = self.short[hs];
        let (mut hl, mut cl) = (0, P::candidate(0, 0));
        if DUAL {
            hl = h_long(v, self.active_bits);
            cl = self.long[hl];
        }
        let candidate = P::candidate(self.base + i + 1, v4);
        self.short[hs] = candidate;
        if DUAL {
            self.long[hl] = candidate;
        }
        let mut best = if try_rep {
            probe_reps(
                d,
                i,
                end,
                v4,
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
        let mut c_long = usize::MAX;
        if DUAL {
            let (raw, first) = P::unpack(cl);
            if first == v4 && raw > base {
                let c = raw - base - 1;
                if c < i {
                    let off = i - c;
                    if off <= win {
                        c_long = c;
                        let l = match_first(d, c, i, end, v);
                        if l >= MIN {
                            let s = match_score(l, off, 0);
                            if s > best.0 {
                                best = (s, off, l, 0);
                            }
                        }
                    }
                }
            }
        }
        if !DUAL || best.2 < 24 {
            let (raw, first) = P::unpack(cs);
            if first == v4 && raw > base {
                let c = raw - base - 1;
                if c < i && (!DUAL || c != c_long) {
                    let off = i - c;
                    if off <= win {
                        let l = match_first(d, c, i, end, v);
                        if l >= cfg.min_match {
                            let s = match_score(l, off, 0);
                            if s > best.0 {
                                best = (s, off, l, 0);
                            }
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

            // 仅在紧邻上条匹配处做一次 i+1 的 rep[0] 探测，避免污染内层 miss 循环
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
                    let v4 = v as u32;
                    let hs = h_short(v, bits) & mask;
                    let cs = short[hs];
                    short[hs] = P::candidate(base + i + 1, v4);
                    let (raw, first) = P::unpack(cs);
                    if first == v4 && raw > base {
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
                short[h_short(vp, bits) & mask] = P::candidate(base + next_i - 1, vp as u32);
            }
            i = next_i;
            anchor = i;
            // 连续 0-literal rep[1] 链式快检
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
                        short[h_short(vp, bits) & mask] = P::candidate(base + ni - 1, vp as u32);
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

    /// 双哈希快速匹配器（DoubleFast，L3）：极简无 rep 内层 miss 循环 + 4/5B 短表 + 7B 长表 + i+1 率失真前瞻。
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

            // 紧邻上条匹配处单次探测 i+1 的 rep[0]
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
                    let cl = long[hl];
                    let cs = short[hs];
                    let cand = P::candidate(base + i + 1, v4);
                    long[hl] = cand;
                    short[hs] = cand;

                    // 1) 长表命中（7B 哈希）
                    let (raw_l, first_l) = P::unpack(cl);
                    if first_l == v4 && raw_l > base {
                        let c = raw_l - base - 1;
                        if c < i && i - c <= win {
                            let l = match_first(d, c, i, end, v);
                            if l >= MIN {
                                off = i - c;
                                len = l;
                                break;
                            }
                        }
                    }

                    // 2) 短表命中（4/5B 哈希）：命中时对 i+1 做长/短率失真前瞻
                    let (raw_s, first_s) = P::unpack(cs);
                    if first_s == v4 && raw_s > base {
                        let c = raw_s - base - 1;
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
                                    let v4_nx = v_nx as u32;
                                    let hl2 = h_long(v_nx, bits) & mask;
                                    let cl2 = long[hl2];
                                    let cand2 = P::candidate(base + i + 2, v4_nx);
                                    long[hl2] = cand2;
                                    let mut take_nx = false;
                                    let (raw2, first2) = P::unpack(cl2);
                                    if first2 == v4_nx && raw2 > base {
                                        let c2 = raw2 - base - 1;
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
                                            h_4(v4_nx, bits) & mask
                                        };
                                        let cs2 = short[hs2];
                                        short[hs2] = cand2;
                                        let (raw2, first2) = P::unpack(cs2);
                                        if first2 == v4_nx && raw2 > base {
                                            let c2 = raw2 - base - 1;
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
                let c1 = P::candidate(base + i + 2, v1 as u32);
                let hs1 = if SHORT5 {
                    h_short(v1, bits) & mask
                } else {
                    h_4(v1 as u32, bits) & mask
                };
                short[hs1] = c1;
                long[h_long(v1, bits) & mask] = c1;
            }
            if len > 4 && next_i >= 2 && next_i + 6 <= end {
                let vp = rd(d, next_i - 2);
                let cp = P::candidate(base + next_i - 1, vp as u32);
                let hsp = if SHORT5 {
                    h_short(vp, bits) & mask
                } else {
                    h_4(vp as u32, bits) & mask
                };
                short[hsp] = cp;
                long[h_long(vp, bits) & mask] = cp;
            }
            i = next_i;
            anchor = i;
            // 连续 0-literal rep[1] 链式快检
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
                        let cp = P::candidate(base + ni - 1, vp as u32);
                        let hsp = if SHORT5 {
                            h_short(vp, bits) & mask
                        } else {
                            h_4(vp as u32, bits) & mask
                        };
                        short[hsp] = cp;
                        long[h_long(vp, bits) & mask] = cp;
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
        let mut i = start;
        let mut anchor = start;
        let mut miss = 0usize;
        while i + 8 <= end {
            let mut cur = self.best::<DUAL>(d, i, end, anchor, rep, cfg, miss <= 1);
            if cur.2 < MIN {
                i += 1 + (miss >> cfg.miss_shift);
                miss += 1;
                continue;
            }
            miss = 0;
            // 一步前瞻
            let mut did_lazy = false;
            if DUAL && cur.2 < cfg.lazy_limit && i + 9 <= end {
                let nx = self.best::<DUAL>(d, i + 1, end, anchor, rep, cfg, true);
                if nx.0 > cur.0 + 6 {
                    i += 1;
                    cur = nx;
                }
                did_lazy = true;
            }
            let (_, off, mut l) = cur;
            // 回溯补齐因 miss 步进跳过的前缀字节
            while i > anchor && off < i && d[i - 1] == d[i - off - 1] {
                i -= 1;
                l += 1;
            }
            let ll = i - anchor;
            lits.extend_from_slice(&d[anchor..i]);
            let rep_code = apply_rep(rep, ll, off);
            seqs.push(Seq {
                ll: ll as u32,
                ml: l as u32,
                off: off as u32,
                rep_code,
            });
            if !did_lazy && cfg.extra_puts >= 2 {
                self.put::<DUAL>(d, i + 1);
            }
            if l > 4 {
                self.put::<DUAL>(d, i + l - 2);
                if cfg.extra_puts >= 3 {
                    self.put::<DUAL>(d, i + l - 1);
                }
            }
            i += l;
            anchor = i;
        }
        anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_and_wide_tables_agree() {
        let data = b"<div class=\"entry\">alpha beta gamma</div>\n".repeat(1000);
        for bits in [1, 8, 14] {
            for level in [1, 2, 3, 4] {
                let cfg = FastCfg::for_level(level);
                let (mut a, mut b) = (
                    Fast::<u32>::new(cfg.dual, bits),
                    Fast::<usize>::new(cfg.dual, bits),
                );
                a.prepare(data.len(), bits, cfg.dual, usize::MAX);
                b.prepare(data.len(), bits, cfg.dual, usize::MAX);
                let (mut sa, mut sb, mut la, mut lb) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                let (mut ra, mut rb) = ([1, 4, 8], [1, 4, 8]);
                assert_eq!(
                    a.parse(&data, (0, data.len()), cfg, &mut ra, &mut sa, &mut la),
                    b.parse(&data, (0, data.len()), cfg, &mut rb, &mut sb, &mut lb)
                );
                assert_eq!(la, lb);
                assert_eq!(ra, rb);
                assert_eq!(sa, sb);
                for wide in [false, true] {
                    let mut compact = FastMode::new(cfg.dual, bits, false, wide);
                    compact.prepare(data.len(), bits, cfg.dual, usize::MAX);
                    let (mut seqs, mut lits, mut rep) = (Vec::new(), Vec::new(), [1, 4, 8]);
                    compact.parse(&data, (0, data.len()), cfg, &mut rep, &mut seqs, &mut lits);
                    assert_eq!(lits, la);
                    assert_eq!(rep, ra);
                    assert_eq!(seqs, sa);
                }
            }
        }
    }
}
