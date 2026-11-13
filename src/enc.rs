//! 自适应 Zstd 编码器：
//! - level 1..=4：无清零代际哈希表（Fast/DoubleFast），支持三重复偏移与位代价评分
//! - level 5..=15：浅/深哈希链 + 1/2 步前瞻
//! - level 16..=22：深哈希链 + 块内前向比特代价最优 DP
//! - 零每块堆分配，完整支持 Raw/RLE/Compressed 块及 Predef/RLE/Custom/Repeat FSE 表。
use crate::chain::{Chain, ChainCfg};
use crate::dec::{LL_BASE, LL_DEFAULT, ML_BASE, ML_DEFAULT, OF_DEFAULT};
#[cfg(test)]
use crate::fse;
use crate::henc::{HuffScratch, normalize_into, write_dist_into, write_literals_into};
use crate::lz::{FastCfg, FastMode};
use crate::resources::Budget;

pub(crate) const BLOCK: usize = 128 * 1024;

/// 压缩参数配置（支持等级、帧末尾 XXH64 校验和、可选窗口对数上限及多线程并行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompressOptions {
    /// 压缩等级：`<= 0` 仅存 Raw/RLE 块，`1..=4` 快速/双哈希，`5..=15` 哈希链前瞻，`16..=22` 最优 DP。
    pub level: i32,
    /// 是否在帧尾写入 XXH64 低 32 位内容校验和（默认开启）。
    pub checksum: bool,
    /// 可选窗口对数上限（合法范围 `10..=30`；`None` 表示单段帧或随硬件预算）。
    pub window_log: Option<u32>,
    /// 压缩并行线程数：`0` 表示自动探测多核并行，`1` 表示单线程（默认），`N >= 2` 表示使用 `N` 个工作线程。
    pub threads: usize,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            level: 3,
            checksum: true,
            window_log: None,
            threads: 1,
        }
    }
}

impl CompressOptions {
    pub fn new(level: i32) -> Self {
        Self {
            level,
            ..Self::default()
        }
    }

    pub fn with_level(mut self, level: i32) -> Self {
        self.level = level;
        self
    }

    pub fn with_checksum(mut self, checksum: bool) -> Self {
        self.checksum = checksum;
        self
    }

    pub fn with_window_log(mut self, window_log: Option<u32>) -> Self {
        self.window_log = window_log.map(|w| w.clamp(10, 30));
        self
    }

    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    #[inline]
    fn window_limit(self) -> usize {
        match self.window_log {
            Some(w) => 1usize << w.clamp(10, 30),
            None => (u32::MAX - 3) as usize,
        }
    }
}

/// 返回压缩 `len` 字节最坏情况所需的输出上界字节数（含多帧分块上限）。
pub const fn compress_bound(len: usize) -> usize {
    len.saturating_add(len / BLOCK + 1)
        .saturating_add((len / BLOCK + 1).saturating_mul(3))
        .saturating_add((len / (256 * 1024) + 1).saturating_mul(20))
        .saturating_add(22)
}

/// 无自有缓冲的流式位累加器，直接写入调用方 `&mut Vec<u8>`。
pub(crate) struct BitAcc {
    acc: u64,
    n: u32,
}

impl BitAcc {
    #[inline(always)]
    pub const fn new() -> Self {
        Self { acc: 0, n: 0 }
    }

    #[inline(always)]
    pub fn put(&mut self, out: &mut Vec<u8>, v: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc |= ((v as u64) & ((1u64 << bits) - 1)) << self.n;
        self.n += bits;
        if self.n >= 32 {
            out.extend_from_slice(&(self.acc as u32).to_le_bytes());
            self.acc >>= 32;
            self.n -= 32;
        }
    }

    #[inline(always)]
    pub fn flush_bytes(mut self, out: &mut Vec<u8>) {
        while self.n > 0 {
            out.push(self.acc as u8);
            self.acc >>= 8;
            self.n = self.n.saturating_sub(8);
        }
    }

    #[inline(always)]
    pub fn finish(mut self, out: &mut Vec<u8>) {
        self.put(out, 1, 1);
        self.flush_bytes(out);
    }
}

/// 小值（< N）直接查表得到码；用 `bases` 里的基值反推。
const fn small_codes<const N: usize, const M: usize>(
    bases: &[(u32, u32); M],
    delta: u32,
) -> [u8; N] {
    let mut t = [0u8; N];
    let mut v = 0;
    let mut c = 0;
    while v < N {
        while c + 1 < M && (bases[c + 1].0 as usize) <= v + delta as usize {
            c += 1;
        }
        t[v] = c as u8;
        v += 1;
    }
    t
}

