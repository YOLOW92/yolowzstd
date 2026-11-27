//! 自适应 Zstd 编码器框架：公开压缩 API、帧头/块调度、多档匹配引擎复用。
mod seq_enc;

use crate::chain::{Chain, ChainCfg};
use crate::henc::{HuffScratch, write_literals_into};
use crate::lz::{FastCfg, FastMode};
use crate::opt::{OptCfg, OptParser};
use crate::resources::Budget;
use crate::row::{RowCfg, RowMatcher};

use seq_enc::{SeqScratch, commit_seq_tables, predef_tables, write_seqs_into};

pub(crate) use seq_enc::{BitAcc, Seq, ll_code, ml_code};

pub(crate) const BLOCK: usize = 128 * 1024;

/// 压缩参数配置（支持等级、帧末尾 XXH64 校验和、可选窗口对数上限及多线程并行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompressOptions {
    /// 压缩等级：`<= 0` 仅存 Raw/RLE 块，`1..=4` 快速/双哈希，`5..=11` SWAR 行哈希前瞻，`12..=15` 后缀二叉树前瞻，`16..=22` 熵引导最优解析。
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
    fast: Option<(FastMode, bool, bool)>,
    row: Option<RowMatcher>,
    row_idle: u8,
    opt: Option<OptParser>,
    opt_idle: u8,
    chain: Option<Chain<u32>>,
    wide_chain: Option<Chain<usize>>,
    chain_idle: u8,
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
            row: None,
            row_idle: 0,
            opt: None,
            opt_idle: 0,
            chain: None,
            wide_chain: None,
            chain_idle: 0,
            seqs: Vec::with_capacity(4096),
            lits: Vec::with_capacity(32 * 1024),
            body: Vec::with_capacity(64 * 1024),
            huff: HuffScratch::new(),
            seq_sc: SeqScratch::new(),
        }
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.fast = None;
        self.row = None;
        self.row_idle = 0;
        self.opt = None;
        self.opt_idle = 0;
        self.chain = None;
        self.wide_chain = None;
        self.chain_idle = 0;
        self.seqs.clear();
        self.seqs.shrink_to_fit();
        self.lits.clear();
        self.lits.shrink_to_fit();
        self.body.clear();
        self.body.shrink_to_fit();
        self.seq_sc.shrink_to_fit();
    }

    #[inline]
    fn tick_idle_engines(&mut self, keep_row: bool, keep_opt: bool, keep_chain: bool) {
        if !keep_chain && (self.chain.is_some() || self.wide_chain.is_some()) {
            self.chain_idle = self.chain_idle.saturating_add(1);
            if self.chain_idle >= 8 {
                self.chain = None;
                self.wide_chain = None;
                self.chain_idle = 0;
            }
        }
        if !keep_row && self.row.is_some() {
            self.row_idle = self.row_idle.saturating_add(1);
            if self.row_idle >= 8 {
                self.row = None;
                self.row_idle = 0;
            }
        }
        if !keep_opt && self.opt.is_some() {
            self.opt_idle = self.opt_idle.saturating_add(1);
            if self.opt_idle >= 8 {
                self.opt = None;
                self.opt_idle = 0;
            }
        }
    }
}

/// 可复用的 Zstd 压缩上下文，跨次调用保留哈希表、链表、DP 状态与熵编码暂存；
/// 通过代际戳记实现无表清零的毫秒级小包与流式反复压缩，启用多线程选项时复用每核工作上下文。
pub struct Compressor {
    core: EncCore,
    mt_workers: Vec<EncCore>,
    mt_slots: Vec<Vec<u8>>,
    mt_idle: u8,
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
            mt_idle: 0,
        }
    }

    /// 释放匹配表、DP 工作集及并行工作池内存，保留当前压缩器可继续复用。
    pub fn shrink_to_fit(&mut self) {
        self.core.shrink_to_fit();
        self.mt_workers.clear();
        self.mt_workers.shrink_to_fit();
        self.mt_slots.clear();
        self.mt_slots.shrink_to_fit();
        self.mt_idle = 0;
    }

    #[inline]
    fn tick_mt_idle(&mut self) {
        if !self.mt_workers.is_empty() || !self.mt_slots.is_empty() {
            self.mt_idle = self.mt_idle.saturating_add(1);
            if self.mt_idle >= 8 {
                self.mt_workers.clear();
                self.mt_workers.shrink_to_fit();
                self.mt_slots.clear();
                self.mt_slots.shrink_to_fit();
                self.mt_idle = 0;
            }
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
            self.tick_mt_idle();
            self.core.compress_single_frame(data, out, opts);
        } else {
            crate::mt::compress_parallel(
                &mut self.core,
                &mut self.mt_workers,
                &mut self.mt_slots,
                &mut self.mt_idle,
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
        let mut row_cfg = None;
        let mut opt_cfg = None;
        let mut fast_cfg = FastCfg::for_level(level);
        if level == 3 && (data.len() > BLOCK || data.len() < 128) {
            fast_cfg.min_match = 5;
        }
        let mut chain_cfg = None;
        if let Some(ref b) = budget {
            if level < 5 {
                use_fast = true;
                self.tick_idle_engines(false, false, false);
                let active_bits = match level {
                    ..=1 => b.hash_bits.clamp(1, 15),
                    3 if data.len() > BLOCK => {
                        16u32.min(usize::BITS - 1 - data.len().leading_zeros())
                    }
                    _ => b.hash_bits.max(1),
                };
                let fast_win = win_limit;
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
                    f.prepare(data.len(), active_bits, fast_cfg.dual, fast_win);
                }
            } else if level <= 11 && !wide {
                self.row_idle = 0;
                self.tick_idle_engines(true, false, false);
                let rc = RowCfg::for_level(level, data.len(), win_limit);
                let rm = self.row.get_or_insert_with(RowMatcher::new);
                rm.prepare(data.len(), rc);
                row_cfg = Some(rc);
            } else if level >= 16 && !wide {
                self.opt_idle = 0;
                self.tick_idle_engines(false, true, false);
                let oc = OptCfg::for_level(level, data.len(), b.window, win_limit);
                let op = self.opt.get_or_insert_with(OptParser::new);
                op.prepare(data.len(), oc);
                opt_cfg = Some(oc);
            } else {
                self.chain_idle = 0;
                self.tick_idle_engines(false, false, true);
                let cc = ChainCfg::from_budget(b, level, win_limit);
                chain_cfg = Some(cc);
                if wide {
                    let ch = self.wide_chain.get_or_insert_with(|| Chain::new(b));
                    ch.prepare(data.len(), b, cc.bt);
                } else {
                    if self.wide_chain.is_some() {
                        self.wide_chain = None;
                    }
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
        let block_max = win_limit.min(BLOCK);
        let mut start = 0usize;
        while start < data.len() {
            let end = (start + block_max).min(data.len());
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
            } else if let Some(rc) = row_cfg {
                self.row.as_mut().expect("row init").parse(
                    data,
                    (start, end),
                    rc,
                    &mut rep,
                    &mut self.seqs,
                    &mut self.lits,
                )
            } else if let Some(oc) = opt_cfg {
                self.opt.as_mut().expect("opt init").parse(
                    data,
                    (start, end),
                    oc,
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
                    level <= 1,
                    &mut self.body,
                );
                Some(self.body[seq_hdr_pos])
            };

            if self.body.len() >= blk.len() {
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
