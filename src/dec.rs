//! 帧、块、字面量段、序列段解码（可复用上下文 + 单遍融合序列解码执行）。
use std::sync::OnceLock;

use crate::bits::{RevBits, RevCursor};
use crate::fse;
use crate::huff::{self, HuffTable};
use crate::{Error, Result};

pub(crate) const MAX_BLOCK: usize = 128 * 1024;

pub(crate) const LL_BASE: [(u32, u32); 36] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 1),
    (18, 1),
    (20, 1),
    (22, 1),
    (24, 2),
    (28, 2),
    (32, 3),
    (40, 3),
    (48, 4),
    (64, 6),
    (128, 7),
    (256, 8),
    (512, 9),
    (1024, 10),
    (2048, 11),
    (4096, 12),
    (8192, 13),
    (16384, 14),
    (32768, 15),
    (65536, 16),
];
pub(crate) const ML_BASE: [(u32, u32); 53] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 0),
    (17, 0),
    (18, 0),
    (19, 0),
    (20, 0),
    (21, 0),
    (22, 0),
    (23, 0),
    (24, 0),
    (25, 0),
    (26, 0),
    (27, 0),
    (28, 0),
    (29, 0),
    (30, 0),
    (31, 0),
    (32, 0),
    (33, 0),
    (34, 0),
    (35, 1),
    (37, 1),
    (39, 1),
    (41, 1),
    (43, 2),
    (47, 2),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 5),
    (131, 7),
    (259, 8),
    (515, 9),
    (1027, 10),
    (2051, 11),
    (4099, 12),
    (8195, 13),
    (16387, 14),
    (32771, 15),
    (65539, 16),
];

pub(crate) const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
pub(crate) const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
pub(crate) const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// 序列码展开后的解码格：状态转移（base、nb）加上额外位数和基值。
#[derive(Clone, Copy, Default)]
struct SCell {
    base: u16,
    nb: u8,
    eb: u8,
    val: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableSrc {
    None,
    Predef,
    Custom,
}

#[derive(Clone, Copy)]
enum Kind {
    Ll,
    Of,
    Ml,
}

struct Predefs {
    ll: [SCell; 512],
    of: [SCell; 256],
    ml: [SCell; 512],
}

fn predefs() -> &'static Predefs {
    static PREDEF: OnceLock<Predefs> = OnceLock::new();
    PREDEF.get_or_init(|| {
        let mut f_ll = [fse::Cell::default(); 64];
        let mut f_of = [fse::Cell::default(); 32];
        let mut f_ml = [fse::Cell::default(); 64];
        fse::build_into(&LL_DEFAULT, 6, &mut f_ll).expect("预定义表合法");
        fse::build_into(&OF_DEFAULT, 5, &mut f_of).expect("预定义表合法");
        fse::build_into(&ML_DEFAULT, 6, &mut f_ml).expect("预定义表合法");
        let mut ll = [SCell::default(); 512];
        let mut of = [SCell::default(); 256];
        let mut ml = [SCell::default(); 512];
        expand_cells(&f_ll, Kind::Ll, &mut ll[..64]);
        expand_cells(&f_of, Kind::Of, &mut of[..32]);
        expand_cells(&f_ml, Kind::Ml, &mut ml[..64]);
        let (ll_head, ll_tail) = ll.split_at_mut(64);
        for c in ll_tail.as_chunks_mut::<64>().0 {
            c.copy_from_slice(ll_head);
        }
        let (of_head, of_tail) = of.split_at_mut(32);
        for c in of_tail.as_chunks_mut::<32>().0 {
            c.copy_from_slice(of_head);
        }
        let (ml_head, ml_tail) = ml.split_at_mut(64);
        for c in ml_tail.as_chunks_mut::<64>().0 {
            c.copy_from_slice(ml_head);
        }
        Predefs { ll, of, ml }
    })
}

fn expand_cells(src: &[fse::Cell], kind: Kind, dst: &mut [SCell]) {
    for (d, c) in dst.iter_mut().zip(src.iter()) {
        let sym = c.sym as usize;
        let (val, eb) = match kind {
            Kind::Ll => LL_BASE[sym.min(LL_BASE.len() - 1)],
            Kind::Ml => ML_BASE[sym.min(ML_BASE.len() - 1)],
            Kind::Of => (1u32 << sym.min(31), sym.min(31) as u32),
        };
        *d = SCell {
            base: c.base,
            nb: c.nb,
            eb: eb as u8,
            val,
        };
    }
}

pub(crate) struct State {
    hist: [usize; 3],
    huff_valid: bool,
    huff: HuffTable,
    ll_src: TableSrc,
    of_src: TableSrc,
    ml_src: TableSrc,
    ll_al: u32,
    of_al: u32,
    ml_al: u32,
    ll_cells: [SCell; 512],
    of_cells: [SCell; 256],
    ml_cells: [SCell; 512],
    lits: Vec<u8>,
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            hist: [1, 4, 8],
            huff_valid: false,
            huff: HuffTable::default(),
            ll_src: TableSrc::None,
            of_src: TableSrc::None,
            ml_src: TableSrc::None,
            ll_al: 0,
            of_al: 0,
            ml_al: 0,
            ll_cells: [SCell::default(); 512],
            of_cells: [SCell::default(); 256],
            ml_cells: [SCell::default(); 512],
            lits: Vec::new(),
        }
    }

    fn reset_frame(&mut self) {
        self.hist = [1, 4, 8];
        self.huff_valid = false;
        self.ll_src = TableSrc::None;
        self.of_src = TableSrc::None;
        self.ml_src = TableSrc::None;
    }
}

/// 解压参数配置（支持多帧输入自动/指定多线程并行解码）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecompressOptions {
    /// 并行解码线程数：`0` 表示自动探测多核（输入含多帧时生效），`1` 表示单线程（默认）。
    pub threads: usize,
}