static LL_TAB: [u8; 64] = small_codes(&LL_BASE, 0);
static ML_TAB: [u8; 128] = small_codes(&ML_BASE, 3);

/// 返回（码，额外位数，额外值）。大值的码是 highbit 加常数，小值查表，没有循环。
#[inline(always)]
fn ll_code(v: u32) -> (u32, u32, u32) {
    let c = if v < 64 {
        LL_TAB[v as usize] as u32
    } else {
        19 + (31 - v.leading_zeros())
    };
    let (base, bits) = LL_BASE[c as usize];
    (c, bits, v - base)
}

#[inline(always)]
fn ml_code(v: u32) -> (u32, u32, u32) {
    let m = v - 3;
    let c = if m < 128 {
        ML_TAB[m as usize] as u32
    } else {
        36 + (31 - m.leading_zeros())
    };
    let (base, bits) = ML_BASE[c as usize];
    (c, bits, v - base)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seq {
    pub ll: u32,
    pub ml: u32,
    pub off: u32,
    /// 0 = 普通偏移 (`ofv = off + 3`), 1..=3 = 重复偏移码 (`ofv = rep_code`).
    pub rep_code: u8,
}

/// L1 缓存驻留的 FSE 编码查找表（无分支转移，支持 Predef / RLE / Custom / Repeat）。
#[derive(Clone)]
struct SymTable {
    al: u32,
    size: u32,
    symbols: u64,
    delta_nb: [u32; 64],
    sym_off: [i16; 64],
    cumul: [u16; 64],
    state_table: [u16; 512],
    /// 自定义表或 RLE 的原始归一化概率，用于评估下块 `Repeat` 收益。
    valid: bool,
    is_rle: bool,
    probs: [i16; 64],
    probs_len: usize,
}

impl SymTable {
    fn empty() -> Self {
        Self {
            al: 0,
            size: 1,
            symbols: 0,
            delta_nb: [0; 64],
            sym_off: [0; 64],
            cumul: [0; 64],
            state_table: [0; 512],
            valid: false,
            is_rle: false,
            probs: [0; 64],
            probs_len: 0,
        }
    }

    fn set_rle(&mut self, sym: u8) {
        self.al = 0;
        self.size = 1;
        self.symbols = 1u64 << sym;
        self.delta_nb.fill(0);
        self.sym_off.fill(0);
        self.cumul.fill(0);
        self.sym_off[sym as usize] = -1;
        self.state_table[0] = 1;
        self.valid = true;
        self.is_rle = true;
        self.probs.fill(0);
        self.probs[0] = sym as i16;
        self.probs_len = 1;
    }

    /// 直接从归一化分布扩散填表并预加 `size` 基址，热路径无需再构造解码器 `Cell` 或加 `self.size`。
    fn fill_from_probs(&mut self, probs: &[i16], al: u32) -> bool {
        let size = 1u32 << al;
        let size_usize = size as usize;
        let mut syms = [0u8; 512];
        let mut high = size_usize;
        let mut symbols = 0u64;
        let mut total = 0u16;
        self.cumul.fill(0);
        self.delta_nb.fill(0);
        self.sym_off.fill(0);
        for (s, &p) in probs.iter().enumerate() {
            self.cumul[s] = total;
            if p != 0 {
                let c = if p == -1 {
                    high -= 1;
                    syms[high] = s as u8;
                    1u32
                } else {
                    p.max(0) as u32
                };
                symbols |= 1u64 << s;
                total += c as u16;
                let k = 31 - c.leading_zeros();
                let max_bits_out = al - k;
                let thresh = c << max_bits_out;
                self.delta_nb[s] = (max_bits_out << 16).wrapping_sub(thresh);
                self.sym_off[s] = self.cumul[s] as i16 - c as i16;
            }
        }
        let step = (size_usize >> 1) + (size_usize >> 3) + 3;
        let mask = size_usize - 1;
        let mut pos = 0usize;
        for (s, &p) in probs.iter().enumerate() {
            for _ in 0..p.max(0) {
                syms[pos] = s as u8;
                pos = (pos + step) & mask;
                while pos >= high {
                    pos = (pos + step) & mask;
                }
            }
        }
        if pos != 0 {
            return false;
        }
        let mut cursor = self.cumul;
        for (idx, &sym) in syms[..size_usize].iter().enumerate() {
            let s = sym as usize;
            let slot = cursor[s] as usize;
            cursor[s] += 1;
            self.state_table[slot & 511] = (idx + size_usize) as u16;
        }
        self.al = al;
        self.size = size;
        self.symbols = symbols;
        self.probs.fill(0);
        self.probs[..probs.len()].copy_from_slice(probs);
        self.probs_len = probs.len();
        self.valid = true;
        self.is_rle = false;
        true
    }

    fn from_predef(dist: &[i16], al: u32) -> Self {
        let mut st = Self::empty();
        assert!(st.fill_from_probs(dist, al), "预定义表合法");
        st
    }

    #[inline(always)]
    fn init_state(&self, sym: u32) -> u32 {
        self.state_table[(self.cumul[sym as usize] as usize) & 511] as u32
    }

    #[inline(always)]
    fn step_state(&self, sym: u32, state: &mut u32) -> (u32, u32) {
        let s = sym as usize;
        let x = *state;
        let nb = (x + self.delta_nb[s]) >> 16;
        let bits = x & ((1u32 << nb) - 1);
        let x_occ = x >> nb;
        let next_idx = ((x_occ as i32 + self.sym_off[s] as i32) as usize) & 511;
        *state = self.state_table[next_idx] as u32;
        (nb, bits)
    }
}

struct EncTables {
    ll: SymTable,
    of: SymTable,
    ml: SymTable,
}

impl EncTables {
    fn new() -> Self {
        Self {
            ll: SymTable::from_predef(&LL_DEFAULT, 6),
            of: SymTable::from_predef(&OF_DEFAULT, 5),
            ml: SymTable::from_predef(&ML_DEFAULT, 6),
        }
    }
}

#[derive(Clone, Copy)]
enum CompKind {
    Ll,
    Of,
    Ml,
}

#[derive(Clone, Copy, Default)]
struct SeqBits {
    lc: u8,
    oc: u8,
    mc: u8,
    lb: u8,
    mb: u8,
    ob: u8,
    lv: u16,
    mv: u16,
    ov: u32,
}

#[derive(Clone, Copy)]
enum ModeSel {
    Predef,
    Cur,
    Prev,
}

struct SeqScratch {
    cur: [SymTable; 3],
    prev: [SymTable; 3],
    prev_is_predef: [bool; 3],
    desc: Vec<u8>,
    encoded: Vec<SeqBits>,
    seq_buf: Vec<u8>,
}

impl SeqScratch {
    fn new() -> Self {
        Self {
            cur: [SymTable::empty(), SymTable::empty(), SymTable::empty()],
            prev: [SymTable::empty(), SymTable::empty(), SymTable::empty()],
            prev_is_predef: [false; 3],
            desc: Vec::with_capacity(256),
            encoded: Vec::with_capacity(4096),
            seq_buf: vec![0u8; 64 * 1024],
        }
    }

    fn reset_frame(&mut self) {
        for p in &mut self.prev {
            p.valid = false;
        }
        self.prev_is_predef = [false; 3];
    }
}

fn predef_tables() -> &'static EncTables {
    static TABLES: std::sync::OnceLock<EncTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(EncTables::new)
}

