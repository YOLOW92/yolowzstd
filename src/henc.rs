//! Huffman 字面量编码：O(n) Kraft 截断限长码、香农熵/KL 散度早退、打包表 5 符号批编码。
use crate::enc::BitAcc;
use crate::entropy::{log2_fp8, normalize_into, write_dist_into};
use crate::fse;

/// 跨块复用的 Huffman 编码暂存与上块激活树（支持无头复用类型 3）。
pub(crate) struct HuffScratch {
    sbuf: [Vec<u8>; 4],
    desc: Vec<u8>,
    cand_len: [u8; 256],
    /// 打包编码表：`(code << 8) | len`，单次访存同时获取码字与码长。
    cand_packed: [u32; 256],
    cand_fresh: bool,
    active_valid: bool,
    active_len: [u8; 256],
    active_packed: [u32; 256],
}

impl HuffScratch {
    pub fn new() -> Self {
        Self {
            sbuf: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            desc: Vec::with_capacity(140),
            cand_len: [0; 256],
            cand_packed: [0; 256],
            cand_fresh: false,
            active_valid: false,
            active_len: [0; 256],
            active_packed: [0; 256],
        }
    }

    #[inline]
    pub fn reset_frame(&mut self) {
        self.active_valid = false;
        self.cand_fresh = false;
    }

    #[inline]
    pub fn commit_block(&mut self) {
        if self.cand_fresh {
            self.active_valid = true;
            self.active_len = self.cand_len;
            self.active_packed = self.cand_packed;
            self.cand_fresh = false;
        }
    }
}

/// 统计字节直方图（长块用 4 路累加避免重复字节停顿，短块只清 1 KiB）。
#[inline]
fn count_bytes(data: &[u8]) -> [u32; 256] {
    let mut c0 = [0u32; 256];
    if data.len() < 1024 {
        for &b in data {
            c0[b as usize] += 1;
        }
        return c0;
    }
    let mut c1 = [0u32; 256];
    let mut c2 = [0u32; 256];
    let mut c3 = [0u32; 256];
    let (chunks, rem) = data.as_chunks::<4>();
    for &[a, b, c, d] in chunks {
        c0[a as usize] += 1;
        c1[b as usize] += 1;
        c2[c as usize] += 1;
        c3[d as usize] += 1;
    }
    for &b in rem {
        c0[b as usize] += 1;
    }
    for i in 0..256 {
        c0[i] += c1[i] + c2[i] + c3[i];
    }
    c0
}

