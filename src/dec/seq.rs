//! 序列位流批解码（单基址紧凑序列表 + 零越界切片游标）与零边界检查 LZ77 拷贝执行热路径。
use crate::bits::{RevBits, RevCursor};
use crate::{Error, Result};

use super::tables::SeqTables;

pub(super) const MASK64: [u64; 64] = {
    let mut t = [0u64; 64];
    let mut i = 1usize;
    while i < 64 {
        t[i] = (1u64 << i) - 1;
        i += 1;
    }
    t
};

#[inline(always)]
pub(super) fn mask64(n: u32) -> u64 {
    MASK64[(n as usize) & 63]
}

#[cold]
#[inline(never)]
fn err_block_overflow(cur_pos: usize, step: usize, max_block_end: usize) -> Result<()> {
    if cur_pos.saturating_add(step) > max_block_end {
        Err(Error::Corrupt("regenerated block too large"))
    } else {
        Err(Error::TooLarge)
    }
}

#[inline(never)]
fn copy_long_lits(dst: &mut [u8], pos: usize, lits: &[u8], lp: usize, llen: usize) {
    dst[pos..pos + llen].copy_from_slice(&lits[lp..lp + llen]);
}

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

/// 帧相对切片（`start == 0`、`dst.len() == fast_end_cap + 32`、`lits.len() == lit_len + 32`）的零越界检查拷贝核心。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_seq_slice(
    llen: u32,
    mlen: u32,
    off: usize,
    lits: &[u8],
    lit_len: usize,
    lp: &mut usize,
    dst: &mut [u8],
    fast_end_cap: usize,
    pos: &mut usize,
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
    if cur_pos > fast_end_cap || step > fast_end_cap - cur_pos {
        return err_block_overflow(cur_pos, step, max_block_end);
    }
    let next_pos = cur_pos + step;
    if ll <= 16 {
        dst[cur_pos..cur_pos + 16].copy_from_slice(&lits[cur_lp..cur_lp + 16]);
    } else if ll <= 32 {
        dst[cur_pos..cur_pos + 32].copy_from_slice(&lits[cur_lp..cur_lp + 32]);
    } else {
        copy_long_lits(dst, cur_pos, lits, cur_lp, ll);
    }
    *lp = cur_lp + ll;
    let p = cur_pos + ll;
    let max_off = p.min(win_limit);
    if off.wrapping_sub(1) >= max_off {
        return Err(Error::Corrupt("offset beyond history"));
    }
    if off >= 16 {
        let (head, tail) = dst.split_at_mut(p);
        let src1 = &head[head.len() - off..];
        tail[..16].copy_from_slice(&src1[..16]);
        if ml > 16 {
            if ml <= 32 {
                let (h2, t2) = dst.split_at_mut(p + 16);
                let src2 = &h2[h2.len() - off..];
                t2[..16].copy_from_slice(&src2[..16]);
            } else {
                copy_long_match(dst, p, p - off, off, ml);
            }
        }
    } else {
        copy_short_off(dst, p, p - off, off, ml);
    }
    *pos = next_pos;
    Ok(())
}