/// 估算复用 `prev` 表（`Repeat_Mode`）是否比新建 `Custom` 表更省总比特。
fn prev_is_cheaper(cnt: &[u32], total_seqs: usize, prev: &SymTable, active_mask: u64) -> bool {
    if !prev.valid || prev.is_rle || (active_mask & !prev.symbols) != 0 {
        return false;
    }
    if total_seqs < 48 {
        return true;
    }
    let prev_size = 1usize << prev.al;
    let mut prev_bits = 0i64;
    let mut ideal_bits = 0i64;
    let total_f = total_seqs.max(1) as u32;
    for (s, &c) in cnt.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let p = prev.probs[s].abs().max(1) as u32;
        let cost_prev = prev.al as i64 - (31 - p.leading_zeros()) as i64;
        let cost_cur = ((31 - total_f.leading_zeros()) - (31 - c.leading_zeros())) as i64;
        prev_bits += c as i64 * cost_prev;
        ideal_bits += c as i64 * cost_cur;
        let _ = prev_size;
    }
    // 自定义分布表头通常约 12..36 字节（~100..280 比特）
    prev_bits <= ideal_bits + 128
}

#[allow(clippy::too_many_arguments)]
fn select_component(
    kind: CompKind,
    cnt: &[u32],
    al: u32,
    shift: u32,
    total_seqs: usize,
    pre: &SymTable,
    cur: &mut SymTable,
    prev: &SymTable,
    modes: &mut u8,
    desc: &mut Vec<u8>,
) -> ModeSel {
    let mut active_mask = 0u64;
    let mut active_cnt = 0u32;
    let mut last_sym = 0u8;
    for (s, &c) in cnt.iter().enumerate() {
        if c > 0 {
            active_mask |= 1u64 << s;
            active_cnt += 1;
            last_sym = s as u8;
        }
    }
    let required = (active_mask & !pre.symbols) != 0;

    // 1) 单符号时优先 RLE（仅需 1 字节，且所有序列位宽为 0）
    if active_cnt == 1 {
        *modes |= 1 << shift;
        desc.push(last_sym);
        cur.set_rle(last_sym);
        return ModeSel::Cur;
    }

    // 2) 短序列且预定义表全覆盖时直接用预定义表（0 字节表头）
    if !required && total_seqs < 48 {
        return ModeSel::Predef;
    }

    // 3) 若上一压缩块的同表全覆盖且估算比特更低，使用 Repeat_Mode（0 字节表头 + 零建表开销）
    if prev_is_cheaper(cnt, total_seqs, prev, active_mask) {
        *modes |= 3 << shift;
        return ModeSel::Prev;
    }

    // 4) 构建 Custom_Mode（复用预分配缓冲）
    let mut probs_i32 = [0i32; 64];
    let n = cnt.len();
    if normalize_into(cnt, al, &mut probs_i32[..n]).is_some() {
        let mark = desc.len();
        if write_dist_into(&probs_i32[..n], al, desc).is_some() {
            let mut probs_i16 = [0i16; 64];
            for i in 0..n {
                probs_i16[i] = probs_i32[i] as i16;
            }
            if cur.fill_from_probs(&probs_i16[..n], al) {
                *modes |= 2 << shift;
                return ModeSel::Cur;
            }
        }
        desc.truncate(mark);
    }

    // 若极罕见情况下 Custom 构建失败且必须替代预定义表，保底 Predef（仅在无超标符号时有效）
    let _ = kind;
    ModeSel::Predef
}

