//! 零依赖多核并行分帧压缩与多帧并行解压调度。
//!
//! - 编码端：将大输入切分为对齐块大小的独立切片，并发压缩为合法的串联 Zstd 帧；
//!   通过每核复用 [`EncCore`] 与预分配槽位避免重复页错误与锁竞争，自动按 L2/物理核/逻辑核扩展。
//! - 解码端：先按块头线性扫描出帧边界（零解压开销），当存在多帧且输出受 `max_out` 约束时，
//!   跨多核并发解压各帧再按序拼装，保持单帧/异常路径与单线程语义完全一致。
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::dec::{DecompressOptions, State, decompress_with_state, parse_frame_header};
use crate::enc::{BLOCK, CompressOptions, EncCore};
use crate::resources::{resolve_dec_threads, resolve_enc_threads};
use crate::{Error, Result};

const MAX_RETAINED_SLOTS: usize = 32;
const MAX_RETAINED_SLOT_CAP: usize = 4 << 20;
const MAX_RETAINED_SLOTS_BYTES: usize = 32 << 20;

#[inline]
fn retain_slots(mt_slots: &mut Vec<Vec<u8>>, buf: Vec<u8>, retained_bytes: &mut usize) {
    let cap = buf.capacity();
    if mt_slots.len() < MAX_RETAINED_SLOTS
        && cap <= MAX_RETAINED_SLOT_CAP
        && retained_bytes.saturating_add(cap) <= MAX_RETAINED_SLOTS_BYTES
    {
        *retained_bytes += cap;
        let mut b = buf;
        b.clear();
        mt_slots.push(b);
    }
}

#[inline]
fn tick_idle_vec<T>(workers: &mut Vec<T>, slots: &mut Vec<Vec<u8>>, mt_idle: &mut u8) {
    if !workers.is_empty() || !slots.is_empty() {
        *mt_idle = mt_idle.saturating_add(1);
        if *mt_idle >= 8 {
            workers.clear();
            workers.shrink_to_fit();
            slots.clear();
            slots.shrink_to_fit();
            *mt_idle = 0;
        }
    }
}

struct FrameSpan {
    start: usize,
    end: usize,
    uncomp_len: Option<u64>,
}

fn enc_chunk_ranges(len: usize, level: i32, workers: usize) -> Option<Vec<(usize, usize)>> {
    if workers <= 1 {
        return None;
    }
    let (min_total, min_chunk, max_chunk) = if level <= 0 {
        (4 << 20, 2 << 20, 4 << 20)
    } else if level >= 16 {
        (512 << 10, 256 << 10, 1 << 20)
    } else if level >= 5 {
        (768 << 10, 384 << 10, 2 << 20)
    } else {
        (1 << 20, 512 << 10, 2 << 20)
    };
    if len < min_total {
        return None;
    }
    let target_chunks = workers.saturating_mul(3).max(2);
    let raw = len.div_ceil(target_chunks);
    let aligned = raw.div_ceil(BLOCK).saturating_mul(BLOCK);
    let chunk = aligned.clamp(min_chunk, max_chunk);
    let mut ranges = Vec::with_capacity(len.div_ceil(chunk));
    let mut pos = 0usize;
    while pos < len {
        let mut end = (pos + chunk).min(len);
        if len - end < BLOCK {
            end = len;
        }
        ranges.push((pos, end));
        pos = end;
    }
    if ranges.len() < 2 {
        return None;
    }
    Some(ranges)
}

