//! Huffman 字面量：零分配权重解析、11 位展开解码表、4 路/单路 64 位批处理解码。
use crate::bits::{RevBits, RevCursor};
use crate::fse;
use crate::{Error, Result};

#[derive(Clone)]
pub(crate) struct HuffTable {
    pub entries: [u16; 2048],
}

impl Default for HuffTable {
    fn default() -> Self {
        Self { entries: [0; 2048] }
    }
}

/// 解析权重及隐含的最后一个符号，写入栈数组，返回（符号总数，最大码长，消耗字节）。
fn read_weights_into(data: &[u8], weights: &mut [u8; 256]) -> Result<(usize, u32, usize)> {
    let hb = *data.first().ok_or(Error::Corrupt("empty huffman header"))? as usize;
    let mut n = 0usize;
    let used = if hb >= 128 {
        n = hb - 127;
        let bytes = n.div_ceil(2);
        let body = data
            .get(1..1 + bytes)
            .ok_or(Error::Corrupt("huffman weights truncated"))?;
        for i in 0..n {
            let b = body[i >> 1];
            weights[i] = if i & 1 == 0 { b >> 4 } else { b & 15 };
        }
        1 + bytes
    } else {
        let body = data
            .get(1..1 + hb)
            .ok_or(Error::Corrupt("huffman fse truncated"))?;
        let mut probs = [0i16; 256];
        let (plen, al, off) = fse::read_dist_into(body, 255, 6, &mut probs)?;
        let mut cells = [fse::Cell::default(); 64];
        fse::build_into(&probs[..plen], al, &mut cells)?;
        let mut r = RevBits::new(body.get(off..).ok_or(Error::Corrupt("huffman fse"))?)?;
        let mut s1 = r.read(al) as usize;
        let mut s2 = r.read(al) as usize;
        loop {
            if n > 255 {
                return Err(Error::Corrupt("too many huffman weights"));
            }
            let c = cells[s1 & 63];
            weights[n] = c.sym;
            n += 1;
            s1 = c.base as usize + r.read(c.nb as u32) as usize;
            if r.overflow() {
                if n > 255 {
                    return Err(Error::Corrupt("too many huffman weights"));
                }
                weights[n] = cells[s2 & 63].sym;
                n += 1;
                break;
            }
            if n > 255 {
                return Err(Error::Corrupt("too many huffman weights"));
            }
            let c = cells[s2 & 63];
            weights[n] = c.sym;
            n += 1;
            s2 = c.base as usize + r.read(c.nb as u32) as usize;
            if r.overflow() {
                if n > 255 {
                    return Err(Error::Corrupt("too many huffman weights"));
                }
                weights[n] = cells[s1 & 63].sym;
                n += 1;
                break;
            }
        }
        1 + hb
    };
    if n > 255 {
        return Err(Error::Corrupt("too many huffman weights"));
    }
    let mut sum: u32 = 0;
    for &w in &weights[..n] {
        if w > 11 {
            return Err(Error::Corrupt("huffman weight too large"));
        }
        if w > 0 {
            sum += 1 << (w - 1);
        }
    }
    if sum == 0 {
        return Err(Error::Corrupt("empty huffman tree"));
    }
    let max_bits = 32 - sum.leading_zeros();
    if max_bits > 11 {
        return Err(Error::Corrupt("huffman depth"));
    }
    let rest = (1u32 << max_bits) - sum;
    if rest == 0 || !rest.is_power_of_two() {
        return Err(Error::Corrupt("huffman weights do not complete"));
    }
    weights[n] = (rest.trailing_zeros() + 1) as u8;
    n += 1;
    Ok((n, max_bits, used))
}

#[cfg(test)]
pub(crate) fn check_tree(data: &[u8]) -> Result<(u32, usize)> {
    let mut weights = [0u8; 256];
    read_weights_into(data, &mut weights).map(|(_, bits, used)| (bits, used))
}

/// 解析权重点并直接展开填充到 `table.entries`（2048 项 = 4 KiB），零堆分配。
pub(crate) fn read_tree_into(data: &[u8], table: &mut HuffTable) -> Result<usize> {
    let mut weights = [0u8; 256];
    let (n, max_bits, used) = read_weights_into(data, &mut weights)?;

    let mut counts = [0usize; 12];
    for &w in &weights[..n] {
        counts[w as usize] += 1;
    }

    let mut rank_pos = [0usize; 12];
    let mut pos = 0usize;
    let shift = 11 - max_bits as usize;
    for (w, &cnt) in counts.iter().enumerate().skip(1).take(max_bits as usize) {
        rank_pos[w] = pos;
        pos += cnt << (w - 1 + shift);
    }

    let entries = &mut table.entries;
    for (sym, &w) in weights[..n].iter().enumerate() {
        if w > 0 {
            let wu = w as usize;
            let nb = (max_bits as u16) + 1 - (wu as u16);
            let val = ((sym as u16) << 8) | nb;
            let len = 1usize << (11 - nb as usize);
            let p = rank_pos[wu];
            entries[p..p + len].fill(val);
            rank_pos[wu] = p + len;
        }
    }
    Ok(used)
}