fn write_seqs_into(seqs: &[Seq], pre: &EncTables, sc: &mut SeqScratch, out: &mut Vec<u8>) {
    let mut cl = [0u32; 36];
    let mut co = [0u32; 32];
    let mut cm = [0u32; 53];
    let n = seqs.len();
    sc.encoded.clear();
    sc.encoded.reserve(n);
    for s in seqs {
        let (lc, lb, lv) = ll_code(s.ll);
        let (mc, mb, mv) = ml_code(s.ml);
        let ofv = if s.rep_code != 0 {
            s.rep_code as u32
        } else {
            s.off + 3
        };
        let oc = 31 - ofv.leading_zeros();
        let ov = ofv - (1 << oc);
        cl[lc as usize] += 1;
        co[oc as usize] += 1;
        cm[mc as usize] += 1;
        sc.encoded.push(SeqBits {
            lc: lc as u8,
            oc: oc as u8,
            mc: mc as u8,
            lb: lb as u8,
            mb: mb as u8,
            ob: oc as u8,
            lv: lv as u16,
            mv: mv as u16,
            ov,
        });
    }

    let ll_al = if n < 512 { 8 } else { 9 };
    let ml_al = if n < 512 { 8 } else { 9 };
    let mut modes = 0u8;
    sc.desc.clear();
    let prev_ll = if sc.prev_is_predef[0] {
        &pre.ll
    } else {
        &sc.prev[0]
    };
    let prev_of = if sc.prev_is_predef[1] {
        &pre.of
    } else {
        &sc.prev[1]
    };
    let prev_ml = if sc.prev_is_predef[2] {
        &pre.ml
    } else {
        &sc.prev[2]
    };
    let sel_ll = select_component(
        CompKind::Ll,
        &cl,
        ll_al,
        6,
        n,
        &pre.ll,
        &mut sc.cur[0],
        prev_ll,
        &mut modes,
        &mut sc.desc,
    );
    let sel_of = select_component(
        CompKind::Of,
        &co,
        8,
        4,
        n,
        &pre.of,
        &mut sc.cur[1],
        prev_of,
        &mut modes,
        &mut sc.desc,
    );
    let sel_ml = select_component(
        CompKind::Ml,
        &cm,
        ml_al,
        2,
        n,
        &pre.ml,
        &mut sc.cur[2],
        prev_ml,
        &mut modes,
        &mut sc.desc,
    );

    let tl = match sel_ll {
        ModeSel::Predef => &pre.ll,
        ModeSel::Cur => &sc.cur[0],
        ModeSel::Prev => prev_ll,
    };
    let to = match sel_of {
        ModeSel::Predef => &pre.of,
        ModeSel::Cur => &sc.cur[1],
        ModeSel::Prev => prev_of,
    };
    let tm = match sel_ml {
        ModeSel::Predef => &pre.ml,
        ModeSel::Cur => &sc.cur[2],
        ModeSel::Prev => prev_ml,
    };

    out.push(modes);
    out.extend_from_slice(&sc.desc);

    let need = n * 12 + 32;
    if sc.seq_buf.len() < need {
        sc.seq_buf.resize(need, 0);
    }
    let buf = &mut sc.seq_buf[..need];
    let mut wp = 0usize;
    let mut acc = 0u64;
    let mut nbits = 0u32;

    macro_rules! flush64 {
        () => {{
            buf[wp..wp + 8].copy_from_slice(&acc.to_le_bytes());
            let bytes = ((nbits >> 3) & 7) as usize;
            wp += bytes;
            acc >>= bytes << 3;
            nbits &= 7;
        }};
    }

    let last = sc.encoded[n - 1];
    let mut stl = tl.init_state(last.lc as u32);
    let mut sto = to.init_state(last.oc as u32);
    let mut stm = tm.init_state(last.mc as u32);

    acc |= (last.lv as u64) | ((last.mv as u64) << last.lb);
    nbits += last.lb as u32 + last.mb as u32;
    flush64!();
    acc |= (last.ov as u64) << nbits;
    nbits += last.ob as u32;
    flush64!();

    for &sb in sc.encoded[..n - 1].iter().rev() {
        if wp + 16 > buf.len() {
            break;
        }
        let (n1, v1) = to.step_state(sb.oc as u32, &mut sto);
        let (n2, v2) = tm.step_state(sb.mc as u32, &mut stm);
        let (n3, v3) = tl.step_state(sb.lc as u32, &mut stl);

        let fse = (v1 | (v2 << n1) | (v3 << (n1 + n2))) as u64;
        let fse_nb = n1 + n2 + n3;
        let lb = sb.lb as u32;
        let mb = sb.mb as u32;
        let ob = sb.ob as u32;
        let total = fse_nb + lb + mb + ob;
        if total <= 56 {
            let bits = fse
                | ((sb.lv as u64) << fse_nb)
                | ((sb.mv as u64) << (fse_nb + lb))
                | ((sb.ov as u64) << (fse_nb + lb + mb));
            acc |= bits << nbits;
            nbits += total;
            flush64!();
        } else {
            acc |= (fse | ((sb.lv as u64) << fse_nb)) << nbits;
            nbits += fse_nb + lb;
            flush64!();
            acc |= ((sb.mv as u64) | ((sb.ov as u64) << mb)) << nbits;
            nbits += mb + ob;
            flush64!();
        }
    }

    let final_bits =
        (stm - tm.size) | ((sto - to.size) << tm.al) | ((stl - tl.size) << (tm.al + to.al));
    let final_nb = tm.al + to.al + tl.al;
    acc |= (final_bits as u64) << nbits;
    nbits += final_nb;
    flush64!();
    acc |= 1u64 << nbits;
    nbits += 1;
    while nbits > 0 {
        buf[wp] = acc as u8;
        wp += 1;
        acc >>= 8;
        nbits = nbits.saturating_sub(8);
    }
    out.extend_from_slice(&buf[..wp]);
}