/// O(n) 限长（≤ max_len）Huffman 码长：单次建树 + Kraft 不等式精确截断（Moffat & Katajainen 1995 / HUF_setMaxHeight），零堆分配。
fn lengths_into(freq: &[u32; 256], max_len: u8, out: &mut [u8; 256]) -> (u8, usize) {
    let mut leaves = [(0u32, 0u8); 256];
    let mut n = 0usize;
    let mut lastsym = 0usize;
    for (s, &c) in freq.iter().enumerate() {
        if c > 0 {
            leaves[n] = (c, s as u8);
            n += 1;
            lastsym = s;
        }
    }
    out.fill(0);
    if n == 0 {
        return (0, 0);
    }
    if n == 1 {
        let sym = leaves[0].1 as usize;
        out[sym] = 1;
        return (1, sym);
    }
    leaves[..n].sort_unstable();
    let mut weight = [0u64; 512];
    let mut parent = [0u16; 512];
    let mut depth = [0u8; 512];
    for i in 0..n {
        weight[i] = leaves[i].0 as u64;
    }
    let mut total = n;
    let (mut li, mut ii) = (0usize, n);
    for _ in 0..n - 1 {
        let mut pick = || -> usize {
            let leaf_ok = li < n;
            let int_ok = ii < total;
            if leaf_ok && (!int_ok || weight[li] <= weight[ii]) {
                li += 1;
                li - 1
            } else {
                ii += 1;
                ii - 1
            }
        };
        let a = pick();
        let b = pick();
        let id = total;
        weight[id] = weight[a] + weight[b];
        parent[a] = id as u16;
        parent[b] = id as u16;
        total += 1;
    }
    let root = total - 1;
    depth[root] = 0;
    for i in (0..root).rev() {
        depth[i] = depth[parent[i] as usize] + 1;
    }
    let mut maxlen = *depth[..n].iter().max().unwrap_or(&0);
    if maxlen > max_len {
        let target = 1i32 << max_len;
        let mut kraft = 0i32;
        for d in &mut depth[..n] {
            if *d > max_len {
                *d = max_len;
            }
            kraft += 1i32 << (max_len - *d);
        }
        while kraft > target {
            for d in &mut depth[..n] {
                if *d < max_len {
                    *d += 1;
                    kraft -= 1i32 << (max_len - *d);
                    if kraft <= target {
                        break;
                    }
                }
            }
        }
        while kraft < target {
            for d in depth[..n].iter_mut().rev() {
                if *d > 1 {
                    let step = 1i32 << (max_len - *d);
                    if kraft + step <= target {
                        *d -= 1;
                        kraft += step;
                        if kraft == target {
                            break;
                        }
                    }
                }
            }
        }
        debug_assert_eq!(kraft, target);
        maxlen = *depth[..n].iter().max().unwrap_or(&0);
    }
    for (k, &(_, s)) in leaves[..n].iter().enumerate() {
        out[s as usize] = depth[k];
    }
    (maxlen, lastsym)
}

/// 把权重表写成 FSE 压缩形式（带 1 字节总长前缀）直接追加到 `out`；不满足压缩条件时返回 false（不污染 `out`）。
fn try_write_fse_weights(w: &[u8], out: &mut Vec<u8>) -> bool {
    const AL: u32 = 6;
    const SIZE: usize = 1 << AL;
    let n = w.len();
    if !(3..=255).contains(&n) {
        return false;
    }
    let mut cnt = [0u32; 16];
    let mut max_w = 0usize;
    let mut active_w = 0usize;
    for &x in w {
        let xi = x as usize;
        if xi >= 16 {
            return false;
        }
        if cnt[xi] == 0 {
            active_w += 1;
        }
        cnt[xi] += 1;
        if xi > max_w {
            max_w = xi;
        }
    }
    // RFC 8878 §4.2.1.2：单符号 FSE 权重表所有状态格 nb == 0，解码器无法通过位流溢出终止
    if active_w <= 1 {
        return false;
    }
    let mut probs = [0i32; 16];
    if normalize_into(&cnt[..=max_w], AL, &mut probs).is_none() {
        return false;
    }
    let start_len = out.len();
    out.push(0);
    if write_dist_into(&probs[..=max_w], AL, out).is_none() {
        out.truncate(start_len);
        return false;
    }
    let mut dist = [0i16; 16];
    for i in 0..=max_w {
        dist[i] = probs[i] as i16;
    }
    let mut cells = [fse::Cell::default(); SIZE];
    if fse::build_into(&dist[..=max_w], AL, &mut cells).is_err() {
        out.truncate(start_len);
        return false;
    }
    let mut head = [u8::MAX; 16];
    let mut next_cell = [u8::MAX; SIZE];
    for i in (0..SIZE).rev() {
        let s = cells[i].sym as usize;
        next_cell[i] = head[s];
        head[s] = i as u8;
    }
    let mut state_pos = [0u8; 256];
    let mut bw = BitAcc::new();
    for k in (0..n).rev() {
        let sym = w[k] as usize;
        let mut idx = head[sym];
        if k + 2 < n {
            let next_st = state_pos[k + 2] as usize;
            let mut found = None;
            while idx != u8::MAX {
                let c = cells[idx as usize];
                let base = c.base as usize;
                let span = 1usize << c.nb;
                if next_st >= base && next_st < base + span {
                    found = Some((idx, c.nb as u32, (next_st - base) as u32));
                    break;
                }
                idx = next_cell[idx as usize];
            }
            let Some((chosen, nb, bits_val)) = found else {
                out.truncate(start_len);
                return false;
            };
            state_pos[k] = chosen;
            bw.put(out, bits_val, nb);
        } else {
            if k == n - 2 {
                while idx != u8::MAX && cells[idx as usize].nb == 0 {
                    idx = next_cell[idx as usize];
                }
            }
            if idx == u8::MAX {
                out.truncate(start_len);
                return false;
            }
            state_pos[k] = idx;
        }
    }
    bw.put(out, state_pos[1] as u32, AL);
    bw.put(out, state_pos[0] as u32, AL);
    bw.finish(out);
    let payload_len = out.len() - start_len - 1;
    if payload_len >= 128 {
        out.truncate(start_len);
        return false;
    }
    out[start_len] = payload_len as u8;
    true
}