pub(crate) fn compress_parallel(
    core: &mut EncCore,
    mt_workers: &mut Vec<EncCore>,
    mt_slots: &mut Vec<Vec<u8>>,
    mt_idle: &mut u8,
    data: &[u8],
    out: &mut Vec<u8>,
    opts: &CompressOptions,
) {
    let workers = resolve_enc_threads(opts.threads, opts.level);
    let Some(ranges) = enc_chunk_ranges(data.len(), opts.level, workers) else {
        tick_idle_vec(mt_workers, mt_slots, mt_idle);
        core.compress_single_frame(data, out, opts);
        return;
    };
    let nchunks = ranges.len();
    let active = workers.min(nchunks);
    if active <= 1 {
        tick_idle_vec(mt_workers, mt_slots, mt_idle);
        core.compress_single_frame(data, out, opts);
        return;
    }
    *mt_idle = 0;

    if mt_workers.len() < active - 1 {
        mt_workers.resize_with(active - 1, EncCore::new);
    }
    let slots: Vec<Mutex<Vec<u8>>> = (0..nchunks)
        .map(|_| {
            let mut b = mt_slots.pop().unwrap_or_default();
            b.clear();
            Mutex::new(b)
        })
        .collect();

    let next = AtomicUsize::new(0);
    let mut chunk_opts = *opts;
    chunk_opts.threads = 1;
    let run_worker = |engine: &mut EncCore| {
        let mut tmp = Vec::new();
        loop {
            let idx = next.fetch_add(1, Ordering::Relaxed);
            if idx >= nchunks {
                break;
            }
            {
                let mut guard = slots[idx].lock().unwrap_or_else(|e| e.into_inner());
                std::mem::swap(&mut tmp, &mut *guard);
            }
            tmp.clear();
            let (a, b) = ranges[idx];
            engine.compress_single_frame(&data[a..b], &mut tmp, &chunk_opts);
            {
                let mut guard = slots[idx].lock().unwrap_or_else(|e| e.into_inner());
                std::mem::swap(&mut tmp, &mut *guard);
            }
        }
    };

    std::thread::scope(|s| {
        let workers_slice = &mut mt_workers[..active - 1];
        for worker in workers_slice.iter_mut() {
            s.spawn(|| run_worker(worker));
        }
        run_worker(core);
    });

    let total_comp: usize = slots
        .iter()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).len())
        .sum();
    out.reserve(total_comp);
    let mut retained_bytes: usize = mt_slots.iter().map(Vec::capacity).sum();
    for slot in slots {
        let buf = slot.into_inner().unwrap_or_else(|e| e.into_inner());
        out.extend_from_slice(&buf);
        retain_slots(mt_slots, buf, &mut retained_bytes);
    }
}

