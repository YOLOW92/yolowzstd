//! 块级字面量解析与序列段调度（流式 Vec 写入与零拷贝直接切片写入）。
use crate::bits::{RevBits, RevCursor};
use crate::huff::{self, HuffTable};
use crate::{Error, Result};

use super::seq::{
    SeqLoopState, apply_seq_exact, apply_seq_slice, finish_slow_seqs, mask64, read_long_seq,
    run_seq_loop, update_hist,
};
use super::tables::{Kind, predefs, prepare_seq_table};
use super::{MAX_BLOCK, State, parse_frame_header, push};

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
            if let Some(padded) = b.get(hl..hl + n + 32) {
                return Ok((Some(padded), n, hl + n));
            }
            if scratch.len() < n + 32 {
                scratch.resize(n + 32, 0);
            }
            scratch[..n].copy_from_slice(s);
            return Ok((None, n, hl + n));
        }
        let val = *b.get(hl).ok_or(Error::Corrupt("rle literal truncated"))?;
        if scratch.len() < n + 32 {
            scratch.resize(n + 32, val);
        }
        scratch[..n + 32].fill(val);
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
    if scratch.len() < regen + 32 {
        scratch.resize(regen + 32, 0);
    }
    huff::decode_into(huff, streams, four, &mut scratch[..regen])?;
    Ok((None, regen, hl + comp))
}

#[inline]
fn parse_seq_header<'a>(
    b: &'a [u8],
    st: &mut State,
    block_max: usize,
) -> Result<(Option<&'a [u8]>, usize, usize, usize)> {
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
    if nseq > block_max / 3 {
        return Err(Error::Corrupt("too many sequences"));
    }
    Ok((lit_raw, lit_len, nseq, p))
}