#[inline]
fn build_packed_codes(len: &[u8; 256], maxlen: u8, lastsym: usize, packed: &mut [u32; 256]) {
    packed.fill(0);
    let mut pos = 0u32;
    for w in 1..=maxlen {
        let nb = maxlen + 1 - w;
        let step = 1u32 << (w - 1);
        for s in 0..=lastsym {
            if len[s] > 0 && (maxlen + 1 - len[s]) == w {
                let code = pos >> (maxlen - nb);
                packed[s] = (code << 8) | (nb as u32);
                pos += step;
            }
        }
    }
}

#[inline]
fn encode_huff_part(part: &[u8], packed: &[u32; 256], out: &mut Vec<u8>) -> usize {
    let need = part.len() * 2 + 16;
    if out.len() < need {
        out.resize(need, 0);
    }
    let buf = &mut out[..need];
    let mut wp = 0usize;
    let mut acc = 0u64;
    let mut nbits = 0u32;
    let mut chunks = part.rchunks_exact(5);
    for ch in &mut chunks {
        let pe = packed[ch[4] as usize];
        let pd = packed[ch[3] as usize];
        let pc = packed[ch[2] as usize];
        let pb = packed[ch[1] as usize];
        let pa = packed[ch[0] as usize];
        let (ne, nd, nc, nb, na) = (pe & 15, pd & 15, pc & 15, pb & 15, pa & 15);
        let s1 = ne;
        let s2 = s1 + nd;
        let s3 = s2 + nc;
        let s4 = s3 + nb;
        let bits = ((pe >> 8) as u64)
            | (((pd >> 8) as u64) << s1)
            | (((pc >> 8) as u64) << s2)
            | (((pb >> 8) as u64) << s3)
            | (((pa >> 8) as u64) << s4);
        acc |= bits << nbits;
        nbits += s4 + na;
        buf[wp..wp + 8].copy_from_slice(&acc.to_le_bytes());
        let bytes = (nbits >> 3) as usize;
        wp += bytes;
        acc >>= bytes << 3;
        nbits &= 7;
    }
    for &byte in chunks.remainder().iter().rev() {
        let p = packed[byte as usize];
        acc |= ((p >> 8) as u64) << nbits;
        nbits += p & 15;
        while nbits >= 8 {
            buf[wp] = acc as u8;
            wp += 1;
            acc >>= 8;
            nbits -= 8;
        }
    }
    acc |= 1u64 << nbits;
    nbits += 1;
    while nbits > 0 {
        buf[wp] = acc as u8;
        wp += 1;
        acc >>= 8;
        nbits = nbits.saturating_sub(8);
    }
    wp
}