#[inline(always)]
fn finish_stream(entries: &[u16; 2048], r: &mut RevBits<'_>, tail: &mut [u8]) -> Result<()> {
    for o in tail {
        let e = entries[(r.peek(11) as usize) & 2047];
        r.skip((e & 15) as u32);
        *o = (e >> 8) as u8;
    }
    if r.remaining() != 0 {
        return Err(Error::Corrupt("huffman stream length"));
    }
    Ok(())
}

fn decode_single_stream(entries: &[u16; 2048], data: &[u8], out: &mut [u8]) -> Result<()> {
    let mut cur = RevCursor::new(data)?;
    let (chunks, _) = out.as_chunks_mut::<4>();
    let mut done_chunks = 0usize;
    for chunk in chunks {
        if cur.cur < 8 {
            break;
        }
        let mut b = cur.load_msb(data);
        let e0 = entries[(b >> 53) as usize];
        let l0 = (e0 & 15) as u32;
        b <<= l0;
        let e1 = entries[(b >> 53) as usize];
        let l1 = (e1 & 15) as u32;
        b <<= l1;
        let e2 = entries[(b >> 53) as usize];
        let l2 = (e2 & 15) as u32;
        b <<= l2;
        let e3 = entries[(b >> 53) as usize];
        let l3 = (e3 & 15) as u32;
        cur.advance(l0 + l1 + l2 + l3);
        *chunk = [
            (e0 >> 8) as u8,
            (e1 >> 8) as u8,
            (e2 >> 8) as u8,
            (e3 >> 8) as u8,
        ];
        done_chunks += 1;
    }
    let mut r = cur.into_rev(data);
    finish_stream(entries, &mut r, &mut out[done_chunks * 4..])
}

fn stream_parts(data: &[u8], n: usize) -> Result<([&[u8]; 4], usize)> {
    let jt = data.get(..6).ok_or(Error::Corrupt("huffman jump table"))?;
    let s1 = u16::from_le_bytes([jt[0], jt[1]]) as usize;
    let s2 = u16::from_le_bytes([jt[2], jt[3]]) as usize;
    let s3 = u16::from_le_bytes([jt[4], jt[5]]) as usize;
    let body = &data[6..];
    if s1 + s2 + s3 > body.len() {
        return Err(Error::Corrupt("huffman stream sizes"));
    }
    let seg = n.div_ceil(4);
    if seg * 3 > n {
        return Err(Error::Corrupt("huffman 4-stream size"));
    }
    let parts = [
        &body[..s1],
        &body[s1..s1 + s2],
        &body[s1 + s2..s1 + s2 + s3],
        &body[s1 + s2 + s3..],
    ];
    Ok((parts, seg))
}

