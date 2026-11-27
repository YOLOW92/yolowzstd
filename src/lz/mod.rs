//! 快速匹配器原语与模式分派（level 1..=4：无清零代际哈希表 Fast / DoubleFast / Compact）。
mod compact;
mod fast;

use crate::enc::Seq;

pub(crate) use compact::Compact;
pub(crate) use fast::Fast;

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
    Compact(Compact<u32>),
    Tagged(Fast<u32>),
    WideCompact(Compact<usize>),
    WideTagged(Fast<usize>),
}

impl FastMode {
    pub fn new(dual: bool, max_bits: u32, tagged: bool, wide: bool) -> Self {
        match (tagged, wide) {
            (false, false) => Self::Compact(Compact::new(dual, max_bits)),
            (true, false) => Self::Tagged(Fast::new(dual, max_bits)),
            (false, true) => Self::WideCompact(Compact::new(dual, max_bits)),
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