#[inline]
fn load_seq_tables(b: &[u8], p: &mut usize, st: &mut State) -> Result<()> {
    let modes = *b.get(*p).ok_or(Error::Corrupt("missing modes"))?;
    *p += 1;
    if modes & 3 != 0 {
        return Err(Error::Corrupt("reserved modes bits"));
    }
    let pre = predefs();
    prepare_seq_table(
        modes >> 6,
        b,
        p,
        &mut st.ll_src,
        &mut st.ll_predef,
        &mut st.ll_al,
        &mut st.tables.ll,
        &pre.ll,
        (35, 9, 6),
        Kind::Ll,
    )?;
    prepare_seq_table(
        (modes >> 4) & 3,
        b,
        p,
        &mut st.of_src,
        &mut st.of_predef,
        &mut st.of_al,
        &mut st.tables.of,
        &pre.of,
        (31, 8, 5),
        Kind::Of,
    )?;
    prepare_seq_table(
        (modes >> 2) & 3,
        b,
        p,
        &mut st.ml_src,
        &mut st.ml_predef,
        &mut st.ml_al,
        &mut st.tables.ml,
        &pre.ml,
        (52, 9, 6),
        Kind::Ml,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn exec_block_seqs(
    seq_data: &[u8],
    cur: &mut RevCursor,
    nseq: usize,
    st: &mut State,
    lit_raw: Option<&[u8]>,
    lit_len: usize,
    dst: &mut [u8],
    block_base: usize,
    start: usize,
    win_limit: usize,
    block_end_cap: usize,
    max_block_end: usize,
) -> Result<usize> {
    let lits = &(lit_raw.unwrap_or(&st.lits))[..lit_len + 32];
    let tables = &st.tables;
    let batch = &mut st.seq_batch;

    // 帧内相对切片：start 归零，使 LLVM 静态证明 off <= p 及 wildcopy 无界内溢出错检。
    let dst = &mut dst[start..];
    let block_base = block_base - start;
    let block_end_cap = block_end_cap - start;
    let max_block_end = max_block_end - start;

    let (mut sl, mut so, mut sm);
    let init_bits = st.ll_al + st.of_al + st.ml_al;
    let mut seq_idx = 0usize;
    let mut lp = 0usize;
    let mut pos = block_base;
    let mut hist = st.hist;
    let fast_end_cap = block_end_cap.min(dst.len().saturating_sub(32));

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

        let mut s = SeqLoopState {
            seq_idx,
            sl,
            so,
            sm,
            lp,
            pos,
            hist,
            cur: *cur,
        };
        let limit = nseq.saturating_sub(1);
        loop {
            run_seq_loop(
                &mut s,
                nseq,
                seq_data,
                tables,
                batch,
                lits,
                dst,
                block_end_cap,
                win_limit,
                max_block_end,
            )?;
            if s.seq_idx >= limit || s.cur.cur < 8 || s.cur.cur > seq_data.len() {
                break;
            }
            let cl = tables.ll[s.sl & 511];
            let co = tables.of[s.so & 255];
            let cm = tables.ml[s.sm & 511];
            let (eo, em, el) = (co.eb(), cm.eb(), cl.eb());
            let (nl, nm, no) = (cl.nb(), cm.nb(), co.nb());
            let res = read_long_seq(&mut s.cur, seq_data, eo, em, el, nl, nm, no, nl + nm + no);
            let (xo, xm, xl, ul, um, uo) = (res.0, res.1, res.2, res.3, res.4, res.5);
            s.sl = cl.base() + ul as usize;
            s.sm = cm.base() + um as usize;
            s.so = co.base() + uo as usize;
            let ofv = co.val() + xo as u32;
            let mlen = cm.val() + xm as u32;
            let llen = cl.val() + xl as u32;
            let off = update_hist(ofv, llen, &mut s.hist);
            let step = llen as usize + mlen as usize;
            if s.pos <= fast_end_cap && step <= fast_end_cap - s.pos {
                apply_seq_slice(
                    llen,
                    mlen,
                    off,
                    lits,
                    lit_len,
                    &mut s.lp,
                    dst,
                    fast_end_cap,
                    &mut s.pos,
                    win_limit,
                    max_block_end,
                )?;
            } else {
                apply_seq_exact(
                    llen,
                    mlen,
                    off,
                    lits,
                    lit_len,
                    &mut s.lp,
                    dst,
                    block_end_cap,
                    &mut s.pos,
                    win_limit,
                    max_block_end,
                )?;
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
        *cur = s.cur;
    } else {
        let mut r = cur.into_rev(seq_data);
        sl = r.read(st.ll_al) as usize;
        so = r.read(st.of_al) as usize;
        sm = r.read(st.ml_al) as usize;
        slow_r = Some(r);
    }

    let r = slow_r.unwrap_or_else(|| cur.into_rev(seq_data));
    let final_rel = finish_slow_seqs(
        seq_idx,
        nseq,
        sl,
        so,
        sm,
        r,
        tables,
        lits,
        lp,
        lit_len,
        dst,
        pos,
        win_limit,
        block_end_cap,
        max_block_end,
        hist,
        &mut st.hist,
    )?;
    Ok(start + final_rel)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn block(
    b: &[u8],
    st: &mut State,
    out: &mut Vec<u8>,
    start: usize,
    max_out: usize,
    frame_rem: Option<usize>,
    block_max: usize,
    win_limit: usize,
) -> Result<()> {
    let (lit_raw, lit_len, nseq, mut p) = parse_seq_header(b, st, block_max)?;
    if nseq == 0 {
        if frame_rem.is_some_and(|rem| lit_len > rem) {
            return Err(Error::Corrupt("frame content size mismatch"));
        }
        let lits = lit_raw.unwrap_or(&st.lits);
        return push(out, &lits[..lit_len], max_out);
    }
    load_seq_tables(b, &mut p, st)?;
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

    let dst = &mut out[..block_end_cap + 32];

    match exec_block_seqs(
        seq_data,
        &mut cur,
        nseq,
        st,
        lit_raw,
        lit_len,
        dst,
        block_base,
        start,
        win_limit,
        block_end_cap,
        max_block_end,
    ) {
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

/// 将单帧直接解码写入已预分配的精确目标切片 `dst`（零额外堆分配、零尾随 memcpy，专用于多核并行解压）。
pub(crate) fn decompress_frame_into_slice(
    st: &mut State,
    frame_with_magic: &[u8],
    dst: &mut [u8],
) -> Result<()> {
    let magic = frame_with_magic
        .get(..4)
        .ok_or(Error::Corrupt("truncated magic"))?;
    let m = u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]);
    if m != 0xFD2FB528 {
        return Err(Error::Corrupt("bad magic"));
    }
    let d = &frame_with_magic[4..];
    let fh = parse_frame_header(d)?;
    let Some(expected) = fh.uncomp_len else {
        return Err(Error::Corrupt("missing frame content size"));
    };
    if usize::try_from(expected).ok() != Some(dst.len()) {
        return Err(Error::Corrupt("frame content size mismatch"));
    }
    let block_max = fh.block_max();
    let win_limit = fh.window_size;
    let mut p = fh.header_len;
    let mut pos = 0usize;
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
                if pos + size > dst.len() {
                    return Err(Error::Corrupt("frame content size mismatch"));
                }
                dst[pos..pos + size].copy_from_slice(b);
                pos += size;
                p += size;
            }
            1 => {
                if size > block_max {
                    return Err(Error::Corrupt("rle block too large"));
                }
                let b = *d.get(p).ok_or(Error::Corrupt("truncated rle block"))?;
                if pos + size > dst.len() {
                    return Err(Error::Corrupt("frame content size mismatch"));
                }
                dst[pos..pos + size].fill(b);
                pos += size;
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
                let (lit_raw, lit_len, nseq, mut bp) = parse_seq_header(b, st, block_max)?;
                let block_end_cap = (pos + block_max).min(dst.len());
                if nseq == 0 {
                    if pos + lit_len > block_end_cap {
                        return Err(Error::Corrupt("frame content size mismatch"));
                    }
                    let lits = lit_raw.unwrap_or(&st.lits);
                    dst[pos..pos + lit_len].copy_from_slice(&lits[..lit_len]);
                    pos += lit_len;
                } else {
                    load_seq_tables(b, &mut bp, st)?;
                    let seq_data = b.get(bp..).unwrap_or(&[]);
                    let mut cur = RevCursor::new(seq_data)?;
                    pos = exec_block_seqs(
                        seq_data,
                        &mut cur,
                        nseq,
                        st,
                        lit_raw,
                        lit_len,
                        dst,
                        pos,
                        0,
                        win_limit,
                        block_end_cap,
                        block_end_cap,
                    )?;
                }
                p += size;
            }
            _ => return Err(Error::Corrupt("reserved block type")),
        }
        if last {
            break;
        }
    }
    if pos != dst.len() {
        return Err(Error::Corrupt("frame content size mismatch"));
    }
    if fh.checksum {
        let c = d
            .get(p..p + 4)
            .ok_or(Error::Corrupt("truncated checksum"))?;
        let want = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        if crate::xxh::xxh64(dst) as u32 != want {
            return Err(Error::Corrupt("checksum mismatch"));
        }
        p += 4;
    }
    if p != d.len() {
        return Err(Error::Corrupt("trailing frame bytes"));
    }
    Ok(())
}