impl Default for DecompressOptions {
    fn default() -> Self {
        Self { threads: 1 }
    }
}

impl DecompressOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }
}

/// 可复用的 Zstd 解压器，跨次调用保留 Huffman/FSE 解码表与字面量暂存，避免重复分配和清零；
/// 启用多线程选项且遇到多帧输入时可复用并行的每核上下文。
pub struct Decompressor {
    state: Box<State>,
    mt_states: Vec<Box<State>>,
    mt_slots: Vec<Vec<u8>>,
    mt_idle: u8,
}

impl Default for Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor {
    /// 创建一个新的可复用解压器。
    pub fn new() -> Self {
        Self {
            state: Box::new(State::new()),
            mt_states: Vec::new(),
            mt_slots: Vec::new(),
            mt_idle: 0,
        }
    }

    /// 释放并行工作上下文与临时输出槽位，保留当前解压器可继续复用。
    pub fn shrink_to_fit(&mut self) {
        self.state.lits.clear();
        self.state.lits.shrink_to_fit();
        self.mt_states.clear();
        self.mt_states.shrink_to_fit();
        self.mt_slots.clear();
        self.mt_slots.shrink_to_fit();
        self.mt_idle = 0;
    }

    #[inline]
    fn tick_mt_idle(&mut self) {
        if !self.mt_states.is_empty() || !self.mt_slots.is_empty() {
            self.mt_idle = self.mt_idle.saturating_add(1);
            if self.mt_idle >= 8 {
                self.mt_states.clear();
                self.mt_states.shrink_to_fit();
                self.mt_slots.clear();
                self.mt_slots.shrink_to_fit();
                self.mt_idle = 0;
            }
        }
    }

    /// 解压 `data`（可含多帧、跳过帧），输出不超过 `max_out` 字节。
    pub fn decompress(&mut self, data: &[u8], max_out: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.decompress_into(data, &mut out, max_out)?;
        Ok(out)
    }

    /// 同 [`Self::decompress`]，但按 [`DecompressOptions`] 配置并行线程等选项。
    pub fn decompress_with(
        &mut self,
        data: &[u8],
        opts: &DecompressOptions,
        max_out: usize,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.decompress_into_with(data, &mut out, opts, max_out)?;
        Ok(out)
    }

    /// 同 [`Self::decompress`]，但写入调用方的缓冲；出错时 `out` 里留着已解好的完整块（超限时按块截断）。
    pub fn decompress_into(
        &mut self,
        data: &[u8],
        out: &mut Vec<u8>,
        max_out: usize,
    ) -> Result<()> {
        self.tick_mt_idle();
        decompress_with_state(&mut self.state, data, out, max_out)
    }

    /// 同 [`Self::decompress_into`]，并在多线程开启且输入含多个独立帧时并发解压各帧。
    pub fn decompress_into_with(
        &mut self,
        data: &[u8],
        out: &mut Vec<u8>,
        opts: &DecompressOptions,
        max_out: usize,
    ) -> Result<()> {
        if opts.threads == 1 || data.len() < MAX_BLOCK {
            self.tick_mt_idle();
            decompress_with_state(&mut self.state, data, out, max_out)
        } else {
            crate::mt::decompress_parallel(
                &mut self.state,
                &mut self.mt_states,
                &mut self.mt_slots,
                &mut self.mt_idle,
                data,
                out,
                opts,
                max_out,
            )
        }
    }
}

/// 解压 `data`（可含多帧、跳过帧），输出不超过 `max_out` 字节。
pub fn decompress(data: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decompress_into(data, &mut out, max_out)?;
    Ok(out)
}

/// 按 [`DecompressOptions`] 解压 `data`（多帧输入时可自动或按指定线程并行）。
pub fn decompress_with(data: &[u8], opts: &DecompressOptions, max_out: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decompress_into_with(data, &mut out, opts, max_out)?;
    Ok(out)
}

std::thread_local! {
    static DEC_TLS: std::cell::RefCell<Option<Box<Decompressor>>> = const { std::cell::RefCell::new(None) };
}

/// 同 [`decompress`]，但写入调用方的缓冲；出错时 `out` 里留着已解好的完整块（超限时按块截断）。
pub fn decompress_into(data: &[u8], out: &mut Vec<u8>, max_out: usize) -> Result<()> {
    decompress_into_with(data, out, &DecompressOptions::default(), max_out)
}

/// 同 [`decompress_with`]，写入调用方缓冲 `out`。
pub fn decompress_into_with(
    data: &[u8],
    out: &mut Vec<u8>,
    opts: &DecompressOptions,
    max_out: usize,
) -> Result<()> {
    DEC_TLS.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            let dec = guard.get_or_insert_with(|| Box::new(Decompressor::new()));
            dec.decompress_into_with(data, out, opts, max_out)
        } else {
            let mut dec = Decompressor::new();
            dec.decompress_into_with(data, out, opts, max_out)
        }
    })
}

pub(crate) fn decompress_with_state(
    st: &mut State,
    data: &[u8],
    out: &mut Vec<u8>,
    max_out: usize,
) -> Result<()> {
    let mut pos = 0usize;
    if data.is_empty() {
        return Err(Error::Corrupt("empty input"));
    }
    while pos < data.len() {
        let magic = data
            .get(pos..pos + 4)
            .ok_or(Error::Corrupt("truncated magic"))?;
        let m = u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]);
        pos += 4;
        if (0x184D2A50..=0x184D2A5F).contains(&m) {
            let sz = data
                .get(pos..pos + 4)
                .ok_or(Error::Corrupt("truncated skip frame"))?;
            let n = u32::from_le_bytes([sz[0], sz[1], sz[2], sz[3]]) as usize;
            pos = pos
                .checked_add(4 + n)
                .filter(|&p| p <= data.len())
                .ok_or(Error::Corrupt("skip frame size"))?;
            continue;
        }
        if m != 0xFD2FB528 {
            return Err(Error::Corrupt("bad magic"));
        }
        pos += frame(&data[pos..], st, out, max_out)?;
    }
    Ok(())
}

