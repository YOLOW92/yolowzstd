//! 共享信息论与统计学原语：8.8 定点对数、凸最小 KL 散度 FSE 概率归一化、分布表头编码与 Rissanen MDL 表精度选择。
use crate::enc::BitAcc;

/// 8.8 定点 log2(1 + i/256) 查表，精度 < 0.005 bit，用于无浮点信息熵与交叉熵计算。
const LOG2_FRAC: [u16; 256] = {
    let mut tab = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        // 令 x = 256 + i，以 Q28 缩放（最大 < 2^29，平方 < 2^58 不溢出 u64）迭代求 8 位定点小数
        let mut v = ((256 + i) as u64) << 20; // Q28，范围 [1.0, 2.0)
        let mut frac = 0u32;
        let mut bit = 128u32;
        while bit > 0 {
            v = (v * v) >> 28;
            if v >= (2u64 << 28) {
                v >>= 1;
                frac |= bit;
            }
            bit >>= 1;
        }
        v = (v * v) >> 28;
        if v >= (2u64 << 28) {
            frac += 1;
        }
        tab[i] = frac as u16;
        i += 1;
    }
    tab
};

/// 返回 8.8 定点 `log2(x)`（`x >= 1`；`256` 表示 `1.0 bit`）。
#[inline(always)]
pub(crate) fn log2_fp8(x: u32) -> u32 {
    if x <= 1 {
        return 0;
    }
    let k = 31 - x.leading_zeros();
    let frac = if k >= 8 {
        ((x >> (k - 8)) & 255) as usize
    } else {
        ((x << (8 - k)) & 255) as usize
    };
    (k << 8) + LOG2_FRAC[frac] as u32
}

/// MDL 准则（Rissanen 1978）选择最优 FSE 表精度：权衡表头描述长度与量化 KL 损失。
#[inline]
pub(crate) fn opt_fse_al(total_seqs: usize, active_cnt: u32, max_al: u32) -> u32 {
    let mut al = if total_seqs < 64 {
        5
    } else if total_seqs < 128 {
        6
    } else if total_seqs < 512 {
        7
    } else if total_seqs < 2048 {
        8
    } else {
        9
    }
    .min(max_al);
    while (1u32 << al) < active_cnt && al < max_al {
        al += 1;
    }
    al
}

