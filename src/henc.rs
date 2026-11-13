//! Huffman 字面量编码：限长码、权重表（直接 4 位或 FSE 压缩）、1 流/4 流，零每块堆分配。
use crate::enc::BitAcc;
use crate::fse;

/// 跨块复用的 Huffman 编码暂存与上块激活树（支持无头复用类型 3）。
pub(crate) struct HuffScratch {
    sbuf: [Vec<u8>; 4],
    desc: Vec<u8>,
    cand_len: [u8; 256],
    cand_code: [u32; 256],
    cand_fresh: bool,
    active_valid: bool,
    active_len: [u8; 256],
    active_code: [u32; 256],
}

impl HuffScratch {
    pub fn new() -> Self {
        Self {
            sbuf: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            desc: Vec::with_capacity(140),
            cand_len: [0; 256],
            cand_code: [0; 256],
            cand_fresh: false,
            active_valid: false,
            active_len: [0; 256],
            active_code: [0; 256],
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
            self.active_code = self.cand_code;
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

/// 限长（≤ max_len）Huffman 码长；全在栈上完成，无堆分配。
fn lengths_into(freq: &[u32; 256], max_len: u8, out: &mut [u8; 256]) -> (u8, usize) {
    let mut f = *freq;
    let mut leaves = [(0u32, 0u8); 256];
    let mut weight = [0u64; 512];
    let mut parent = [0u16; 512];
    let mut depth = [0u8; 512];
    loop {
        let mut n = 0usize;
        for (s, &c) in f.iter().enumerate() {
            if c > 0 {
                leaves[n] = (c, s as u8);
                n += 1;
            }
        }
        if n == 0 {
            out.fill(0);
            return (0, 0);
        }
        if n == 1 {
            out.fill(0);
            let sym = leaves[0].1 as usize;
            out[sym] = 1;
            return (1, sym);
        }
        leaves[..n].sort_unstable();
        for i in 0..n {
            weight[i] = leaves[i].0 as u64;
        }
        parent[..2 * n].fill(0);
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
        let maxlen = *depth[..n].iter().max().unwrap_or(&0);
        if maxlen <= max_len {
            out.fill(0);
            let mut lastsym = 0usize;
            for &(.., s) in &leaves[..n] {
                if s as usize > lastsym {
                    lastsym = s as usize;
                }
            }
            for (k, &(_, s)) in leaves[..n].iter().enumerate() {
                out[s as usize] = depth[k];
            }
            return (maxlen, lastsym);
        }
        for x in f.iter_mut() {
            if *x > 0 {
                *x = x.div_ceil(2);
            }
        }
    }
}

/// 把计数归一化到总和 2^al(每个出现的符号至少 1);写入调用方 `probs` 切片。
pub(crate) fn normalize_into(cnt: &[u32], al: u32, probs: &mut [i32]) -> Option<()> {
    let len = cnt.len();
    let size = 1i64 << al;
    let n: u64 = cnt.iter().map(|&c| c as u64).sum();
    if n == 0 || probs.len() < len {
        return None;
    }
    let probs = &mut probs[..len];
    probs.fill(0);
    let mut sum = 0i64;
    let half = n / 2;
    for i in 0..len {
        let c = cnt[i];
        if c > 0 {
            let p = (((c as u64 * size as u64 + half) / n) as i32).max(1);
            probs[i] = p;
            sum += p as i64;
        }
    }
    if sum <= size {
        let mut big = 0usize;
        for i in 1..len {
            if probs[i] >= probs[big] {
                big = i;
            }
        }
        probs[big] += (size - sum) as i32;
    } else {
        let mut excess = sum - size;
        while excess > 0 {
            let mut big = 0usize;
            for i in 1..len {
                if probs[i] >= probs[big] {
                    big = i;
                }
            }
            let take = excess.min((probs[big] - 1) as i64);
            if take <= 0 {
                return None;
            }
            probs[big] -= take as i32;
            excess -= take;
        }
    }
    Some(())
}

/// 保留旧接口供测试与零碎调用使用。
#[cfg(test)]
pub(crate) fn normalize(cnt: &[u32], al: u32) -> Option<Vec<i32>> {
    let mut probs = vec![0i32; cnt.len()];
    normalize_into(cnt, al, &mut probs)?;
    Some(probs)
}

/// 写 FSE 分布表头(`probs` 非负;到最后一个出现的符号为止),直接写入 `out`。
pub(crate) fn write_dist_into(probs: &[i32], al: u32, out: &mut Vec<u8>) -> Option<()> {
    let last = probs.iter().rposition(|&p| p > 0)?;
    let mut bw = BitAcc::new();
    bw.put(out, al - 5, 4);
    let (mut remaining, mut thr, mut nbits) = ((1i32 << al) + 1, 1i32 << al, al + 1);
    let mut s = 0usize;
    while remaining > 1 && s <= last {
        let x = probs[s] + 1;
        let max = 2 * thr - 1 - remaining;
        if x < max {
            bw.put(out, x as u32, nbits - 1);
        } else if x >= thr {
            bw.put(out, (x + max) as u32, nbits);
        } else {
            bw.put(out, x as u32, nbits);
        }
        remaining -= probs[s];
        if probs[s] == 0 {
            let mut z = 1;
            while s + z <= last && probs[s + z] == 0 {
                z += 1;
            }
            let mut r = z - 1;
            while r >= 3 {
                bw.put(out, 3, 2);
                r -= 3;
            }
            bw.put(out, r as u32, 2);
            s += z;
        } else {
            s += 1;
        }
        while remaining < thr {
            nbits -= 1;
            thr >>= 1;
        }
    }
    if remaining != 1 {
        return None;
    }
    bw.flush_bytes(out);
    Some(())
}

/// 把权重表写成 FSE 压缩形式（带 1 字节总长前缀）直接追加到 `out`；不满足压缩条件时返回 None（不污染 `out`）。
fn try_write_fse_weights(w: &[u8], out: &mut Vec<u8>) -> bool {
    const AL: u32 = 6;
    const SIZE: usize = 1 << AL;
    let n = w.len();
    if !(3..=255).contains(&n) {
        return false;
    }
    let mut cnt = [0u32; 16];
    let mut max_w = 0usize;
    for &x in w {
        let xi = x as usize;
        if xi >= 16 {
            return false;
        }
        cnt[xi] += 1;
        if xi > max_w {
            max_w = xi;
        }
    }
    let mut probs = [0i32; 16];
    if normalize_into(&cnt[..=max_w], AL, &mut probs).is_none() {
        return false;
    }
    let start_len = out.len();
    out.push(0); // 占位总长度字节
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
    // 按符号串接状态格链表，O(1) 定位（每个符号仅占极少格子）
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
fn build_canonical_codes(len: &[u8; 256], maxlen: u8, lastsym: usize, code: &mut [u32; 256]) {
    code.fill(0);
    let mut pos = 0u32;
    for w in 1..=maxlen {
        let nb = maxlen + 1 - w;
        let step = 1u32 << (w - 1);
        for s in 0..=lastsym {
            if len[s] > 0 && (maxlen + 1 - len[s]) == w {
                code[s] = pos >> (maxlen - nb);
                pos += step;
            }
        }
    }
}

#[inline]
fn encode_huff_part(part: &[u8], len: &[u8; 256], code: &[u32; 256], out: &mut Vec<u8>) -> usize {
    let need = part.len() * 2 + 16;
    if out.len() < need {
        out.resize(need, 0);
    }
    let buf = &mut out[..need];
    let mut wp = 0usize;
    let mut acc = 0u64;
    let mut nbits = 0u32;
    let mut chunks = part.rchunks_exact(4);
    for ch in &mut chunks {
        let (a, b, c, d) = (
            ch[0] as usize,
            ch[1] as usize,
            ch[2] as usize,
            ch[3] as usize,
        );
        let (na, nb, nc, nd) = (len[a] as u32, len[b] as u32, len[c] as u32, len[d] as u32);
        let bits = (code[d] as u64)
            | ((code[c] as u64) << nd)
            | ((code[b] as u64) << (nd + nc))
            | ((code[a] as u64) << (nd + nc + nb));
        acc |= bits << nbits;
        nbits += na + nb + nc + nd;
        if let Some(slot) = buf.get_mut(wp..wp + 8) {
            slot.copy_from_slice(&acc.to_le_bytes());
        }
        let bytes = (nbits >> 3) as usize;
        wp += bytes;
        acc >>= bytes << 3;
        nbits &= 7;
    }
    for &byte in chunks.remainder().iter().rev() {
        let bi = byte as usize;
        acc |= (code[bi] as u64) << nbits;
        nbits += len[bi] as u32;
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

    // 先检查上一块激活的 Huffman 树是否全覆盖且编码体积更有优势。
    let mut prev_bits = u64::MAX;
    if scratch.active_valid {
        let mut ok = true;
        let mut pb = 0u64;
        for (&c, &l) in freq.iter().zip(scratch.active_len.iter()) {
            if c > 0 {
                if l == 0 {
                    ok = false;
                    break;
                }
                pb += c as u64 * l as u64;
            }
        }
        if ok {
            prev_bits = pb;
        }
    }

    let (maxlen, lastsym) = lengths_into(&freq, 11, &mut scratch.cand_len);
    let fresh_bits: u64 = if maxlen > 0 {
        freq.iter()
            .zip(&scratch.cand_len)
            .map(|(&c, &l)| c as u64 * l as u64)
            .sum()
    } else {
        u64::MAX
    };

    // 如果可复用旧树且未明显劣于重新建树，则使用无树头类型 3，省去权重压缩和树描述。
    let use_repeat = prev_bits != u64::MAX && prev_bits <= fresh_bits.saturating_add(320);

    let min_gain = if fast_skip && n >= 4096 {
        (n as u64) >> 2
    } else {
        0
    };
    let (ty, use_len, use_code) = if use_repeat {
        scratch.desc.clear();
        let lb = ((prev_bits + min_gain + streams as u64).div_ceil(8))
            + jump_bytes as u64
            + min_hl as u64;
        if lb >= n as u64 {
            write_raw_or_rle_header(out, 0, n);
            out.extend_from_slice(lits);
            return;
        }
        (3u64, &scratch.active_len, &scratch.active_code)
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
        build_canonical_codes(&scratch.cand_len, maxlen, lastsym, &mut scratch.cand_code);
        (2u64, &scratch.cand_len, &scratch.cand_code)
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
            let pl = encode_huff_part(chunk, use_len, use_code, &mut scratch.sbuf[chunk_idx]);
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
        part_lens[0] = encode_huff_part(lits, use_len, use_code, &mut scratch.sbuf[0]);
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
mod normalization_tests {
    use super::*;
    use crate::huff;

    #[test]
    fn rounding_preserves_every_present_symbol() {
        let counts: Vec<u32> = (1..=256).collect();
        let p = normalize(&counts, 8).unwrap();
        assert_eq!(p.iter().sum::<i32>(), 256);
        assert!(p.iter().all(|&v| v == 1));
        assert!(normalize(&[1; 257], 8).is_none());
        assert!(normalize(&[0; 32], 8).is_none());
    }

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
}