#[inline]
fn commit_seq_tables(sc: &mut SeqScratch, modes: u8) {
    for (idx, shift) in [(0usize, 6u32), (1, 4), (2, 2)] {
        match (modes >> shift) & 3 {
            0 => sc.prev_is_predef[idx] = true,
            1 | 2 => {
                sc.prev_is_predef[idx] = false;
                std::mem::swap(&mut sc.cur[idx], &mut sc.prev[idx]);
            }
            _ => {}
        }
    }
}

#[inline]
fn raw_block(out: &mut Vec<u8>, b: &[u8], last: bool) {
    let h = ((b.len() << 3) as u32) | (last as u32);
    out.extend_from_slice(&h.to_le_bytes()[..3]);
    out.extend_from_slice(b);
}

#[inline]
fn rle_block(out: &mut Vec<u8>, len: usize, val: u8, last: bool) {
    let h = ((len << 3) as u32) | 2 | (last as u32);
    out.extend_from_slice(&h.to_le_bytes()[..3]);
    out.push(val);
}

#[inline]
fn is_all_byte(b: &[u8]) -> Option<u8> {
    let first = *b.first()?;
    let probe = b.len().min(64);
    if b[..probe].iter().all(|&x| x == first) && b[probe..].iter().all(|&x| x == first) {
        Some(first)
    } else {
        None
    }
}