pub(crate) fn decode_into(t: &HuffTable, data: &[u8], four: bool, out: &mut [u8]) -> Result<()> {
    let entries = &t.entries;
    if !four {
        return decode_single_stream(entries, data, out);
    }
    let (parts, seg) = stream_parts(data, out.len())?;
    let (pa, pb, pc, pd) = (parts[0], parts[1], parts[2], parts[3]);
    let (mut ca, mut cb, mut cc, mut cd) = (
        RevCursor::new(pa)?,
        RevCursor::new(pb)?,
        RevCursor::new(pc)?,
        RevCursor::new(pd)?,
    );
    let (oa, rest) = out.split_at_mut(seg);
    let (ob, rest) = rest.split_at_mut(seg);
    let (oc, od) = rest.split_at_mut(seg);

    let (oa_c, _) = oa.as_chunks_mut::<4>();
    let (ob_c, _) = ob.as_chunks_mut::<4>();
    let (oc_c, _) = oc.as_chunks_mut::<4>();
    let (od_c, _) = od.as_chunks_mut::<4>();

    let pa = &pa[..ca.cur.min(pa.len())];
    let pb = &pb[..cb.cur.min(pb.len())];
    let pc = &pc[..cc.cur.min(pc.len())];
    let pd = &pd[..cd.cur.min(pd.len())];

    let mut done_chunks = 0usize;
    for (((ao, bo), co), do_) in oa_c
        .iter_mut()
        .zip(ob_c.iter_mut())
        .zip(oc_c.iter_mut())
        .zip(od_c.iter_mut())
    {
        if ca.cur.min(cb.cur).min(cc.cur).min(cd.cur) < 8 {
            break;
        }
        let mut ba = ca.load_msb(pa);
        let mut bb = cb.load_msb(pb);
        let mut bc = cc.load_msb(pc);
        let mut bd = cd.load_msb(pd);

        let a0 = entries[(ba >> 53) as usize];
        let b0 = entries[(bb >> 53) as usize];
        let c0 = entries[(bc >> 53) as usize];
        let d0 = entries[(bd >> 53) as usize];
        let la0 = (a0 & 15) as u32;
        let lb0 = (b0 & 15) as u32;
        let lc0 = (c0 & 15) as u32;
        let ld0 = (d0 & 15) as u32;
        ba <<= la0;
        bb <<= lb0;
        bc <<= lc0;
        bd <<= ld0;

        let a1 = entries[(ba >> 53) as usize];
        let b1 = entries[(bb >> 53) as usize];
        let c1 = entries[(bc >> 53) as usize];
        let d1 = entries[(bd >> 53) as usize];
        let la1 = (a1 & 15) as u32;
        let lb1 = (b1 & 15) as u32;
        let lc1 = (c1 & 15) as u32;
        let ld1 = (d1 & 15) as u32;
        ba <<= la1;
        bb <<= lb1;
        bc <<= lc1;
        bd <<= ld1;

        let a2 = entries[(ba >> 53) as usize];
        let b2 = entries[(bb >> 53) as usize];
        let c2 = entries[(bc >> 53) as usize];
        let d2 = entries[(bd >> 53) as usize];
        let la2 = (a2 & 15) as u32;
        let lb2 = (b2 & 15) as u32;
        let lc2 = (c2 & 15) as u32;
        let ld2 = (d2 & 15) as u32;
        ba <<= la2;
        bb <<= lb2;
        bc <<= lc2;
        bd <<= ld2;

        let a3 = entries[(ba >> 53) as usize];
        let b3 = entries[(bb >> 53) as usize];
        let c3 = entries[(bc >> 53) as usize];
        let d3 = entries[(bd >> 53) as usize];

        ca.advance(la0 + la1 + la2 + (a3 & 15) as u32);
        cb.advance(lb0 + lb1 + lb2 + (b3 & 15) as u32);
        cc.advance(lc0 + lc1 + lc2 + (c3 & 15) as u32);
        cd.advance(ld0 + ld1 + ld2 + (d3 & 15) as u32);

        *ao = [
            (a0 >> 8) as u8,
            (a1 >> 8) as u8,
            (a2 >> 8) as u8,
            (a3 >> 8) as u8,
        ];
        *bo = [
            (b0 >> 8) as u8,
            (b1 >> 8) as u8,
            (b2 >> 8) as u8,
            (b3 >> 8) as u8,
        ];
        *co = [
            (c0 >> 8) as u8,
            (c1 >> 8) as u8,
            (c2 >> 8) as u8,
            (c3 >> 8) as u8,
        ];
        *do_ = [
            (d0 >> 8) as u8,
            (d1 >> 8) as u8,
            (d2 >> 8) as u8,
            (d3 >> 8) as u8,
        ];
        done_chunks += 1;
    }

    let done_bytes = done_chunks * 4;
    let mut ra = ca.into_rev(pa);
    let mut rb = cb.into_rev(pb);
    let mut rc = cc.into_rev(pc);
    let mut rd = cd.into_rev(pd);
    finish_stream(entries, &mut ra, &mut oa[done_bytes..])?;
    finish_stream(entries, &mut rb, &mut ob[done_bytes..])?;
    finish_stream(entries, &mut rc, &mut oc[done_bytes..])?;
    finish_stream(entries, &mut rd, &mut od[done_bytes..])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_huff_handles_stream_tails() {
        let mut checked = 0;
        let mut table = HuffTable::default();
        for n in [
            32, 33, 34, 35, 63, 64, 65, 127, 128, 129, 255, 256, 257, 258, 259, 260, 511, 512, 513,
            1023, 1024, 1025, 4095, 4096, 4097,
        ] {
            for seed in 1..=3 {
                let mut state = seed * 0x123456789u64;
                let data: Vec<u8> = (0..n)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        let x = (state >> 32) as u32 & 4095;
                        match seed {
                            1 => (x % 5) as u8,
                            2 => ((x * x / 4096) / 16) as u8,
                            _ => ((x % 31).min(7)) as u8,
                        }
                    })
                    .collect();
                let Some(encoded) = crate::henc::encode_literals(&data) else {
                    continue;
                };
                let sf = (encoded[0] >> 2) & 3;
                let header = match sf {
                    0 | 1 => 3,
                    2 => 4,
                    _ => 5,
                };
                let used = read_tree_into(&encoded[header..], &mut table).unwrap();
                let mut output = vec![0; n];
                decode_into(&table, &encoded[header + used..], sf != 0, &mut output).unwrap();
                assert_eq!(output, data, "n={n} seed={seed}");
                checked += 1;
            }
        }
        assert!(checked > 30);
    }
}