#[inline]
pub(crate) fn write_raw_or_rle_header(out: &mut Vec<u8>, ty: u8, n: usize) {
    if n < 32 {
        out.push(ty | ((n as u8) << 3));
    } else if n < 4096 {
        out.push(ty | (1 << 2) | (((n & 15) as u8) << 4));
        out.push((n >> 4) as u8);
    } else {
        out.push(ty | (3 << 2) | (((n & 15) as u8) << 4));
        out.push((n >> 4) as u8);
        out.push((n >> 12) as u8);
    }
}

/// 将 `lits` 按最省空间的方式（RLE / 无头复用 Huffman / 新建 Huffman / Raw）直接写入 `out`。
#[inline(never)]
pub(crate) fn write_literals_into(
    lits: &[u8],
    out: &mut Vec<u8>,
    scratch: &mut HuffScratch,
    allow_huff: bool,
    fast_skip: bool,
) {
    scratch.cand_fresh = false;
    let n = lits.len();
    if n == 0 {
        out.push(0);
        return;
    }
    if n < 96 || !allow_huff {
        if n >= 3 && lits.iter().all(|&b| b == lits[0]) {
            write_raw_or_rle_header(out, 1, n);
            out.push(lits[0]);
            return;
        }
        write_raw_or_rle_header(out, 0, n);
        out.extend_from_slice(lits);
        return;
    }

    let freq = count_bytes(lits);
    if freq[lits[0] as usize] as usize == n {
        write_raw_or_rle_header(out, 1, n);
        out.push(lits[0]);
        return;
    }

    let streams = if n >= 256 { 4usize } else { 1usize };
    let jump_bytes = if streams == 4 { 6 } else { 0 };
    let min_hl = if n >= 16384 {
        5
    } else if n >= 1024 {
        4
    } else {
        3
    };
    let min_gain = if fast_skip && n >= 4096 {
        (n as u64) >> 4
    } else {
        0
    };

    let log2_n = log2_fp8(n as u32) as u64;
    let mut shannon_fp8 = 0u64;
    let mut prev_ok = scratch.active_valid;
    let mut prev_bits = 0u64;
    for (&c, &al) in freq.iter().zip(scratch.active_len.iter()) {
        if c > 0 {
            let cu = c as u64;
            shannon_fp8 += cu * log2_n.saturating_sub(log2_fp8(c) as u64);
            if prev_ok {
                if al == 0 {
                    prev_ok = false;
                } else {
                    prev_bits += cu * al as u64;
                }
            }
        }
    }
    let shannon_bits = shannon_fp8 >> 8;
    if !prev_ok {
        prev_bits = u64::MAX;
    }

    let shannon_lb = ((shannon_bits + min_gain + streams as u64).div_ceil(8))
        + jump_bytes as u64
        + min_hl as u64;
    if shannon_lb >= n as u64 {
        write_raw_or_rle_header(out, 0, n);
        out.extend_from_slice(lits);
        return;
    }

    let fast_repeat = prev_bits != u64::MAX && prev_bits <= shannon_bits.saturating_add(280);

    let (ty, use_packed) = if fast_repeat {
        scratch.desc.clear();
        let lb = ((prev_bits + min_gain + streams as u64).div_ceil(8))
            + jump_bytes as u64
            + min_hl as u64;
        if lb >= n as u64 {
            write_raw_or_rle_header(out, 0, n);
            out.extend_from_slice(lits);
            return;
        }
        (3u64, &scratch.active_packed)
    } else {
        let (maxlen, lastsym) = lengths_into(&freq, 11, &mut scratch.cand_len);
        let fresh_bits: u64 = if maxlen > 0 {
            freq.iter()
                .zip(&scratch.cand_len)
                .map(|(&c, &l)| c as u64 * l as u64)
                .sum()
        } else {
            u64::MAX
        };
        let use_repeat = prev_bits != u64::MAX && prev_bits <= fresh_bits.saturating_add(320);
        if use_repeat {
            scratch.desc.clear();
            let lb = ((prev_bits + min_gain + streams as u64).div_ceil(8))
                + jump_bytes as u64
                + min_hl as u64;
            if lb >= n as u64 {
                write_raw_or_rle_header(out, 0, n);
                out.extend_from_slice(lits);
                return;
            }
            (3u64, &scratch.active_packed)
        } else {
            if maxlen == 0 {
                write_raw_or_rle_header(out, 0, n);
                out.extend_from_slice(lits);
                return;
            }
            let lb = ((fresh_bits + min_gain + streams as u64).div_ceil(8)) + jump_bytes as u64 + 4;
            if lb >= n as u64 {
                write_raw_or_rle_header(out, 0, n);
                out.extend_from_slice(lits);
                return;
            }
            let mut weight = [0u8; 256];
            for (w_dst, &cl) in weight[..=lastsym]
                .iter_mut()
                .zip(scratch.cand_len[..=lastsym].iter())
            {
                if cl > 0 {
                    *w_dst = maxlen + 1 - cl;
                }
            }
            let shown = &weight[..lastsym];
            scratch.desc.clear();
            let fse_ok = try_write_fse_weights(shown, &mut scratch.desc);
            if shown.len() <= 128 {
                let direct_len = 1 + shown.len().div_ceil(2);
                if !fse_ok || direct_len <= scratch.desc.len() {
                    scratch.desc.clear();
                    scratch.desc.push((127 + shown.len()) as u8);
                    for c in shown.chunks(2) {
                        scratch
                            .desc
                            .push((c[0] << 4) | c.get(1).copied().unwrap_or(0));
                    }
                }
            } else if !fse_ok {
                write_raw_or_rle_header(out, 0, n);
                out.extend_from_slice(lits);
                return;
            }
            if prev_bits != u64::MAX
                && prev_bits <= fresh_bits.saturating_add((scratch.desc.len() as u64) << 3)
            {
                scratch.desc.clear();
                (3u64, &scratch.active_packed)
            } else {
                build_packed_codes(&scratch.cand_len, maxlen, lastsym, &mut scratch.cand_packed);
                (2u64, &scratch.cand_packed)
            }
        }
    };

    let four = n >= 256;
    let mut part_lens = [0usize; 4];
    let stream_len = if four {
        let seg = n.div_ceil(4);
        let mut chunk_idx = 0usize;
        let mut total = 6usize;
        for chunk in lits.chunks(seg) {
            if chunk_idx >= 4 {
                break;
            }
            let pl = encode_huff_part(chunk, use_packed, &mut scratch.sbuf[chunk_idx]);
            part_lens[chunk_idx] = pl;
            total += pl;
            chunk_idx += 1;
        }
        if chunk_idx != 4 || part_lens[..3].iter().any(|&l| l > 65535) {
            write_raw_or_rle_header(out, 0, n);
            out.extend_from_slice(lits);
            return;
        }
        total
    } else {
        part_lens[0] = encode_huff_part(lits, use_packed, &mut scratch.sbuf[0]);
        part_lens[0]
    };

    let comp = scratch.desc.len() + stream_len;
    let (sf, bits, hl) = if !four {
        (0u64, 10u32, 3usize)
    } else if n < 1024 && comp < 1024 {
        (1, 10, 3)
    } else if n < 16384 && comp < 16384 {
        (2, 14, 4)
    } else {
        (3, 18, 5)
    };
    if comp >= (1 << bits) || n >= (1 << bits) || comp + hl >= n {
        write_raw_or_rle_header(out, 0, n);
        out.extend_from_slice(lits);
        return;
    }
    let hv = ty | (sf << 2) | ((n as u64) << 4) | ((comp as u64) << (4 + bits));
    out.extend_from_slice(&hv.to_le_bytes()[..hl]);
    out.extend_from_slice(&scratch.desc);
    if four {
        for &pl in &part_lens[..3] {
            out.extend_from_slice(&(pl as u16).to_le_bytes());
        }
        for (buf, &pl) in scratch.sbuf.iter().zip(&part_lens) {
            out.extend_from_slice(&buf[..pl]);
        }
    } else {
        out.extend_from_slice(&scratch.sbuf[0][..part_lens[0]]);
    }
    if ty == 2 {
        scratch.cand_fresh = true;
    }
}