/// 编码帧头：依据 `data_len` 选择最紧凑的 1/2/4/8 字节内容长度及可选窗口描述。
fn write_frame_header(out: &mut Vec<u8>, data_len: usize, opts: &CompressOptions) {
    out.extend_from_slice(&0xFD2FB528u32.to_le_bytes());
    let chk_bit = if opts.checksum { 0x04u8 } else { 0u8 };
    let need_window_desc = opts
        .window_log
        .is_some_and(|w| data_len > (1usize << w.clamp(10, 30)));
    let single_bit = if need_window_desc { 0u8 } else { 0x20u8 };
    if data_len <= 255 && !need_window_desc {
        out.push(single_bit | chk_bit);
        out.push(data_len as u8);
    } else if (256..=65791).contains(&data_len) {
        out.push(0x40 | single_bit | chk_bit);
        if need_window_desc {
            out.push(encode_window_descriptor(opts.window_log.unwrap_or(20)));
        }
        out.extend_from_slice(&((data_len - 256) as u16).to_le_bytes());
    } else if data_len <= u32::MAX as usize {
        out.push(0x80 | single_bit | chk_bit);
        if need_window_desc {
            out.push(encode_window_descriptor(opts.window_log.unwrap_or(20)));
        }
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
    } else {
        out.push(0xC0 | single_bit | chk_bit);
        if need_window_desc {
            out.push(encode_window_descriptor(opts.window_log.unwrap_or(20)));
        }
        out.extend_from_slice(&(data_len as u64).to_le_bytes());
    }
}

#[inline]
fn encode_window_descriptor(window_log: u32) -> u8 {
    let w = window_log.clamp(10, 30);
    ((w - 10) << 3) as u8
}

/// 单线程 Zstd 编码内核状态（可由 [`Compressor`] 复用，或由多线程池按核持有）。
pub(crate) struct EncCore {
    fast: Option<(FastMode, bool, bool)>, // (mode, tagged, wide)
    chain: Option<Chain<u32>>,
    wide_chain: Option<Chain<usize>>,
    seqs: Vec<Seq>,
    lits: Vec<u8>,
    body: Vec<u8>,
    huff: HuffScratch,
    seq_sc: SeqScratch,
}

impl EncCore {
    pub(crate) fn new() -> Self {
        Self {
            fast: None,
            chain: None,
            wide_chain: None,
            seqs: Vec::with_capacity(4096),
            lits: Vec::with_capacity(32 * 1024),
            body: Vec::with_capacity(64 * 1024),
            huff: HuffScratch::new(),
            seq_sc: SeqScratch::new(),
        }
    }
}

/// 可复用的 Zstd 压缩上下文，跨次调用保留哈希表、链表、DP 状态与熵编码暂存；
/// 通过代际戳记实现无表清零的毫秒级小包与流式反复压缩，启用多线程选项时复用每核工作上下文。
pub struct Compressor {
    core: EncCore,
    mt_workers: Vec<EncCore>,
    mt_slots: Vec<Vec<u8>>,
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor {
    /// 创建一个可复用的空压缩上下文（按首次压缩输入延迟分配最小工作集）。
    pub fn new() -> Self {
        Self {
            core: EncCore::new(),
            mt_workers: Vec::new(),
            mt_slots: Vec::new(),
        }
    }

    /// 压缩 `data` 成完整 Zstd 帧（带默认 XXH64 校验和）。
    pub fn compress(&mut self, data: &[u8], level: i32) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() / 2 + 32);
        self.compress_into(data, &mut out, &CompressOptions::new(level));
        out
    }

    /// 按自定义选项压缩 `data` 并返回新建帧缓冲。
    pub fn compress_with(&mut self, data: &[u8], opts: &CompressOptions) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() / 2 + 32);
        self.compress_into(data, &mut out, opts);
        out
    }

    /// 按 `opts` 将 `data` 压缩成一个或多个有效 Zstd 帧，追加写入 `out`。
    pub fn compress_into(&mut self, data: &[u8], out: &mut Vec<u8>, opts: &CompressOptions) {
        if opts.threads == 1 || data.len() < BLOCK * 4 {
            self.core.compress_single_frame(data, out, opts);
        } else {
            crate::mt::compress_parallel(
                &mut self.core,
                &mut self.mt_workers,
                &mut self.mt_slots,
                data,
                out,
                opts,
            );
        }
    }
}

