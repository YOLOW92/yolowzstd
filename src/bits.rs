//! 位读取器：正向（FSE 表头）和反向（块内数据，从末尾哨兵位往前读）。
use crate::{Error, Result};

/// 取 `data` 里从第 `start` 位起的 `n` 位（n ≤ 32），越界处补 0。位序：每字节低位在先。
#[inline(always)]
fn get_bits(data: &[u8], start: usize, n: u32) -> u64 {
    let byte = start >> 3;
    let v = if let Some(s) = data.get(byte..byte + 8) {
        u64::from_le_bytes(s.try_into().unwrap())
    } else {
        let mut acc = 0u64;
        for (i, &b) in data.get(byte..).unwrap_or(&[]).iter().take(8).enumerate() {
            acc |= (b as u64) << (8 * i);
        }
        acc
    };
    (v >> (start & 7)) & 1u64.wrapping_shl(n).wrapping_sub(1)
}

pub(crate) struct FwdBits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> FwdBits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn peek(&self, n: u32) -> u32 {
        get_bits(self.data, self.pos, n) as u32
    }
    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }
    pub fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }
    /// 已消耗的字节数（向上取整）；读过头则报错。
    pub fn bytes_used(&self) -> Result<usize> {
        let b = self.pos.div_ceil(8);
        if b > self.data.len() {
            return Err(Error::Corrupt("fse header truncated"));
        }
        Ok(b)
    }
}

/// 反向位读取器:从末尾哨兵位往前读,64 位缓冲高位对齐,读空后补 0 并记为越界。
pub(crate) struct RevBits<'a> {
    data: &'a [u8],
    /// 还没装进缓冲的字节数(data[..idx])。
    idx: usize,
    buf: u64,
    have: u32,
    /// 剩余有效位数;读过头会变负。
    rem: i64,
}

impl<'a> RevBits<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        let last = *data.last().ok_or(Error::Corrupt("empty bitstream"))?;
        if last == 0 {
            return Err(Error::Corrupt("bitstream without end mark"));
        }
        let lz = last.leading_zeros();
        let have = 7 - lz;
        let idx = data.len() - 1;
        Ok(Self {
            data,
            idx,
            buf: ((last as u64) << 56) << (lz + 1),
            have,
            rem: have as i64 + 8 * idx as i64,
        })
    }
    #[inline(always)]
    fn refill(&mut self) {
        if self.have < 32 {
            if self.idx >= 4 {
                let b = &self.data[self.idx - 4..self.idx];
                let w = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                self.buf |= (w as u64) << (32 - self.have);
                self.have += 32;
                self.idx -= 4;
            } else {
                while self.have <= 56 && self.idx > 0 {
                    self.idx -= 1;
                    self.buf |= (self.data[self.idx] as u64) << (56 - self.have);
                    self.have += 8;
                }
            }
        }
    }
    /// 读 `n` 位(n ≤ 32);越过开头的部分补 0 并让 `overflow()` 为真。
    /// 补到至少 56 位(数据够时)。
    #[inline(always)]
    pub fn refill_wide(&mut self) {
        if self.have < 56 {
            if self.idx >= 8 {
                let b = &self.data[self.idx - 8..self.idx];
                let w = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                let k = (63 - self.have) >> 3; // 1..=7 字节
                self.buf |= (w >> (64 - 8 * k)) << (64 - self.have - 8 * k);
                self.have += 8 * k;
                self.idx -= k as usize;
            } else {
                while self.have <= 56 && self.idx > 0 {
                    self.idx -= 1;
                    self.buf |= (self.data[self.idx] as u64) << (56 - self.have);
                    self.have += 8;
                }
            }
        }
    }
    /// 一次读 `n` 位(n ≤ 56),先读到的在高位。调用方保证刚 `refill_wide` 过。
    #[inline(always)]
    pub fn read_wide(&mut self, n: u32) -> u64 {
        let v = (self.buf >> 8) >> (56 - n);
        self.skip(n);
        v
    }
    #[inline(always)]
    pub fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }
    #[inline(always)]
    pub fn peek(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.refill();
        (self.buf >> (64 - n)) as u32
    }
    #[inline(always)]
    pub fn skip(&mut self, n: u32) {
        self.buf <<= n;
        self.have = self.have.saturating_sub(n);
        self.rem -= n as i64;
    }
    pub fn overflow(&self) -> bool {
        self.rem < 0
    }
    pub fn remaining(&self) -> i64 {
        self.rem
    }
}

/// 反向位流快速游标：`data[..cur]` 为未消费区，其中最高字节 `data[cur - 1]` 已消费顶部 `bit_off`（0..8）位。
/// 在 `cur >= 8` 时可零分支读取至少 57 位并前进。
#[derive(Clone, Copy)]
pub(crate) struct RevCursor {
    pub cur: usize,
    pub bit_off: u32,
}

impl RevCursor {
    #[inline(always)]
    pub fn new(data: &[u8]) -> Result<Self> {
        let last = *data.last().ok_or(Error::Corrupt("empty bitstream"))?;
        if last == 0 {
            return Err(Error::Corrupt("bitstream without end mark"));
        }
        let lz = last.leading_zeros();
        if lz == 7 {
            Ok(Self {
                cur: data.len() - 1,
                bit_off: 0,
            })
        } else {
            Ok(Self {
                cur: data.len(),
                bit_off: lz + 1,
            })
        }
    }

    /// 要求 `self.cur >= 8`：返回高位对齐的 64 位窗，至少前 57 位有效。
    #[inline(always)]
    pub fn load_msb(self, data: &[u8]) -> u64 {
        let b: [u8; 8] = data[self.cur - 8..self.cur].try_into().unwrap();
        u64::from_le_bytes(b) << self.bit_off
    }

    /// 要求 `self.cur >= 8` 且 `bits <= 56`：推进 `bits` 位，保证不越界。
    #[inline(always)]
    pub fn advance(&mut self, bits: u32) {
        let total = self.bit_off + bits;
        self.cur -= (total >> 3) as usize;
        self.bit_off = total & 7;
    }

    #[inline(always)]
    pub fn into_rev(self, data: &[u8]) -> RevBits<'_> {
        if self.bit_off == 0 {
            RevBits {
                data,
                idx: self.cur,
                buf: 0,
                have: 0,
                rem: 8 * self.cur as i64,
            }
        } else {
            let idx = self.cur - 1;
            let have = 8 - self.bit_off;
            let b = data[idx];
            RevBits {
                data,
                idx,
                buf: ((b as u64) << 56) << self.bit_off,
                have,
                rem: have as i64 + 8 * idx as i64,
            }
        }
    }
}
