//! FSE 表：从表头解出分布、建解码表。
use crate::bits::FwdBits;
use crate::{Error, Result};

#[derive(Clone, Copy, Default)]
pub(crate) struct Cell {
    pub sym: u8,
    pub nb: u8,
    pub base: u16,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct FseTable {
    pub cells: Vec<Cell>,
}

/// 读表头到调用方栈缓冲，返回（符号数，精度位数，消耗字节）。零堆分配。
pub(crate) fn read_dist_into(
    data: &[u8],
    max_sym: usize,
    max_al: u32,
    probs: &mut [i16],
) -> Result<(usize, u32, usize)> {
    let max_sym = max_sym.min(probs.len().saturating_sub(1));
    let mut r = FwdBits::new(data);
    let al = r.read(4) + 5;
    if al > max_al {
        return Err(Error::Corrupt("fse accuracy too large"));
    }
    let mut remaining: i32 = (1 << al) + 1;
    let mut threshold: i32 = 1 << al;
    let mut nbits = al + 1;
    let mut len = 0usize;
    while remaining > 1 {
        if len > max_sym {
            return Err(Error::Corrupt("fse too many symbols"));
        }
        let max = 2 * threshold - 1 - remaining;
        let v = r.peek(nbits) as i32;
        let mut count = v & (threshold - 1);
        if count < max {
            r.skip(nbits - 1);
        } else {
            count = v & (2 * threshold - 1);
            if count >= threshold {
                count -= max;
            }
            r.skip(nbits);
        }
        count -= 1;
        remaining -= count.abs();
        if remaining < 1 {
            return Err(Error::Corrupt("fse distribution overflow"));
        }
        probs[len] = count as i16;
        len += 1;
        if count == 0 {
            loop {
                let rep = r.read(2) as usize;
                if len + rep > max_sym + 1 {
                    return Err(Error::Corrupt("fse zero run too long"));
                }
                probs[len..len + rep].fill(0);
                len += rep;
                if rep != 3 {
                    break;
                }
            }
        }
        while remaining < threshold {
            nbits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(Error::Corrupt("fse distribution sum"));
    }
    let used = r.bytes_used()?;
    Ok((len, al, used))
}

pub(crate) fn build_into(probs: &[i16], al: u32, out: &mut [Cell]) -> Result<()> {
    if al > 9 {
        return Err(Error::Corrupt("fse accuracy too large"));
    }
    let size = 1usize << al;
    if size > out.len() {
        return Err(Error::Corrupt("fse table too large"));
    }
    let cells = &mut out[..size];
    let mut high = size;
    let mut next = [0u32; 256];
    let mut syms = [0u8; 512];
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
    for (c, &sym_u8) in cells.iter_mut().zip(&syms[..size]) {
        let sym = sym_u8 as usize;
        let x = next[sym];
        next[sym] = x + 1;
        let nb = al - (31 - x.leading_zeros());
        *c = Cell {
            sym: sym_u8,
            nb: nb as u8,
            base: ((x << nb) - size as u32) as u16,
        };
    }
    Ok(())
}

/// 专为 Huffman 权重表（符号 0..=15，表长 ≤ 64）特化的零冗余栈建表。
pub(crate) fn build_weights_into(probs: &[i16], al: u32, out: &mut [Cell; 64]) -> Result<()> {
    if al > 6 {
        return Err(Error::Corrupt("fse accuracy too large"));
    }
    let size = 1usize << al;
    if probs.len() > 16 {
        return build_into(probs, al, out);
    }
    let cells = &mut out[..size];
    let mut high = size;
    let mut next = [0u32; 16];
    let mut syms = [0u8; 64];
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
    for (c, &sym_u8) in cells.iter_mut().zip(&syms[..size]) {
        let sym = sym_u8 as usize;
        let x = next[sym];
        next[sym] = x + 1;
        let nb = al - (31 - x.leading_zeros());
        *c = Cell {
            sym: sym_u8,
            nb: nb as u8,
            base: ((x << nb) - size as u32) as u16,
        };
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn build(probs: &[i16], al: u32) -> Result<FseTable> {
    let size = 1usize << al;
    let mut cells = vec![Cell::default(); size];
    build_into(probs, al, &mut cells)?;
    Ok(FseTable { cells })
}

/// 读表头并建表。返回（表，消耗字节）。
#[cfg(test)]
pub(crate) fn read_table(data: &[u8], max_sym: usize, max_al: u32) -> Result<(FseTable, usize)> {
    let mut probs = [0i16; 256];
    let (len, al, used) = read_dist_into(data, max_sym, max_al, &mut probs)?;
    Ok((build(&probs[..len], al)?, used))
}