impl EncCore {
    pub(crate) fn compress_single_frame(
        &mut self,
        data: &[u8],
        out: &mut Vec<u8>,
        opts: &CompressOptions,
    ) {
        write_frame_header(out, data.len(), opts);
        if data.is_empty() {
            raw_block(out, &[], true);
            if opts.checksum {
                out.extend_from_slice(&(crate::xxh::xxh64(data) as u32).to_le_bytes());
            }
            return;
        }
        let level = opts.level;
        let win_limit = opts.window_limit();
        let wide = data.len() > u32::MAX as usize;
        let mut budget = (level > 0).then(|| Budget::new(data.len(), level));
        if let Some(b) = budget.as_mut()
            && let Some(wlog) = opts.window_log
        {
            b.window = b.window.min(1usize << wlog.clamp(10, 30));
        }

        let mut use_fast = false;
        let mut fast_cfg = FastCfg::for_level(level);
        let mut chain_cfg = None;
        if let Some(ref b) = budget {
            if level < 5 {
                use_fast = true;
                let active_bits = b.hash_bits.max(1);
                let reuse_ok = self
                    .fast
                    .as_ref()
                    .is_some_and(|(_, t, w)| *t == b.tagged && *w == wide);
                if !reuse_ok {
                    self.fast = Some((
                        FastMode::new(fast_cfg.dual, active_bits, b.tagged, wide),
                        b.tagged,
                        wide,
                    ));
                }
                if let Some((ref mut f, _, _)) = self.fast {
                    f.prepare(data.len(), active_bits, fast_cfg.dual, win_limit);
                }
            } else {
                let cc = ChainCfg::from_budget(b, level, win_limit);
                chain_cfg = Some(cc);
                if wide {
                    let ch = self.wide_chain.get_or_insert_with(|| Chain::new(b));
                    ch.prepare(data.len(), b, cc.bt);
                } else {
                    let ch = self.chain.get_or_insert_with(|| Chain::new(b));
                    ch.prepare(data.len(), b, cc.bt);
                }
            }
        } else {
            fast_cfg.dual = false;
        }

        let pre = (level > 0).then(predef_tables);
        self.huff.reset_frame();
        self.seq_sc.reset_frame();
        let mut rep = [1usize, 4, 8];
        let mut start = 0usize;
        while start < data.len() {
            let end = (start + BLOCK).min(data.len());
            let last = end == data.len();
            let blk = &data[start..end];
            if level <= 0 {
                if blk.len() >= 5
                    && let Some(val) = is_all_byte(blk)
                {
                    rle_block(out, blk.len(), val, last);
                    start = end;
                    continue;
                }
                raw_block(out, blk, last);
                start = end;
                continue;
            }
            if blk.len() >= 5
                && let Some(val) = is_all_byte(blk)
            {
                rle_block(out, blk.len(), val, last);
                start = end;
                continue;
            }

            self.seqs.clear();
            self.lits.clear();
            self.body.clear();
            let saved_rep = rep;
            let anchor = if use_fast {
                let (ref mut f, _, _) = *self.fast.as_mut().expect("fast init");
                f.parse(
                    data,
                    (start, end),
                    fast_cfg,
                    &mut rep,
                    &mut self.seqs,
                    &mut self.lits,
                )
            } else if let Some(cc) = chain_cfg {
                if wide {
                    self.wide_chain.as_mut().expect("wide chain init").parse(
                        data,
                        (start, end),
                        cc,
                        &mut rep,
                        &mut self.seqs,
                        &mut self.lits,
                    )
                } else {
                    self.chain.as_mut().expect("chain init").parse(
                        data,
                        (start, end),
                        cc,
                        &mut rep,
                        &mut self.seqs,
                        &mut self.lits,
                    )
                }
            } else {
                start
            };
            self.lits.extend_from_slice(&data[anchor..end]);

            write_literals_into(&self.lits, &mut self.body, &mut self.huff, true, level <= 1);

            let seq_modes = if self.seqs.is_empty() {
                self.body.push(0);
                None
            } else {
                let n = self.seqs.len();
                if n < 128 {
                    self.body.push(n as u8);
                } else if n < 0x7F00 {
                    self.body.push((n >> 8) as u8 + 128);
                    self.body.push(n as u8);
                } else {
                    self.body.push(255);
                    self.body
                        .extend_from_slice(&((n - 0x7F00) as u16).to_le_bytes());
                }
                let seq_hdr_pos = self.body.len();
                write_seqs_into(
                    &self.seqs,
                    pre.expect("positive level"),
                    &mut self.seq_sc,
                    &mut self.body,
                );
                Some(self.body[seq_hdr_pos])
            };

            if self.body.len() >= blk.len() {
                // Raw 块不会触发解密器的序列状态转移，仅恢复重复偏移历史；
                // `huff` 和 `seq_sc` 的候选仅在提交时生效。
                rep = saved_rep;
                raw_block(out, blk, last);
            } else {
                let h = ((self.body.len() << 3) as u32) | 4 | (last as u32);
                out.extend_from_slice(&h.to_le_bytes()[..3]);
                out.extend_from_slice(&self.body);
                self.huff.commit_block();
                if let Some(modes) = seq_modes {
                    commit_seq_tables(&mut self.seq_sc, modes);
                }
            }
            start = end;
        }
        if opts.checksum {
            out.extend_from_slice(&(crate::xxh::xxh64(data) as u32).to_le_bytes());
        }
    }
}

