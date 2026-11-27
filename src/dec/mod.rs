//! 帧解码、帧头解析、可复用上下文与公共解码 API。
mod block;
mod seq;
mod tables;

use crate::huff::HuffTable;
use crate::{Error, Result};

use block::block;
use tables::{SeqTables, TableSrc};

pub(crate) use block::decompress_frame_into_slice;
pub(crate) use tables::{LL_BASE, LL_DEFAULT, ML_BASE, ML_DEFAULT, OF_DEFAULT};

pub(crate) const MAX_BLOCK: usize = 128 * 1024;

pub(crate) struct State {
    hist: [usize; 3],
    huff_valid: bool,
    huff: HuffTable,
    ll_src: TableSrc,
    of_src: TableSrc,
    ml_src: TableSrc,
    ll_predef: bool,
    of_predef: bool,
    ml_predef: bool,
    ll_al: u32,
    of_al: u32,
    ml_al: u32,
    tables: SeqTables,
    seq_batch: [(u32, u32, u32); 64],
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
            ll_predef: false,
            of_predef: false,
            ml_predef: false,
            ll_al: 0,
            of_al: 0,
            ml_al: 0,
            tables: SeqTables::new(),
            seq_batch: [(0, 0, 0); 64],
            lits: Vec::new(),
        }
    }

    pub(super) fn reset_frame(&mut self) {
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
            mt_idle: 0,
        }
    }

    /// 释放并行工作上下文与临时输出槽位，保留当前解压器可继续复用。
    pub fn shrink_to_fit(&mut self) {
        self.state.lits.clear();
        self.state.lits.shrink_to_fit();
        self.mt_states.clear();
        self.mt_states.shrink_to_fit();
        self.mt_idle = 0;
    }

    #[inline]
    fn tick_mt_idle(&mut self) {
        if !self.mt_states.is_empty() {
            self.mt_idle = self.mt_idle.saturating_add(1);
            if self.mt_idle >= 8 {
                self.mt_states.clear();
                self.mt_states.shrink_to_fit();
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
                .checked_add(4)
                .and_then(|p| p.checked_add(n))
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
        let reserve_cap = (v as usize).min(d.len().saturating_mul(64).max(MAX_BLOCK));
        if out.capacity() - out.len() < reserve_cap {
            out.reserve(reserve_cap);
        }
        frame_end = Some(out.len() + v as usize);
    }
    let mut p = fh.header_len;
    let start = out.len();
    let mut state_reset = false;
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
                if !state_reset {
                    st.reset_frame();
                    state_reset = true;
                }
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

pub(super) fn push(out: &mut Vec<u8>, b: &[u8], max_out: usize) -> Result<()> {
    if out.len() + b.len() > max_out {
        return Err(Error::TooLarge);
    }
    out.extend_from_slice(b);
    Ok(())
}