pub(crate) struct FrameHeader {
    pub header_len: usize,
    pub checksum: bool,
    pub uncomp_len: Option<u64>,
    pub window_size: usize,
}

impl FrameHeader {
    #[inline]
    pub(crate) fn block_max(&self) -> usize {
        self.window_size.min(MAX_BLOCK)
    }
}

/// 解析 Zstd 帧头（不含魔数），返回帧头长度、校验位、可选原始内容长度与窗口上限。
pub(crate) fn parse_frame_header(d: &[u8]) -> Result<FrameHeader> {
    let fd = *d.first().ok_or(Error::Corrupt("truncated frame header"))?;
    let single = fd & 0x20 != 0;
    if fd & 0x08 != 0 {
        return Err(Error::Corrupt("reserved bit set"));
    }
    let checksum = fd & 0x04 != 0;
    let did = [0usize, 1, 2, 4][(fd & 3) as usize];
    let fcs = match fd >> 6 {
        0 => single as usize,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut p = 1 + usize::from(!single);
    let hdr = d
        .get(p..p + did + fcs)
        .ok_or(Error::Corrupt("truncated frame header"))?;
    if hdr[..did].iter().any(|&b| b != 0) {
        return Err(Error::Unsupported("dictionary"));
    }
    let uncomp_len = if fcs > 0 {
        let mut v = 0u64;
        for (i, &b) in hdr[did..].iter().enumerate() {
            v |= (b as u64) << (8 * i);
        }
        if fcs == 2 {
            v += 256;
        }
        Some(v)
    } else {
        None
    };
    let window_size = if single {
        usize::try_from(uncomp_len.unwrap_or(0)).unwrap_or(usize::MAX)
    } else {
        let wd = d[1];
        let exp = (wd >> 3) as u32;
        let mant = (wd & 7) as u64;
        let base = 1u64 << (10 + exp);
        let w = base + (base >> 3) * mant;
        usize::try_from(w).unwrap_or(usize::MAX)
    };
    p += did + fcs;
    Ok(FrameHeader {
        header_len: p,
        checksum,
        uncomp_len,
        window_size,
    })
}

/// 解一帧（不含魔数），返回消耗的字节数。
fn frame(d: &[u8], st: &mut State, out: &mut Vec<u8>, max_out: usize) -> Result<usize> {
    let fh = parse_frame_header(d)?;
    let checksum = fh.checksum;
    let block_max = fh.block_max();
    let win_limit = fh.window_size;
    let mut frame_end = None;
    if let Some(v) = fh.uncomp_len {
        let avail = max_out.saturating_sub(out.len());
        if v > avail as u64 {
            return Err(Error::TooLarge);
        }
        // 按帧声明的原始长度预留容量，同时以输入长度的合理膨胀上限防伪造头 OOM。
        let reserve_cap = (v as usize).min(d.len().saturating_mul(2048).max(MAX_BLOCK));
        out.reserve(reserve_cap);
        frame_end = Some(out.len() + v as usize);
    }
    let mut p = fh.header_len;
    let start = out.len();
    st.reset_frame();
    loop {
        let h = d
            .get(p..p + 3)
            .ok_or(Error::Corrupt("truncated block header"))?;
        let v = h[0] as usize | (h[1] as usize) << 8 | (h[2] as usize) << 16;
        p += 3;
        let last = v & 1 != 0;
        let size = v >> 3;
        match (v >> 1) & 3 {
            0 => {
                if size > block_max {
                    return Err(Error::Corrupt("raw block too large"));
                }
                let b = d
                    .get(p..p + size)
                    .ok_or(Error::Corrupt("truncated raw block"))?;
                if let Some(fe) = frame_end
                    && out.len() + size > fe
                {
                    return Err(Error::Corrupt("frame content size mismatch"));
                }
                push(out, b, max_out)?;
                p += size;
            }
            1 => {
                if size > block_max {
                    return Err(Error::Corrupt("rle block too large"));
                }
                let b = *d.get(p).ok_or(Error::Corrupt("truncated rle block"))?;
                if let Some(fe) = frame_end
                    && out.len() + size > fe
                {
                    return Err(Error::Corrupt("frame content size mismatch"));
                }
                if out.len() + size > max_out {
                    return Err(Error::TooLarge);
                }
                out.resize(out.len() + size, b);
                p += 1;
            }
            2 => {
                if size > block_max {
                    return Err(Error::Corrupt("block too large"));
                }
                let b = d
                    .get(p..p + size)
                    .ok_or(Error::Corrupt("truncated block"))?;
                let frame_rem = frame_end.map(|fe| fe.saturating_sub(out.len()));
                block(b, st, out, start, max_out, frame_rem, block_max, win_limit)?;
                p += size;
            }
            _ => return Err(Error::Corrupt("reserved block type")),
        }
        if last {
            break;
        }
    }
    if let Some(fe) = frame_end
        && out.len() != fe
    {
        return Err(Error::Corrupt("frame content size mismatch"));
    }
    if checksum {
        let c = d
            .get(p..p + 4)
            .ok_or(Error::Corrupt("truncated checksum"))?;
        let want = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        if crate::xxh::xxh64(&out[start..]) as u32 != want {
            return Err(Error::Corrupt("checksum mismatch"));
        }
        p += 4;
    }
    Ok(p)
}

fn push(out: &mut Vec<u8>, b: &[u8], max_out: usize) -> Result<()> {
    if out.len() + b.len() > max_out {
        return Err(Error::TooLarge);
    }
    out.extend_from_slice(b);
    Ok(())
}

fn literals<'a>(
    b: &'a [u8],
    huff: &mut HuffTable,
    huff_valid: &mut bool,
    scratch: &mut Vec<u8>,
) -> Result<(Option<&'a [u8]>, usize, usize)> {
    let b0 = b[0];
    let ty = b0 & 3;
    let sf = (b0 >> 2) & 3;
    if ty < 2 {
        let (n, hl) = match sf {
            0 | 2 => ((b0 >> 3) as usize, 1),
            1 => {
                let x = b.get(1).ok_or(Error::Corrupt("literals header"))?;
                ((b0 >> 4) as usize | (*x as usize) << 4, 2)
            }
            _ => {
                let x = b.get(1..3).ok_or(Error::Corrupt("literals header"))?;
                (
                    (b0 >> 4) as usize | (x[0] as usize) << 4 | (x[1] as usize) << 12,
                    3,
                )
            }
        };
        if n > MAX_BLOCK {
            return Err(Error::Corrupt("literals too large"));
        }
        if ty == 0 {
            let s = b
                .get(hl..hl + n)
                .ok_or(Error::Corrupt("raw literals truncated"))?;
            if let Some(padded) = b.get(hl..hl + n + 16) {
                return Ok((Some(padded), n, hl + n));
            }
            if scratch.len() < n + 16 {
                scratch.resize(n + 16, 0);
            }
            scratch[..n].copy_from_slice(s);
            return Ok((None, n, hl + n));
        }
        let val = *b.get(hl).ok_or(Error::Corrupt("rle literal truncated"))?;
        if scratch.len() < n + 16 {
            scratch.resize(n + 16, val);
        }
        scratch[..n + 16].fill(val);
        return Ok((None, n, hl + 1));
    }
    let (hl, bits, four) = match sf {
        0 => (3, 10, false),
        1 => (3, 10, true),
        2 => (4, 14, true),
        _ => (5, 18, true),
    };
    let h = b.get(..hl).ok_or(Error::Corrupt("literals header"))?;
    let mut v = 0u64;
    for (i, &x) in h.iter().enumerate() {
        v |= (x as u64) << (8 * i);
    }
    let regen = ((v >> 4) & ((1 << bits) - 1)) as usize;
    let comp = ((v >> (4 + bits)) & ((1 << bits) - 1)) as usize;
    if regen > MAX_BLOCK {
        return Err(Error::Corrupt("literals too large"));
    }
    let body = b
        .get(hl..hl + comp)
        .ok_or(Error::Corrupt("literals truncated"))?;
    let streams = if ty == 2 {
        let used = huff::read_tree_into(body, huff)?;
        *huff_valid = true;
        &body[used..]
    } else {
        if !*huff_valid {
            return Err(Error::Corrupt("missing huffman table"));
        }
        body
    };
    if scratch.len() < regen + 16 {
        // 一次性扩容并保留已初始化长度（带 +16 安全余量），后续块不再清零。
        scratch.resize(regen + 16, 0);
    }
    huff::decode_into(huff, streams, four, &mut scratch[..regen])?;
    Ok((None, regen, hl + comp))
}