/// 精确边界拷贝版（零越界 wildcopy），用于无额外填充切片末尾 32 字节内收尾。
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_seq_exact(
    llen: u32,
    mlen: u32,
    off: usize,
    lits: &[u8],
    lit_len: usize,
    lp: &mut usize,
    dst: &mut [u8],
    end_cap: usize,
    pos: &mut usize,
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
        return err_block_overflow(cur_pos, step, max_block_end);
    }
    if ll > 0 {
        dst[cur_pos..cur_pos + ll].copy_from_slice(&lits[cur_lp..cur_lp + ll]);
    }
    *lp = cur_lp + ll;
    let p = cur_pos + ll;
    let max_off = p.min(win_limit);
    if off.wrapping_sub(1) >= max_off {
        return Err(Error::Corrupt("offset beyond history"));
    }
    let from = p - off;
    if off >= ml {
        let (head, tail) = dst.split_at_mut(p);
        tail[..ml].copy_from_slice(&head[from..from + ml]);
    } else if off == 1 {
        let b = dst[from];
        dst[p..p + ml].fill(b);
    } else {
        let mut copied = 0usize;
        while copied < ml {
            let chunk = off.min(ml - copied);
            let (head, tail) = dst.split_at_mut(p + copied);
            tail[..chunk].copy_from_slice(&head[from + copied..from + copied + chunk]);
            copied += chunk;
        }
    }
    *pos = cur_pos + step;
    Ok(())
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn read_long_seq<'a>(
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
pub(super) fn update_hist(ofv: u32, llen: u32, hist: &mut [usize; 3]) -> usize {
    let [h0, h1, h2] = *hist;
    if ofv > 3 {
        let o = (ofv - 3) as usize;
        *hist = [o, h0, h1];
        o
    } else {
        let k = ofv - u32::from(llen != 0);
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

pub(super) struct SeqLoopState {
    pub seq_idx: usize,
    pub sl: usize,
    pub so: usize,
    pub sm: usize,
    pub lp: usize,
    pub pos: usize,
    pub hist: [usize; 3],
    pub cur: RevCursor,
}

/// 单基址 `SeqTables` + 零栈传参 + 双短链并行移位的 64 序列批解码内核。
#[inline(never)]
fn decode_seq_batch(
    s: &mut SeqLoopState,
    seq_data: &[u8],
    tables: &SeqTables,
    batch: &mut [(u32, u32, u32); 64],
    max_cnt: usize,
) -> (usize, bool) {
    let mut src = match seq_data.get(..s.cur.cur) {
        Some(slice) => slice,
        None => return (0, false),
    };
    let mut bit_off = s.cur.bit_off & 7;
    let mut l = s.sl;
    let mut o = s.so;
    let mut m = s.sm;
    let mut cnt = 0usize;
    let mut long_hit = false;

    for slot in &mut batch[..max_cnt] {
        if src.len() < 8 {
            break;
        }
        let cl = tables.ll[l & 511];
        let co = tables.of[o & 255];
        let cm = tables.ml[m & 511];
        let nl = cl.nb();
        let nm = cm.nb();
        let no = co.nb();
        let el = cl.eb();
        let em = cm.eb();
        let tn = nl + nm + no;
        let total_bits = tn + el + em + co.eb();
        if total_bits > 56 {
            long_hit = true;
            break;
        }
        let chunk: [u8; 8] = src[src.len() - 8..].try_into().unwrap();
        let raw = u64::from_le_bytes(chunk) << bit_off;
        let v = (raw >> 8) >> (56 - total_bits);
        let tot = bit_off + total_bits;
        let adv = ((tot >> 3) & 7) as usize;
        src = &src[..src.len() - adv];
        bit_off = tot & 7;

        let ev = v >> tn;
        let uo = v & mask64(no);
        let sv1 = v >> no;
        let um = sv1 & mask64(nm);
        let ul = (sv1 >> nm) & mask64(nl);

        let xl = ev & mask64(el);
        let ev1 = ev >> el;
        let xm = ev1 & mask64(em);
        let xo = ev1 >> em;

        l = cl.base() + ul as usize;
        m = cm.base() + um as usize;
        o = co.base() + uo as usize;

        *slot = (
            cl.val() + xl as u32,
            cm.val() + xm as u32,
            co.val() + xo as u32,
        );
        cnt += 1;
    }

    s.cur.cur = src.len();
    s.cur.bit_off = bit_off;
    s.sl = l;
    s.so = o;
    s.sm = m;
    (cnt, long_hit)
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_seq_loop(
    s: &mut SeqLoopState,
    nseq: usize,
    seq_data: &[u8],
    tables: &SeqTables,
    batch: &mut [(u32, u32, u32); 64],
    lits: &[u8],
    dst: &mut [u8],
    end_cap: usize,
    win_limit: usize,
    max_block_end: usize,
) -> Result<()> {
    let Some(dst_cap) = dst.len().checked_sub(32) else {
        return Ok(());
    };
    let fast_end_cap = end_cap.min(dst_cap);
    let dst = &mut dst[..fast_end_cap + 32];
    let Some(lit_len) = lits.len().checked_sub(32) else {
        return Ok(());
    };
    if s.cur.cur < 8 || s.cur.cur > seq_data.len() {
        return Ok(());
    }
    let seq_data = &seq_data[..s.cur.cur];
    let limit = nseq.saturating_sub(1);
    let has_tail_zone = fast_end_cap < end_cap;

    let mut seq_idx = s.seq_idx;
    let mut lp = s.lp;
    let mut pos = s.pos;
    let mut hist = s.hist;

    while seq_idx < limit && s.cur.cur >= 8 {
        if has_tail_zone && pos > fast_end_cap {
            break;
        }
        let max_cnt = (limit - seq_idx).min(64);
        let (cnt, long_hit) = decode_seq_batch(s, seq_data, tables, batch, max_cnt);
        let mut hit_tail = false;
        for &(llen, mlen, ofv) in &batch[..cnt] {
            let off = update_hist(ofv, llen, &mut hist);
            let ll = llen as usize;
            let ml = mlen as usize;
            let step = ll + ml;
            if pos <= fast_end_cap && step <= fast_end_cap - pos {
                if lp > lit_len || ll > lit_len - lp {
                    return Err(Error::Corrupt("literal run overrun"));
                }
                let next_pos = pos + step;
                if ll <= 16 {
                    dst[pos..pos + 16].copy_from_slice(&lits[lp..lp + 16]);
                } else if ll <= 32 {
                    dst[pos..pos + 32].copy_from_slice(&lits[lp..lp + 32]);
                } else {
                    copy_long_lits(dst, pos, lits, lp, ll);
                }
                lp += ll;
                let p = pos + ll;
                let max_off = p.min(win_limit);
                if off.wrapping_sub(1) >= max_off {
                    return Err(Error::Corrupt("offset beyond history"));
                }
                if off >= 16 {
                    let (head, tail) = dst.split_at_mut(p);
                    let src1 = &head[head.len() - off..];
                    tail[..16].copy_from_slice(&src1[..16]);
                    if ml > 16 {
                        if ml <= 32 {
                            let (h2, t2) = dst.split_at_mut(p + 16);
                            let src2 = &h2[h2.len() - off..];
                            t2[..16].copy_from_slice(&src2[..16]);
                        } else {
                            copy_long_match(dst, p, p - off, off, ml);
                        }
                    }
                } else {
                    copy_short_off(dst, p, p - off, off, ml);
                }
                pos = next_pos;
            } else if has_tail_zone {
                apply_seq_exact(
                    llen,
                    mlen,
                    off,
                    lits,
                    lit_len,
                    &mut lp,
                    dst,
                    end_cap,
                    &mut pos,
                    win_limit,
                    max_block_end,
                )?;
                hit_tail = true;
            } else {
                return err_block_overflow(pos, step, max_block_end);
            }
        }
        seq_idx += cnt;
        if long_hit || hit_tail {
            break;
        }
    }

    s.seq_idx = seq_idx;
    s.lp = lp;
    s.pos = pos;
    s.hist = hist;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finish_slow_seqs(
    mut seq_idx: usize,
    nseq: usize,
    mut sl: usize,
    mut so: usize,
    mut sm: usize,
    mut r: RevBits<'_>,
    tables: &SeqTables,
    lits: &[u8],
    mut lp: usize,
    lit_len: usize,
    dst: &mut [u8],
    mut pos: usize,
    win_limit: usize,
    block_end_cap: usize,
    max_block_end: usize,
    mut hist: [usize; 3],
    hist_out: &mut [usize; 3],
) -> Result<usize> {
    let fast_end_cap = block_end_cap.min(dst.len().saturating_sub(32));
    while seq_idx < nseq {
        let cl = tables.ll[sl & 511];
        let co = tables.of[so & 255];
        let cm = tables.ml[sm & 511];
        let (eo, em, el) = (co.eb(), cm.eb(), cl.eb());
        let (nl, nm, no) = (cl.nb(), cm.nb(), co.nb());
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
        sl = cl.base() + ul as usize;
        sm = cm.base() + um as usize;
        so = co.base() + uo as usize;
        let ofv = co.val() + xo as u32;
        let mlen = cm.val() + xm as u32;
        let llen = cl.val() + xl as u32;
        let off = update_hist(ofv, llen, &mut hist);
        let step = llen as usize + mlen as usize;
        if pos <= fast_end_cap && step <= fast_end_cap - pos {
            apply_seq_slice(
                llen,
                mlen,
                off,
                lits,
                lit_len,
                &mut lp,
                dst,
                fast_end_cap,
                &mut pos,
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
                &mut lp,
                dst,
                block_end_cap,
                &mut pos,
                win_limit,
                max_block_end,
            )?;
        }
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