std::thread_local! {
    static ENC_TLS: std::cell::RefCell<Option<Box<Compressor>>> = const { std::cell::RefCell::new(None) };
}

/// 压缩成一个 Zstd 帧。`level <= 0` 只存放 Raw/RLE 块；正等级随输入和硬件自适应选择匹配策略与工作集。
pub fn compress(data: &[u8], level: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 32);
    compress_into(data, &mut out, &CompressOptions::new(level));
    out
}

/// 按自定义 [`CompressOptions`] 压缩 `data`（支持自定义校验和、窗口上限与多线程并行）并返回压缩缓冲。
pub fn compress_with(data: &[u8], opts: &CompressOptions) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 32);
    compress_into(data, &mut out, opts);
    out
}

/// 同 [`compress`]，但按自定义 [`CompressOptions`] 写入调用方缓冲 `out`（可复用容量或拼接多帧）。
pub fn compress_into(data: &[u8], out: &mut Vec<u8>, opts: &CompressOptions) {
    ENC_TLS.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            let cmp = guard.get_or_insert_with(|| Box::new(Compressor::new()));
            cmp.compress_into(data, out, opts);
        } else {
            let mut cmp = Compressor::new();
            cmp.compress_into(data, out, opts);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_large_offsets_use_rle_or_custom_table() {
        let pre = predef_tables();
        let mut sc = SeqScratch::new();
        // 单一超大稀疏偏移启用 RLE（mode = 1）
        {
            let seqs = [Seq {
                ll: 0,
                ml: 4,
                off: 1 << 29,
                rep_code: 0,
            }];
            let mut body = Vec::new();
            write_seqs_into(&seqs, pre, &mut sc, &mut body);
            assert_eq!((body[0] >> 4) & 3, 1);
            assert_eq!(body[1], 0);
            assert_eq!(body[2], 31 - ((1u32 << 29) + 3).leading_zeros() as u8);
        }
        // 两个不同超大稀疏偏移启用自定义 FSE 表（mode = 2）
        sc.reset_frame();
        let seqs = [
            Seq {
                ll: 0,
                ml: 4,
                off: 1 << 29,
                rep_code: 0,
            },
            Seq {
                ll: 0,
                ml: 4,
                off: u32::MAX - 3,
                rep_code: 0,
            },
        ];
        let mut body = Vec::new();
        write_seqs_into(&seqs, pre, &mut sc, &mut body);
        assert_eq!((body[0] >> 4) & 3, 2);
        let (decoded, _used) = fse::read_table(&sc.desc[1..], 31, 8).unwrap();
        let sym1 = 31 - ((1u32 << 29) + 3).leading_zeros();
        let sym2 = 31 - u32::MAX.leading_zeros();
        assert!(decoded.cells.iter().any(|c| c.sym as u32 == sym1));
        assert!(decoded.cells.iter().any(|c| c.sym as u32 == sym2));
    }
}