/// 仅扫描帧头与 3 字节块头定位所有帧边界，不做熵解码，耗时远小于微秒级/帧。
fn scan_frames(data: &[u8]) -> Result<Vec<FrameSpan>> {
    if data.is_empty() {
        return Err(Error::Corrupt("empty input"));
    }
    let mut spans = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let frame_start = pos;
        let magic = data
            .get(pos..pos + 4)
            .ok_or(Error::Corrupt("truncated magic"))?;
        let m = u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]);
        pos += 4;
        if (0x184D2A50..=0x184D2A5F).contains(&m) {
            let sz = data
                .get(pos..pos + 4)
                .ok_or(Error::Corrupt("truncated skip frame"))?;
            let n = u32::from_le_bytes([sz[0], sz[1], sz[2], sz[3]]) as usize;
            pos = pos
                .checked_add(4 + n)
                .filter(|&p| p <= data.len())
                .ok_or(Error::Corrupt("skip frame size"))?;
            continue;
        }
        if m != 0xFD2FB528 {
            return Err(Error::Corrupt("bad magic"));
        }
        let d = &data[pos..];
        let fh = parse_frame_header(d)?;
        let block_max = fh.block_max();
        let mut p = fh.header_len;
        loop {
            let h = d
                .get(p..p + 3)
                .ok_or(Error::Corrupt("truncated block header"))?;
            let v = h[0] as usize | (h[1] as usize) << 8 | (h[2] as usize) << 16;
            p += 3;
            let last = v & 1 != 0;
            let size = v >> 3;
            match (v >> 1) & 3 {
                0 | 2 => {
                    if size > block_max {
                        return Err(Error::Corrupt("block too large"));
                    }
                    p = p
                        .checked_add(size)
                        .filter(|&np| np <= d.len())
                        .ok_or(Error::Corrupt("truncated block"))?;
                }
                1 => {
                    if size > block_max {
                        return Err(Error::Corrupt("rle block too large"));
                    }
                    p = p
                        .checked_add(1)
                        .filter(|&np| np <= d.len())
                        .ok_or(Error::Corrupt("truncated rle block"))?;
                }
                _ => return Err(Error::Corrupt("reserved block type")),
            }
            if last {
                break;
            }
        }
        if fh.checksum {
            p = p
                .checked_add(4)
                .filter(|&np| np <= d.len())
                .ok_or(Error::Corrupt("truncated checksum"))?;
        }
        pos += p;
        spans.push(FrameSpan {
            start: frame_start,
            end: pos,
            uncomp_len: fh.uncomp_len,
        });
    }
    Ok(spans)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn decompress_parallel(
    state: &mut State,
    mt_states: &mut Vec<Box<State>>,
    mt_slots: &mut Vec<Vec<u8>>,
    mt_idle: &mut u8,
    data: &[u8],
    out: &mut Vec<u8>,
    opts: &DecompressOptions,
    max_out: usize,
) -> Result<()> {
    let workers = resolve_dec_threads(opts.threads);
    if workers <= 1 {
        tick_idle_vec(mt_states, mt_slots, mt_idle);
        return decompress_with_state(state, data, out, max_out);
    }
    let Ok(spans) = scan_frames(data) else {
        // 出错回退给单线程以保持前缀已解码块及精确错误码。
        tick_idle_vec(mt_states, mt_slots, mt_idle);
        return decompress_with_state(state, data, out, max_out);
    };
    if spans.len() < 2 {
        tick_idle_vec(mt_states, mt_slots, mt_idle);
        return decompress_with_state(state, data, out, max_out);
    }

    let mut known_total = Some(0usize);
    for sp in &spans {
        known_total = match (known_total, sp.uncomp_len) {
            (Some(acc), Some(v)) => usize::try_from(v).ok().and_then(|u| acc.checked_add(u)),
            _ => None,
        };
    }
    let avail = max_out.saturating_sub(out.len());
    let Some(total) = known_total else {
        // 含未声明 Frame_Content_Size 的帧时无法提前分配全局预算，直接回退单线程以确保 <= max_out。
        tick_idle_vec(mt_states, mt_slots, mt_idle);
        return decompress_with_state(state, data, out, max_out);
    };
    if total > avail || total < 256 * 1024 {
        tick_idle_vec(mt_states, mt_slots, mt_idle);
        return decompress_with_state(state, data, out, max_out);
    }
    *mt_idle = 0;

    let nframes = spans.len();
    let active = workers.min(nframes);
    if mt_states.len() < active - 1 {
        mt_states.resize_with(active - 1, || Box::new(State::new()));
    }

    let slots: Vec<Mutex<(Vec<u8>, Result<()>)>> = (0..nframes)
        .map(|_| {
            let mut b = mt_slots.pop().unwrap_or_default();
            b.clear();
            Mutex::new((b, Ok(())))
        })
        .collect();
    let next = AtomicUsize::new(0);
    let run_worker = |st: &mut State| {
        let mut tmp = Vec::new();
        loop {
            let idx = next.fetch_add(1, Ordering::Relaxed);
            if idx >= nframes {
                break;
            }
            {
                let mut guard = slots[idx].lock().unwrap_or_else(|e| e.into_inner());
                std::mem::swap(&mut tmp, &mut guard.0);
            }
            tmp.clear();
            let sp = &spans[idx];
            let frame_cap = sp.uncomp_len.expect("checked known") as usize;
            let res = decompress_with_state(st, &data[sp.start..sp.end], &mut tmp, frame_cap);
            {
                let mut guard = slots[idx].lock().unwrap_or_else(|e| e.into_inner());
                std::mem::swap(&mut tmp, &mut guard.0);
                guard.1 = res;
            }
        }
    };

    std::thread::scope(|s| {
        let states_slice = &mut mt_states[..active - 1];
        for worker_st in states_slice.iter_mut() {
            s.spawn(|| run_worker(worker_st));
        }
        run_worker(state);
    });

    let mut total_out = 0usize;
    let mut ok = true;
    for slot in &slots {
        let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if guard.1.is_err() {
            ok = false;
            break;
        }
        if let Some(next_total) = total_out.checked_add(guard.0.len()) {
            total_out = next_total;
        } else {
            ok = false;
            break;
        }
    }
    if !ok || out.len().saturating_add(total_out) > max_out {
        // 异常或超限路径不保留临时槽位缓冲，避免高水位内存滞留。
        drop(slots);
        return decompress_with_state(state, data, out, max_out);
    }

    out.reserve(total_out);
    let mut retained_bytes: usize = mt_slots.iter().map(Vec::capacity).sum();
    for slot in slots {
        let (buf, _) = slot.into_inner().unwrap_or_else(|e| e.into_inner());
        out.extend_from_slice(&buf);
        retain_slots(mt_slots, buf, &mut retained_bytes);
    }
    Ok(())
}
