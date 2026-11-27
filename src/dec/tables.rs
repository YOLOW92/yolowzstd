//! Zstd 序列码基值表、预定义分布与解码表构造。
use std::sync::OnceLock;

use crate::fse;
use crate::{Error, Result};

pub(crate) const LL_BASE: [(u32, u32); 36] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 1),
    (18, 1),
    (20, 1),
    (22, 1),
    (24, 2),
    (28, 2),
    (32, 3),
    (40, 3),
    (48, 4),
    (64, 6),
    (128, 7),
    (256, 8),
    (512, 9),
    (1024, 10),
    (2048, 11),
    (4096, 12),
    (8192, 13),
    (16384, 14),
    (32768, 15),
    (65536, 16),
];

pub(crate) const ML_BASE: [(u32, u32); 53] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 0),
    (17, 0),
    (18, 0),
    (19, 0),
    (20, 0),
    (21, 0),
    (22, 0),
    (23, 0),
    (24, 0),
    (25, 0),
    (26, 0),
    (27, 0),
    (28, 0),
    (29, 0),
    (30, 0),
    (31, 0),
    (32, 0),
    (33, 0),
    (34, 0),
    (35, 1),
    (37, 1),
    (39, 1),
    (41, 1),
    (43, 2),
    (47, 2),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 5),
    (131, 7),
    (259, 8),
    (515, 9),
    (1027, 10),
    (2051, 11),
    (4099, 12),
    (8195, 13),
    (16387, 14),
    (32771, 15),
    (65539, 16),
];

pub(crate) const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

pub(crate) const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

pub(crate) const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// 序列码展开后的解码格（8 字节对齐，x86-64 可零移位直读字段）：状态转移（base、nb）加上额外位数和基值。
#[derive(Clone, Copy, Default)]
pub(super) struct SCell {
    pub base: u16,
    pub nb: u8,
    pub eb: u8,
    pub val: u32,
}

impl SCell {
    #[inline(always)]
    pub(super) const fn pack(val: u32, base: u16, nb: u8, eb: u8) -> Self {
        Self { base, nb, eb, val }
    }

    #[inline(always)]
    pub(super) const fn val(self) -> u32 {
        self.val
    }

    #[inline(always)]
    pub(super) const fn base(self) -> usize {
        self.base as usize
    }

    #[inline(always)]
    pub(super) const fn nb(self) -> u32 {
        self.nb as u32
    }

    #[inline(always)]
    pub(super) const fn eb(self) -> u32 {
        self.eb as u32
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TableSrc {
    None,
    Predef,
    Custom,
}

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Ll,
    Of,
    Ml,
}

pub(super) struct SeqTables {
    pub ll: [SCell; 512],
    pub of: [SCell; 256],
    pub ml: [SCell; 512],
}

impl SeqTables {
    pub(super) fn new() -> Self {
        Self {
            ll: [SCell::default(); 512],
            of: [SCell::default(); 256],
            ml: [SCell::default(); 512],
        }
    }
}

pub(super) struct Predefs {
    pub ll: [SCell; 64],
    pub of: [SCell; 32],
    pub ml: [SCell; 64],
}

pub(super) fn predefs() -> &'static Predefs {
    static PREDEF: OnceLock<Predefs> = OnceLock::new();
    PREDEF.get_or_init(|| {
        let mut f_ll = [fse::Cell::default(); 64];
        let mut f_of = [fse::Cell::default(); 32];
        let mut f_ml = [fse::Cell::default(); 64];
        fse::build_into(&LL_DEFAULT, 6, &mut f_ll).expect("预定义表合法");
        fse::build_into(&OF_DEFAULT, 5, &mut f_of).expect("预定义表合法");
        fse::build_into(&ML_DEFAULT, 6, &mut f_ml).expect("预定义表合法");
        let mut ll = [SCell::default(); 64];
        let mut of = [SCell::default(); 32];
        let mut ml = [SCell::default(); 64];
        expand_cells(&f_ll, Kind::Ll, &mut ll);
        expand_cells(&f_of, Kind::Of, &mut of);
        expand_cells(&f_ml, Kind::Ml, &mut ml);
        Predefs { ll, of, ml }
    })
}

fn expand_cells(src: &[fse::Cell], kind: Kind, dst: &mut [SCell]) {
    for (d, c) in dst.iter_mut().zip(src.iter()) {
        let sym = c.sym as usize;
        let (val, eb) = match kind {
            Kind::Ll => LL_BASE[sym.min(LL_BASE.len() - 1)],
            Kind::Ml => ML_BASE[sym.min(ML_BASE.len() - 1)],
            Kind::Of => (1u32 << sym.min(31), sym.min(31) as u32),
        };
        *d = SCell::pack(val, c.base, c.nb, eb as u8);
    }
}

fn build_seq_cells_into(probs: &[i16], al: u32, kind: Kind, out: &mut [SCell]) -> Result<()> {
    let size = 1usize << al;
    let cells = &mut out[..size];
    let mut syms = [0u8; 512];
    let mut high = size;
    let mut next = [0u32; 64];
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
    for (d, &sym_u8) in cells.iter_mut().zip(&syms[..size]) {
        let sym = sym_u8 as usize;
        let x = next[sym];
        next[sym] = x + 1;
        let nb = al - (31 - x.leading_zeros());
        let base = ((x << nb) - size as u32) as u16;
        let (val, eb) = match kind {
            Kind::Ll => LL_BASE[sym.min(LL_BASE.len() - 1)],
            Kind::Ml => ML_BASE[sym.min(ML_BASE.len() - 1)],
            Kind::Of => (1u32 << sym.min(31), sym.min(31) as u32),
        };
        *d = SCell::pack(val, base, nb as u8, eb as u8);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_seq_table(
    mode: u8,
    data: &[u8],
    pos: &mut usize,
    src: &mut TableSrc,
    predef_loaded: &mut bool,
    al: &mut u32,
    cells: &mut [SCell],
    predef_cells: &[SCell],
    spec: (usize, u32, u32),
    kind: Kind,
) -> Result<()> {
    let (max_sym, max_al, predef_al) = spec;
    match mode {
        0 => {
            if !*predef_loaded {
                cells[..predef_cells.len()].copy_from_slice(predef_cells);
                *predef_loaded = true;
            }
            *src = TableSrc::Predef;
            *al = predef_al;
        }
        1 => {
            let s = *data.get(*pos).ok_or(Error::Corrupt("rle symbol"))?;
            if s as usize > max_sym {
                return Err(Error::Corrupt("rle symbol out of range"));
            }
            *pos += 1;
            let sym = s as usize;
            let (val, eb) = match kind {
                Kind::Ll => LL_BASE[sym.min(LL_BASE.len() - 1)],
                Kind::Ml => ML_BASE[sym.min(ML_BASE.len() - 1)],
                Kind::Of => (1u32 << sym.min(31), sym.min(31) as u32),
            };
            cells[0] = SCell::pack(val, 0, 0, eb as u8);
            *predef_loaded = false;
            *src = TableSrc::Custom;
            *al = 0;
        }
        2 => {
            let tail = data.get(*pos..).unwrap_or(&[]);
            let mut probs = [0i16; 64];
            let (plen, table_al, used) = fse::read_dist_into(tail, max_sym, max_al, &mut probs)?;
            build_seq_cells_into(&probs[..plen], table_al, kind, cells)?;
            *pos += used;
            *predef_loaded = false;
            *src = TableSrc::Custom;
            *al = table_al;
        }
        _ => {
            if *src == TableSrc::None {
                return Err(Error::Corrupt("repeat mode without table"));
            }
        }
    }
    Ok(())
}