/// 生成压缩的字面量段（含段头）；退回 Raw 时返回 None。保留给单测和独立调用。
#[cfg(test)]
pub(crate) fn encode_literals(lits: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut scratch = HuffScratch::new();
    write_literals_into(lits, &mut out, &mut scratch, true, false);
    if (out[0] & 3) == 0 { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huff;

    #[test]
    fn encode_literals_tree_roundtrips_through_decoder() {
        let mut lits = Vec::new();
        for i in 0..4096 {
            lits.push(((i * 37) % 19) as u8);
        }
        let encoded = encode_literals(&lits).unwrap();
        assert_eq!(encoded[0] & 3, 2);
        let comp_bits = if (encoded[0] >> 2) & 3 == 2 { 14 } else { 18 };
        let hl = if comp_bits == 14 { 4 } else { 5 };
        let (bits, used) = huff::check_tree(&encoded[hl..]).unwrap();
        assert!(bits <= 11);
        assert!(used > 0);
    }

    #[test]
    fn fse_weights_reject_single_weight_and_handle_zero_nb_states() {
        let mut out = Vec::new();
        assert!(!try_write_fse_weights(&[1u8; 12], &mut out));
        assert!(out.is_empty());
        assert!(!try_write_fse_weights(&[1u8; 192], &mut out));
        assert!(out.is_empty());

        for zero_pos in [0usize, 1, 95, 190, 191] {
            let mut w = vec![1u8; 192];
            w[zero_pos] = 0;
            out.clear();
            assert!(try_write_fse_weights(&w, &mut out), "zero_pos={zero_pos}");
            let hb = out[0] as usize;
            let body = &out[1..1 + hb];
            let mut probs = [0i16; 256];
            let (plen, al, off) = fse::read_dist_into(body, 255, 6, &mut probs).unwrap();
            let mut cells = [fse::Cell::default(); 64];
            fse::build_into(&probs[..plen], al, &mut cells).unwrap();
            let mut r = crate::bits::RevBits::new(&body[off..]).unwrap();
            let mut s1 = r.read(al) as usize;
            let mut s2 = r.read(al) as usize;
            let mut decoded = Vec::new();
            loop {
                let c = cells[s1 & 63];
                decoded.push(c.sym);
                s1 = c.base as usize + r.read(c.nb as u32) as usize;
                if r.overflow() {
                    decoded.push(cells[s2 & 63].sym);
                    break;
                }
                let c = cells[s2 & 63];
                decoded.push(c.sym);
                s2 = c.base as usize + r.read(c.nb as u32) as usize;
                if r.overflow() {
                    decoded.push(cells[s1 & 63].sym);
                    break;
                }
            }
            assert_eq!(decoded, w, "zero_pos={zero_pos}");
        }
    }

    #[test]
    fn kraft_truncation_handles_deep_fibonacci_tree() {
        let mut freq = [0u32; 256];
        let (mut a, mut b) = (1u32, 1u32);
        for slot in freq.iter_mut().take(24) {
            *slot = a;
            let next = a + b;
            a = b;
            b = next;
        }
        let mut lens = [0u8; 256];
        let (maxlen, lastsym) = lengths_into(&freq, 11, &mut lens);
        assert!((1..=11).contains(&maxlen));
        assert_eq!(lastsym, 23);
        let kraft: u32 = lens[..=lastsym]
            .iter()
            .filter(|&&l| l > 0)
            .map(|&l| 1u32 << (maxlen - l))
            .sum();
        assert_eq!(kraft, 1u32 << maxlen);
    }
}