/// 凸最优最小 KL 散度 FSE 概率归一化（Gross 1956 / Duda 2013 / RFC 8878 `-1` 极低频符号支持）：
/// 在 `Σ |p_i| = 2^al` 约束下最小化交叉熵 `-Σ c_i log2(|p_i| / 2^al)`。
pub(crate) fn normalize_into(cnt: &[u32], al: u32, probs: &mut [i32]) -> Option<()> {
    let len = cnt.len();
    let size = 1i64 << al;
    let mut n = 0u64;
    let mut active = 0i64;
    for &c in cnt {
        if c > 0 {
            n += c as u64;
            active += 1;
        }
    }
    if n == 0 || active > size || probs.len() < len {
        return None;
    }
    let probs = &mut probs[..len];
    probs.fill(0);
    if active == size {
        for i in 0..len {
            if cnt[i] > 0 {
                probs[i] = 1;
            }
        }
        return Some(());
    }
    let mut used = 0i64;
    let size_u = size as u64;
    for i in 0..len {
        let c = cnt[i];
        if c > 0 {
            // 真实概率显著低于 0.5 格时使用 RFC 8878 的 `-1`（占 1 格且分布表头编码值 0 更省位）。
            if (c as u64) * size_u * 2 < n {
                probs[i] = -1;
                used += 1;
            } else {
                // Webster/Sainte-Laguë 离散 KKT 初值：四舍五入使残差由 O(active/2) 降至 O(sqrt(active))。
                let p = ((c as u64 * size_u + (n >> 1)) / n).max(1) as i32;
                probs[i] = p;
                used += p as i64;
            }
        }
    }
    if used < size {
        // 凸离散资源分配（边际 KL 收益 c_i / (p_i + 0.5) 最大者优先 +1，纯整数交叉乘法无浮点无除法）。
        let mut rem = size - used;
        while rem > 0 {
            let mut best = usize::MAX;
            let mut best_c = 0u64;
            let mut best_denom = 1u64;
            for i in 0..len {
                let p = probs[i];
                if p > 0 {
                    let c = cnt[i] as u64;
                    let denom = (p as u64) * 2 + 1;
                    if best == usize::MAX || c * best_denom > best_c * denom {
                        best = i;
                        best_c = c;
                        best_denom = denom;
                    }
                }
            }
            if best == usize::MAX {
                let mut big = 0usize;
                for i in 1..len {
                    if cnt[i] > cnt[big] {
                        big = i;
                    }
                }
                probs[big] = 1;
                continue;
            }
            probs[best] += 1;
            rem -= 1;
        }
    } else if used > size {
        // 边际 KL 损失 c_i / (p_i - 0.5) 最小者优先 -1。
        let mut excess = used - size;
        while excess > 0 {
            let mut best = usize::MAX;
            let mut best_c = 0u64;
            let mut best_denom = 1u64;
            for i in 0..len {
                let p = probs[i];
                if p >= 2 {
                    let c = cnt[i] as u64;
                    let denom = (p as u64) * 2 - 1;
                    if best == usize::MAX || c * best_denom < best_c * denom {
                        best = i;
                        best_c = c;
                        best_denom = denom;
                    }
                }
            }
            if best == usize::MAX {
                return None;
            }
            probs[best] -= 1;
            excess -= 1;
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

/// 写 FSE 分布表头（支持 `-1` 低概率符号；到最后一个非零符号为止），直接写入 `out`。
pub(crate) fn write_dist_into(probs: &[i32], al: u32, out: &mut Vec<u8>) -> Option<()> {
    let last = probs.iter().rposition(|&p| p != 0)?;
    let mut bw = BitAcc::new();
    bw.put(out, al - 5, 4);
    let (mut remaining, mut thr, mut nbits) = ((1i32 << al) + 1, 1i32 << al, al + 1);
    let mut s = 0usize;
    while remaining > 1 && s <= last {
        let p = probs[s];
        let x = p + 1;
        let max = 2 * thr - 1 - remaining;
        if x < max {
            bw.put(out, x as u32, nbits - 1);
        } else if x >= thr {
            bw.put(out, (x + max) as u32, nbits);
        } else {
            bw.put(out, x as u32, nbits);
        }
        remaining -= p.abs();
        if p == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn kkt_rounded_normalization_matches_exact_sum_and_bounds() {
        let mut s = 0xCAFE_BABE_1234_5678u64;
        for al in 5..=9 {
            let size = 1i32 << al;
            for _ in 0..40 {
                let mut cnt = [0u32; 53];
                let active = ((s % 24) as usize + 2).min(size as usize - 1);
                for slot in cnt.iter_mut().take(active) {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    *slot = ((s >> 32) % 5000) as u32 + 1;
                }
                let p = normalize(&cnt, al).unwrap();
                let sum: i32 = p.iter().map(|&v| v.abs()).sum();
                assert_eq!(sum, size, "al={al}");
                for (&c, &v) in cnt.iter().zip(&p) {
                    if c > 0 {
                        assert_ne!(v, 0);
                    } else {
                        assert_eq!(v, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn kkt_normalization_extreme_distributions_and_fse_roundtrip() {
        let cases: Vec<(u32, Vec<u32>)> = vec![
            (5, vec![1]),
            (5, vec![u32::MAX]),
            (5, vec![u32::MAX, 1]),
            (5, vec![1, u32::MAX]),
            (5, {
                let mut v = vec![1u32; 31];
                v[0] = u32::MAX;
                v
            }),
            (5, {
                let mut v = vec![1u32; 32];
                v[0] = 100_000;
                v
            }),
            // 精确 -1 阈值边界：c * size * 2 == n - 1 vs n
            (5, vec![1, 63]),
            (5, vec![1, 64]),
            (5, vec![1, 65]),
            (6, {
                let mut v = vec![1u32; 53];
                v[52] = 500_000;
                v
            }),
            (9, {
                let mut v = vec![1u32; 53];
                v[0] = u32::MAX / 2;
                v[1] = u32::MAX / 2;
                v
            }),
            (9, vec![u32::MAX; 53]),
            (9, vec![1u32; 512]),
        ];

        for (al, cnt) in cases {
            let p = normalize(&cnt, al).expect("valid distribution must normalize");
            let size = 1i32 << al;
            let sum: i32 = p.iter().map(|&v| v.abs()).sum();
            assert_eq!(sum, size, "al={al} len={}", cnt.len());
            for (&c, &v) in cnt.iter().zip(&p) {
                if c > 0 {
                    assert_ne!(v, 0);
                } else {
                    assert_eq!(v, 0);
                }
            }
            // 对所有 <= 256 符号的合法 FSE 分布做写表头 + 读表头 + 建表闭环验证
            if cnt.len() <= 256 {
                let mut hdr = Vec::new();
                write_dist_into(&p, al, &mut hdr).expect("write_dist_into failed");
                let mut decoded = [0i16; 256];
                let (dlen, dal, used) =
                    crate::fse::read_dist_into(&hdr, cnt.len() - 1, al, &mut decoded)
                        .expect("read_dist_into failed");
                assert_eq!(dal, al);
                assert_eq!(used, hdr.len());
                let last_nz = p.iter().rposition(|&v| v != 0).unwrap();
                assert_eq!(dlen, last_nz + 1);
                for i in 0..dlen {
                    assert_eq!(decoded[i] as i32, p[i], "al={al} sym={i}");
                }
                let mut cells = vec![crate::fse::Cell::default(); 1 << al];
                crate::fse::build_into(&decoded[..dlen], dal, &mut cells)
                    .expect("fse::build_into failed");
            }
        }

        // build_weights_into 遇到非法 al > 6 必须返回 Err 而非越界 panic
        let mut wcells = [crate::fse::Cell::default(); 64];
        assert!(crate::fse::build_weights_into(&[64, 64], 7, &mut wcells).is_err());
    }
}