fn build_seq_cells_into(probs: &[i16], al: u32, kind: Kind, out: &mut [SCell]) -> Result<()> {
    let size = 1usize << al;
    let cells = &mut out[..size];
    let mut syms = [0u8; 512];
    let mut high = size;
    let mut next = [0u32; 64];
    for (s, &p) in probs.iter().enumerate() {
        if p == -1 {
            high -= 1;
            syms[high] = s as u8;
            next[s] = 1;
        } else {
            next[s] = p.max(0) as u32;
        }
    }
    let step = (size >> 1) + (size >> 3) + 3;
    let mask = size - 1;
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
        return Err(Error::Corrupt("fse table spread"));
    }
    for (d, &sym_u8) in cells.iter_mut().zip(&syms[..size]) {
        let sym = sym_u8 as usize;
        let x = next[sym];
        next[sym] = x + 1;
        let nb = al - (31 - x.leading_zeros());
        let base = ((x << nb) - size as u32) as u16;
        let (val, eb) = match kind {
            Kind::Ll => LL_BASE[sym.min(LL_BASE.len() - 1)],
            Kind::Ml => ML_BASE[sym.min(ML_BASE.len() - 1)],
            Kind::Of => (1u32 << sym.min(31), sym.min(31) as u32),
        };
        *d = SCell {
            base,
            nb: nb as u8,
            eb: eb as u8,
            val,
        };
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_seq_table(
    mode: u8,
    data: &[u8],
    pos: &mut usize,
    src: &mut TableSrc,
    al: &mut u32,
    cells: &mut [SCell],
    spec: (usize, u32, u32),
    kind: Kind,
) -> Result<()> {
    let (max_sym, max_al, predef_al) = spec;
    match mode {
        0 => {
            *src = TableSrc::Predef;
            *al = predef_al;
        }
        1 => {
            let s = *data.get(*pos).ok_or(Error::Corrupt("rle symbol"))?;
            if s as usize > max_sym {
                return Err(Error::Corrupt("rle symbol out of range"));
            }
            *pos += 1;
            let sym = s as usize;
            let (val, eb) = match kind {
                Kind::Ll => LL_BASE[sym.min(LL_BASE.len() - 1)],
                Kind::Ml => ML_BASE[sym.min(ML_BASE.len() - 1)],
                Kind::Of => (1u32 << sym.min(31), sym.min(31) as u32),
            };
            cells[0] = SCell {
                base: 0,
                nb: 0,
                eb: eb as u8,
                val,
            };
            *src = TableSrc::Custom;
            *al = 0;
        }
        2 => {
            let tail = data.get(*pos..).unwrap_or(&[]);
            let mut probs = [0i16; 64];
            let (plen, table_al, used) = fse::read_dist_into(tail, max_sym, max_al, &mut probs)?;
            build_seq_cells_into(&probs[..plen], table_al, kind, cells)?;
            *pos += used;
            *src = TableSrc::Custom;
            *al = table_al;
        }
        _ => {
            if *src == TableSrc::None {
                return Err(Error::Corrupt("repeat mode without table"));
            }
        }
    }
    Ok(())
}

#[cold]
#[inline(never)]
fn copy_long_lits(dst: &mut [u8], pos: usize, lits: &[u8], lp: usize, llen: usize) {
    dst[pos..pos + llen].copy_from_slice(&lits[lp..lp + llen]);
}

#[cold]
#[inline(never)]
fn copy_long_match(dst: &mut [u8], p: usize, from: usize, off: usize, mlen: usize) {
    if off >= mlen {
        let (head, tail) = dst.split_at_mut(p);
        tail[16..mlen].copy_from_slice(&head[from + 16..from + mlen]);
    } else {
        let mut d = 16usize;
        while d < mlen {
            let (h, t) = dst.split_at_mut(p + d);
            t[..16].copy_from_slice(&h[from + d..from + d + 16]);
            d += 16;
        }
    }
}

#[cold]
#[inline(never)]
fn copy_short_off_tail(dst: &mut [u8], p: usize, off: usize, mlen: usize) {
    const STEP16: [usize; 16] = [
        0, 16, 16, 18, 16, 20, 18, 21, 16, 18, 20, 22, 24, 26, 28, 30,
    ];
    let step = STEP16[off];
    let mut d = 16usize;
    while d < mlen {
        let wp = p + d;
        let (head, tail) = dst.split_at_mut(wp);
        tail[..16].copy_from_slice(&head[wp - step..wp - step + 16]);
        d += 16;
    }
}

#[cold]
#[inline(never)]
fn copy_short_off(dst: &mut [u8], p: usize, from: usize, off: usize, mlen: usize) {
    const DEC32: [usize; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
    const DEC64: [usize; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
    if off == 1 {
        let b = dst[from];
        dst[p..p + mlen].fill(b);
        return;
    }
    if off >= 8 {
        let (h1, t1) = dst.split_at_mut(p);
        t1[..8].copy_from_slice(&h1[from..from + 8]);
        if mlen > 8 {
            let (h2, t2) = dst.split_at_mut(p + 8);
            t2[..8].copy_from_slice(&h2[from + 8..from + 16]);
        }
    } else {
        dst[p] = dst[from];
        dst[p + 1] = dst[from + 1];
        dst[p + 2] = dst[from + 2];
        dst[p + 3] = dst[from + 3];
        let s2 = from + DEC32[off];
        let (h4, t4) = dst.split_at_mut(p + 4);
        t4[..4].copy_from_slice(&h4[s2..s2 + 4]);
        if mlen > 8 {
            let s8 = s2 + 8 - DEC64[off];
            let (h8, t8) = dst.split_at_mut(p + 8);
            t8[..8].copy_from_slice(&h8[s8..s8 + 8]);
        }
    }
    if mlen > 16 {
        copy_short_off_tail(dst, p, off, mlen);
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn apply_seq_slice(
    llen: u32,
    mlen: u32,
    off: usize,
    lits: &[u8],
    lit_len: usize,
    lp: &mut usize,
    dst: &mut [u8],
    end_cap: usize,
    pos: &mut usize,
    start: usize,
    win_limit: usize,
    max_block_end: usize,
) -> Result<()> {
    let ll = llen as usize;
    let ml = mlen as usize;
    let cur_lp = *lp;
    let cur_pos = *pos;
    if cur_lp > lit_len || ll > lit_len - cur_lp {
        return Err(Error::Corrupt("literal run overrun"));
    }
    let step = ll + ml;
    if cur_pos > end_cap || step > end_cap - cur_pos {
        let next_pos = cur_pos.saturating_add(step);
        if next_pos > max_block_end {
            return Err(Error::Corrupt("regenerated block too large"));
        }
        return Err(Error::TooLarge);
    }
    let next_pos = cur_pos + step;
    if ll <= 16 {
        dst[cur_pos..cur_pos + 16].copy_from_slice(&lits[cur_lp..cur_lp + 16]);
    } else {
        copy_long_lits(dst, cur_pos, lits, cur_lp, ll);
    }
    *lp = cur_lp + ll;
    let p = cur_pos + ll;
    let max_off = (p - start).min(win_limit);
    if off == 0 || off > max_off {
        return Err(Error::Corrupt("offset beyond history"));
    }
    let from = p - off;
    if off >= 16 {
        let (head, tail) = dst.split_at_mut(p);
        tail[..16].copy_from_slice(&head[from..from + 16]);
        if ml > 16 {
            if off >= 32 && ml <= 32 {
                tail[16..32].copy_from_slice(&head[from + 16..from + 32]);
            } else {
                copy_long_match(dst, p, from, off, ml);
            }
        }
    } else {
        copy_short_off(dst, p, from, off, ml);
    }
    *pos = next_pos;
    Ok(())
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn read_long_seq<'a>(
    cur: &mut RevCursor,
    seq_data: &'a [u8],
    eo: u32,
    em: u32,
    el: u32,
    nl: u32,
    nm: u32,
    no: u32,
    tn: u32,
) -> (u64, u64, u64, u64, u64, u64, Option<RevBits<'a>>) {
    let raw1 = cur.load_msb(seq_data);
    let xo = if eo == 0 { 0 } else { (raw1 >> 8) >> (56 - eo) };
    cur.advance(eo);
    if cur.cur < 8 {
        let mut r = cur.into_rev(seq_data);
        let xm = r.read(em) as u64;
        let xl = r.read(el) as u64;
        let (ul, um, uo) = if tn > 0 {
            (r.read(nl) as u64, r.read(nm) as u64, r.read(no) as u64)
        } else {
            (0, 0, 0)
        };
        return (xo, xm, xl, ul, um, uo, Some(r));
    }
    let em_el = em + el;
    let raw2 = cur.load_msb(seq_data);
    let v2 = if em_el == 0 {
        0
    } else {
        (raw2 >> 8) >> (56 - em_el)
    };
    cur.advance(em_el);
    let xl = v2 & mask64(el);
    let xm = v2 >> el;
    if tn == 0 {
        return (xo, xm, xl, 0, 0, 0, None);
    }
    if cur.cur < 8 {
        let mut r = cur.into_rev(seq_data);
        let ul = r.read(nl) as u64;
        let um = r.read(nm) as u64;
        let uo = r.read(no) as u64;
        return (xo, xm, xl, ul, um, uo, Some(r));
    }
    let raw3 = cur.load_msb(seq_data);
    let v3 = (raw3 >> 8) >> (56 - tn);
    cur.advance(tn);
    let uo = v3 & mask64(no);
    let um = (v3 >> no) & mask64(nm);
    let ul = v3 >> (no + nm);
    (xo, xm, xl, ul, um, uo, None)
}

#[inline(always)]
fn update_hist(ofv: u32, llen: u32, hist: &mut [usize; 3]) -> usize {
    let [h0, h1, h2] = *hist;
    if ofv > 3 {
        let o = (ofv - 3) as usize;
        *hist = [o, h0, h1];
        o
    } else {
        let k = if llen == 0 { ofv } else { ofv - 1 };
        match k {
            0 => h0,
            1 => {
                *hist = [h1, h0, h2];
                h1
            }
            2 => {
                *hist = [h2, h0, h1];
                h2
            }
            _ => {
                let o = h0.saturating_sub(1);
                *hist = [o, h0, h1];
                o
            }
        }
    }
}

const MASK64: [u64; 64] = {
    let mut t = [0u64; 64];
    let mut i = 1usize;
    while i < 64 {
        t[i] = (1u64 << i) - 1;
        i += 1;
    }
    t
};

#[inline(always)]
fn mask64(n: u32) -> u64 {
    MASK64[(n as usize) & 63]
}

struct SeqLoopState {
    seq_idx: usize,
    sl: usize,
    so: usize,
    sm: usize,
    lp: usize,
    pos: usize,
    hist: [usize; 3],
    cur: RevCursor,
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn decode_seq_batch(
    bit_pos: &mut usize,
    seq_data: &[u8],
    ll: &[SCell; 512],
    of: &[SCell; 256],
    ml: &[SCell; 512],
    sl: &mut usize,
    so: &mut usize,
    sm: &mut usize,
    batch: &mut [(u32, u32, u32); 32],
    max_cnt: usize,
) -> (usize, bool) {
    let mut cnt = 0usize;
    let mut bp = *bit_pos;
    let mut l = *sl;
    let mut o = *so;
    let mut m = *sm;
    let mut long_hit = false;
    while cnt < max_cnt && bp >= 57 {
        let cl = ll[l & 511];
        let co = of[o & 255];
        let cm = ml[m & 511];
        let (eo, em, el) = (co.eb as u32, cm.eb as u32, cl.eb as u32);
        let (nl, nm, no) = (cl.nb as u32, cm.nb as u32, co.nb as u32);
        let no_nm = no + nm;
        let tn = no_nm + nl;
        let el_em = el + em;
        let total_bits = eo + el_em + tn;
        if total_bits > 56 {
            long_hit = true;
            break;
        }
        let byte_end = (bp + 7) >> 3;
        let bit_off = (bp.wrapping_neg() & 7) as u32;
        let raw =
            u64::from_le_bytes(seq_data[byte_end - 8..byte_end].try_into().unwrap()) << bit_off;
        let v = (raw >> 8) >> (56 - total_bits);
        bp -= total_bits as usize;

        let ev = v >> tn;
        let uo = v & mask64(no);
        let um = (v >> no) & mask64(nm);
        let ul = (v >> no_nm) & mask64(nl);
        let xl = ev & mask64(el);
        let xm = (ev >> el) & mask64(em);
        let xo = ev >> el_em;

        l = cl.base as usize + ul as usize;
        m = cm.base as usize + um as usize;
        o = co.base as usize + uo as usize;

        batch[cnt] = (cl.val + xl as u32, cm.val + xm as u32, co.val + xo as u32);
        cnt += 1;
    }
    *bit_pos = bp;
    *sl = l;
    *so = o;
    *sm = m;
    (cnt, long_hit)
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_seq_loop(
    s: &mut SeqLoopState,
    nseq: usize,
    seq_data: &[u8],
    ll: &[SCell; 512],
    of: &[SCell; 256],
    ml: &[SCell; 512],
    lits: &[u8],
    dst: &mut [u8],
    start: usize,
    win_limit: usize,
    max_block_end: usize,
) -> Result<()> {
    let Some(end_cap) = dst.len().checked_sub(32) else {
        return Ok(());
    };
    let Some(lit_len) = lits.len().checked_sub(16) else {
        return Ok(());
    };
    if s.cur.cur < 8 || s.cur.cur > seq_data.len() {
        return Ok(());
    }
    let seq_data = &seq_data[..s.cur.cur];
    let mut bit_pos = s.cur.cur * 8 - s.cur.bit_off as usize;
    let mut seq_idx = s.seq_idx;
    let mut sl = s.sl;
    let mut so = s.so;
    let mut sm = s.sm;
    let mut lp = s.lp;
    let mut pos = s.pos;
    let mut hist = s.hist;

    let limit = nseq.saturating_sub(1);
    let mut batch = [(0u32, 0u32, 0u32); 32];
    while seq_idx < limit && bit_pos >= 57 {
        let max_cnt = (limit - seq_idx).min(32);
        let (cnt, long_hit) = decode_seq_batch(
            &mut bit_pos,
            seq_data,
            ll,
            of,
            ml,
            &mut sl,
            &mut so,
            &mut sm,
            &mut batch,
            max_cnt,
        );
        for &(llen, mlen, ofv) in &batch[..cnt] {
            let off = update_hist(ofv, llen, &mut hist);
            apply_seq_slice(
                llen,
                mlen,
                off,
                lits,
                lit_len,
                &mut lp,
                dst,
                end_cap,
                &mut pos,
                start,
                win_limit,
                max_block_end,
            )?;
        }
        seq_idx += cnt;
        if long_hit {
            break;
        }
    }

    s.seq_idx = seq_idx;
    s.sl = sl;
    s.so = so;
    s.sm = sm;
    s.lp = lp;
    s.pos = pos;
    s.hist = hist;
    s.cur = RevCursor {
        cur: (bit_pos + 7) >> 3,
        bit_off: (bit_pos.wrapping_neg() & 7) as u32,
    };
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn block(
    b: &[u8],
    st: &mut State,
    out: &mut Vec<u8>,
    start: usize,
    max_out: usize,
    frame_rem: Option<usize>,
    block_max: usize,
    win_limit: usize,
) -> Result<()> {
    if b.is_empty() {
        return Err(Error::Corrupt("empty block"));
    }
    let (lit_raw, lit_len, mut p) = literals(b, &mut st.huff, &mut st.huff_valid, &mut st.lits)?;
    if lit_len > block_max {
        return Err(Error::Corrupt("literals too large"));
    }
    let n0 = *b.get(p).ok_or(Error::Corrupt("missing sequences header"))? as usize;
    p += 1;
    let nseq = if n0 == 0 {
        0
    } else if n0 < 128 {
        n0
    } else if n0 < 255 {
        let x = *b.get(p).ok_or(Error::Corrupt("sequence count"))? as usize;
        p += 1;
        ((n0 - 128) << 8) + x
    } else {
        let x = b.get(p..p + 2).ok_or(Error::Corrupt("sequence count"))?;
        p += 2;
        x[0] as usize + ((x[1] as usize) << 8) + 0x7F00
    };
    if nseq == 0 {
        if frame_rem.is_some_and(|rem| lit_len > rem) {
            return Err(Error::Corrupt("frame content size mismatch"));
        }
        let lits = lit_raw.unwrap_or(&st.lits);
        return push(out, &lits[..lit_len], max_out);
    }
    if nseq > block_max / 3 {
        return Err(Error::Corrupt("too many sequences"));
    }
    let modes = *b.get(p).ok_or(Error::Corrupt("missing modes"))?;
    p += 1;
    if modes & 3 != 0 {
        return Err(Error::Corrupt("reserved modes bits"));
    }

    prepare_seq_table(
        modes >> 6,
        b,
        &mut p,
        &mut st.ll_src,
        &mut st.ll_al,
        &mut st.ll_cells,
        (35, 9, 6),
        Kind::Ll,
    )?;
    prepare_seq_table(
        (modes >> 4) & 3,
        b,
        &mut p,
        &mut st.of_src,
        &mut st.of_al,
        &mut st.of_cells,
        (31, 8, 5),
        Kind::Of,
    )?;
    prepare_seq_table(
        (modes >> 2) & 3,
        b,
        &mut p,
        &mut st.ml_src,
        &mut st.ml_al,
        &mut st.ml_cells,
        (52, 9, 6),
        Kind::Ml,
    )?;

    let lits = &(lit_raw.unwrap_or(&st.lits))[..lit_len + 16];

    let pre = predefs();
    let ll: &[SCell; 512] = match st.ll_src {
        TableSrc::Predef => &pre.ll,
        _ => &st.ll_cells,
    };
    let of: &[SCell; 256] = match st.of_src {
        TableSrc::Predef => &pre.of,
        _ => &st.of_cells,
    };
    let ml: &[SCell; 512] = match st.ml_src {
        TableSrc::Predef => &pre.ml,
        _ => &st.ml_cells,
    };

    let seq_data = b.get(p..).unwrap_or(&[]);
    let mut cur = RevCursor::new(seq_data)?;
    let block_base = out.len();
    let max_by_out = max_out.saturating_sub(block_base);
    let max_block_end = block_base + frame_rem.map_or(block_max, |rem| rem.min(block_max));
    let block_end_cap = block_base
        + frame_rem
            .map_or(max_by_out, |rem| rem.min(max_by_out))
            .min(block_max);
    out.resize(block_end_cap + 32, 0);
    let (mut sl, mut so, mut sm);
    let init_bits = st.ll_al + st.of_al + st.ml_al;
    let mut seq_idx = 0usize;
    let mut lp = 0usize;
    let mut pos = block_base;
    let mut hist = st.hist;

    let mut slow_r: Option<RevBits<'_>> = None;
    if cur.cur >= 8 {
        let raw = cur.load_msb(seq_data);
        let v = if init_bits == 0 {
            0
        } else {
            (raw >> 8) >> (56 - init_bits)
        };
        cur.advance(init_bits);
        sm = (v & mask64(st.ml_al)) as usize;
        let v1 = v >> st.ml_al;
        so = (v1 & mask64(st.of_al)) as usize;
        sl = ((v1 >> st.of_al) & mask64(st.ll_al)) as usize;

        let dst = &mut out[..block_end_cap + 32];
        let mut s = SeqLoopState {
            seq_idx,
            sl,
            so,
            sm,
            lp,
            pos,
            hist,
            cur,
        };
        let limit = nseq.saturating_sub(1);
        loop {
            if let Err(e) = run_seq_loop(
                &mut s,
                nseq,
                seq_data,
                ll,
                of,
                ml,
                lits,
                dst,
                start,
                win_limit,
                max_block_end,
            ) {
                out.truncate(block_base);
                return Err(e);
            }
            if s.seq_idx >= limit || s.cur.cur < 8 || s.cur.cur > seq_data.len() {
                break;
            }
            let cl = ll[s.sl & 511];
            let co = of[s.so & 255];
            let cm = ml[s.sm & 511];
            let (eo, em, el) = (co.eb as u32, cm.eb as u32, cl.eb as u32);
            let (nl, nm, no) = (cl.nb as u32, cm.nb as u32, co.nb as u32);
            let res = read_long_seq(&mut s.cur, seq_data, eo, em, el, nl, nm, no, nl + nm + no);
            let (xo, xm, xl, ul, um, uo) = (res.0, res.1, res.2, res.3, res.4, res.5);
            s.sl = cl.base as usize + ul as usize;
            s.sm = cm.base as usize + um as usize;
            s.so = co.base as usize + uo as usize;
            let ofv = co.val + xo as u32;
            let mlen = cm.val + xm as u32;
            let llen = cl.val + xl as u32;
            let off = update_hist(ofv, llen, &mut s.hist);
            if let Err(e) = apply_seq_slice(
                llen,
                mlen,
                off,
                lits,
                lit_len,
                &mut s.lp,
                dst,
                block_end_cap,
                &mut s.pos,
                start,
                win_limit,
                max_block_end,
            ) {
                out.truncate(block_base);
                return Err(e);
            }
            s.seq_idx += 1;
            if res.6.is_some() {
                slow_r = res.6;
                break;
            }
        }
        seq_idx = s.seq_idx;
        sl = s.sl;
        so = s.so;
        sm = s.sm;
        lp = s.lp;
        pos = s.pos;
        hist = s.hist;
        cur = s.cur;
    } else {
        let mut r = cur.into_rev(seq_data);
        sl = r.read(st.ll_al) as usize;
        so = r.read(st.of_al) as usize;
        sm = r.read(st.ml_al) as usize;
        slow_r = Some(r);
    }

    let r = slow_r.unwrap_or_else(|| cur.into_rev(seq_data));
    let res = finish_slow_seqs(
        seq_idx,
        nseq,
        sl,
        so,
        sm,
        r,
        ll,
        of,
        ml,
        lits,
        lp,
        lit_len,
        out.as_mut_slice(),
        pos,
        start,
        win_limit,
        block_end_cap,
        max_block_end,
        hist,
        &mut st.hist,
    );
    match res {
        Ok(final_pos) => {
            out.truncate(final_pos);
            Ok(())
        }
        Err(e) => {
            out.truncate(block_base);
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_slow_seqs(
    mut seq_idx: usize,
    nseq: usize,
    mut sl: usize,
    mut so: usize,
    mut sm: usize,
    mut r: RevBits<'_>,
    ll: &[SCell; 512],
    of: &[SCell; 256],
    ml: &[SCell; 512],
    lits: &[u8],
    mut lp: usize,
    lit_len: usize,
    dst: &mut [u8],
    mut pos: usize,
    start: usize,
    win_limit: usize,
    block_end_cap: usize,
    max_block_end: usize,
    mut hist: [usize; 3],
    hist_out: &mut [usize; 3],
) -> Result<usize> {
    while seq_idx < nseq {
        let cl = ll[sl & 511];
        let co = of[so & 255];
        let cm = ml[sm & 511];
        let (eo, em, el) = (co.eb as u32, cm.eb as u32, cl.eb as u32);
        let (nl, nm, no) = (cl.nb as u32, cm.nb as u32, co.nb as u32);
        let te = eo + em + el;
        let tn = if seq_idx + 1 < nseq { nl + nm + no } else { 0 };
        r.refill_wide();
        let (xo, xm, xl, ul, um, uo);
        if te + tn <= 56 {
            let v = r.read_wide(te + tn);
            uo = v & mask64(no);
            um = (v >> no) & mask64(nm);
            ul = (v >> (no + nm)) & mask64(nl);
            xl = (v >> tn) & mask64(el);
            xm = (v >> (tn + el)) & mask64(em);
            xo = v >> (tn + el + em);
        } else {
            xo = r.read(eo) as u64;
            xm = r.read(em) as u64;
            xl = r.read(el) as u64;
            if tn > 0 {
                ul = r.read(nl) as u64;
                um = r.read(nm) as u64;
                uo = r.read(no) as u64;
            } else {
                (ul, um, uo) = (0, 0, 0);
            }
        }
        sl = cl.base as usize + ul as usize;
        sm = cm.base as usize + um as usize;
        so = co.base as usize + uo as usize;
        let ofv = co.val + xo as u32;
        let mlen = cm.val + xm as u32;
        let llen = cl.val + xl as u32;
        let off = update_hist(ofv, llen, &mut hist);
        apply_seq_slice(
            llen,
            mlen,
            off,
            lits,
            lit_len,
            &mut lp,
            dst,
            block_end_cap,
            &mut pos,
            start,
            win_limit,
            max_block_end,
        )?;
        seq_idx += 1;
    }

    if r.overflow() {
        return Err(Error::Corrupt("sequence bitstream overrun"));
    }
    if r.remaining() != 0 {
        return Err(Error::Corrupt("sequence bitstream not consumed"));
    }

    let rest = lit_len - lp;
    let final_pos = pos + rest;
    if final_pos > max_block_end {
        return Err(Error::Corrupt("regenerated block too large"));
    }
    if final_pos > block_end_cap {
        return Err(Error::TooLarge);
    }
    if rest > 0 {
        dst[pos..final_pos].copy_from_slice(&lits[lp..lit_len]);
    }
    *hist_out = hist;
    Ok(final_pos)
}
