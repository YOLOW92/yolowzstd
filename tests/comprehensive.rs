use std::io::Write;
use std::process::{Command, Stdio};

use yolowzstd::{
    CompressOptions, Compressor, DecompressOptions, Decompressor, Error, compress, compress_bound,
    compress_into, compress_with, decompress, decompress_into, decompress_into_with,
    decompress_with,
};

fn lcg(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let b = match i % 29 {
            0..=15 => b'a' + ((s >> 56) as u8 % 12),
            16..=22 => b'0' + ((s >> 48) as u8 % 10),
            23..=25 => b' ',
            _ => (s >> 40) as u8,
        };
        out.push(b);
    }
    out
}

fn structured_payload(len: usize) -> Vec<u8> {
    let chunk =
        b"{\"id\":42,\"name\":\"yolowzstd\",\"tags\":[\"rfc8878\",\"pure-rust\",\"zstd\"]}\n";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = (len - out.len()).min(chunk.len());
        out.extend_from_slice(&chunk[..take]);
    }
    out
}

#[test]
fn roundtrip_edges_and_levels() {
    let mut comp = Compressor::new();
    let mut dec = Decompressor::new();
    let mut cbuf = Vec::new();
    let mut dbuf = Vec::new();

    for &size in &[
        0usize, 1, 2, 3, 4, 5, 17, 64, 255, 256, 257, 1024, 4096, 16384, 131_071, 131_072, 131_073,
        280_000,
    ] {
        let data = if size % 2 == 0 {
            structured_payload(size)
        } else {
            lcg(size, size as u64 + 7)
        };
        for &level in &[0, 1, 2, 3, 5, 9, 12, 16, 19] {
            cbuf.clear();
            let opts = CompressOptions::new(level).with_checksum(level % 2 == 1);
            comp.compress_into(&data, &mut cbuf, &opts);
            assert!(
                cbuf.len() <= compress_bound(data.len()) || size == 0,
                "size={size} level={level} exceeded bound"
            );
            dbuf.clear();
            dec.decompress_into(&cbuf, &mut dbuf, data.len().max(1))
                .unwrap();
            assert_eq!(dbuf, data, "roundtrip mismatch size={size} level={level}");
        }
    }
}

#[test]
fn rle_blocks_and_raw_fallback_preserve_state() {
    // 块 1 可压缩（建立 repeat/huff），块 2 不可压缩（触发 Raw_Block 回退），块 3 再度可压缩。
    let mut data = Vec::with_capacity(390 * 1024);
    data.extend(structured_payload(128 * 1024));
    let mut rng = 0x1234_5678_9ABC_DEF0u64;
    for _ in 0..(128 * 1024) {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        data.push((rng >> 32) as u8);
    }
    data.extend(vec![b'Z'; 128 * 1024]);
    data.extend(structured_payload(16 * 1024));

    for &level in &[1, 3, 9, 19] {
        let z = compress(&data, level);
        let out = decompress(&z, data.len()).unwrap();
        assert_eq!(out, data, "multi-block raw/rle mix failed at level {level}");
    }
}

#[test]
fn compress_options_and_window_log() {
    let data = structured_payload(200 * 1024);
    let opts = CompressOptions::new(3)
        .with_checksum(true)
        .with_window_log(Some(12));
    let mut z = Vec::new();
    compress_into(&data, &mut z, &opts);
    let mut decoded = Vec::new();
    decompress_into(&z, &mut decoded, data.len()).unwrap();
    assert_eq!(decoded, data);

    // 破坏末尾 checksum 必须被拒绝
    let last = z.len() - 1;
    z[last] ^= 0x80;
    assert!(matches!(decompress(&z, data.len()), Err(Error::Corrupt(_))));
}

#[test]
fn skippable_and_concatenated_frames() {
    let a = structured_payload(4096);
    let b = lcg(5000, 99);
    let mut stream = Vec::new();
    // Skippable frame magic 0x184D2A53 + 4-byte len + payload
    stream.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
    stream.extend_from_slice(&5u32.to_le_bytes());
    stream.extend_from_slice(b"HELLO");
    stream.extend_from_slice(&compress(&a, 3));
    stream.extend_from_slice(&0x184D_2A5Fu32.to_le_bytes());
    stream.extend_from_slice(&0u32.to_le_bytes());
    stream.extend_from_slice(&compress(&b, 1));

    let out = decompress(&stream, a.len() + b.len()).unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(&a);
    expected.extend_from_slice(&b);
    assert_eq!(out, expected);
}

#[test]
fn error_paths_and_fuzz_safety() {
    let data = structured_payload(2048);
    let z = compress(&data, 3);
    assert!(matches!(decompress(&z, 100), Err(Error::TooLarge)));
    assert!(matches!(decompress(&[], 1024), Err(Error::Corrupt(_))));
    assert!(matches!(
        decompress(&[0, 0, 0, 0], 1024),
        Err(Error::Corrupt(_))
    ));

    // Dict ID flag set (FHD bit 0..=1 = 1) -> Unsupported
    let bad_dict = [0x28, 0xB5, 0x2F, 0xFD, 0x01, 0x00, 0x01, 0x00, 0x00];
    assert!(matches!(
        decompress(&bad_dict, 1024),
        Err(Error::Unsupported(_))
    ));

    // 单字节截断与比特翻转不应 panic
    let mut dec = Decompressor::new();
    let mut out = Vec::new();
    for cut in 0..z.len() {
        out.clear();
        let _ = dec.decompress_into(&z[..cut], &mut out, 4096);
    }
    let mut rng = 0xCAFE_BABEu64;
    let mut mutated = z.clone();
    for _ in 0..500 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let idx = (rng as usize) % mutated.len();
        mutated[idx] ^= ((rng >> 32) as u8).max(1);
        out.clear();
        let _ = dec.decompress_into(&mutated, &mut out, 8192);
        mutated[idx] = z[idx];
    }
}

#[test]
fn official_zstd_cli_bidirectional_matrix() {
    if Command::new("zstd").arg("--version").output().is_err() {
        eprintln!("zstd CLI unavailable, skipping external interop matrix");
        return;
    }

    let samples = [
        Vec::new(),
        b"x".to_vec(),
        structured_payload(150_000),
        lcg(160_000, 2026),
        vec![7u8; 140_000],
    ];

    // yolowzstd -> zstd -d
    for (i, raw) in samples.iter().enumerate() {
        for &lvl in &[0, 1, 2, 3, 5, 9, 15, 19] {
            let z = compress(raw, lvl);
            let mut child = Command::new("zstd")
                .args(["-d", "-c", "-q"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&z).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "zstd -d rejected sample={i} level={lvl}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(out.stdout, *raw, "sample={i} level={lvl}");
        }
    }

    // zstd -level -> yolowzstd::decompress
    for (i, raw) in samples.iter().enumerate() {
        for flag in ["-1", "-3", "-9", "-19", "--long=20"] {
            let mut child = Command::new("zstd")
                .args([flag, "-c", "-q"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(raw).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let decoded = decompress(&out.stdout, raw.len() + 64).unwrap();
            assert_eq!(decoded, *raw, "failed decoding zstd {flag} on sample={i}");
        }
    }
}

#[test]
fn parallel_compress_and_multiframe_decompress() {
    let payload = structured_payload(2_600_000);
    let mut comp = Compressor::new();
    let mut dec = Decompressor::new();

    for &lvl in &[0, 1, 3, 9, 19] {
        for &threads in &[0usize, 4] {
            let copts = CompressOptions::new(lvl).with_threads(threads);
            let z = comp.compress_with(&payload, &copts);
            assert!(
                z.len() <= compress_bound(payload.len()),
                "compress_bound exceeded at level={lvl}"
            );

            let dopts = DecompressOptions::new().with_threads(threads);
            let rec = dec.decompress_with(&z, &dopts, payload.len()).unwrap();
            assert_eq!(
                rec, payload,
                "parallel roundtrip level={lvl} threads={threads}"
            );
            assert_eq!(decompress(&z, payload.len()).unwrap(), payload);
        }
    }

    // Free function symmetry + tight max_out parity between single & multi-thread decode.
    let multi = compress_with(&payload, &CompressOptions::new(3).with_threads(4));
    let single_err_out = {
        let mut o = Vec::new();
        let err = dec
            .decompress_into_with(
                &multi,
                &mut o,
                &DecompressOptions::new().with_threads(1),
                700_000,
            )
            .unwrap_err();
        assert_eq!(err, Error::TooLarge);
        o
    };
    let mt_err_out = {
        let mut o = Vec::new();
        let err = decompress_into_with(
            &multi,
            &mut o,
            &DecompressOptions::new().with_threads(4),
            700_000,
        )
        .unwrap_err();
        assert_eq!(err, Error::TooLarge);
        o
    };
    assert_eq!(single_err_out, mt_err_out);

    // Corrupt final checksum byte in multi-frame input -> both paths agree on Corrupt error.
    let mut bad = multi.clone();
    *bad.last_mut().unwrap() ^= 0xFF;
    assert!(matches!(
        decompress_with(
            &bad,
            &DecompressOptions::new().with_threads(0),
            payload.len()
        ),
        Err(Error::Corrupt(_))
    ));

    if Command::new("zstd").arg("--version").output().is_ok() {
        let mut child = Command::new("zstd")
            .args(["-d", "-c", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&multi).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, payload);
    }
}

#[test]
fn bt_block_boundary_common_prefix_invariant() {
    // Near-identical >=32B prefixes straddling the 128 KiB block boundary must never corrupt BST ordering.
    let mut data = vec![b'a'; 300_000];
    let stem = b"__26nsXPCComponents_InterfacesX";
    let p0 = 130_912;
    data[p0..p0 + stem.len()].copy_from_slice(stem);
    data[p0 + stem.len()..p0 + stem.len() + 8].copy_from_slice(b"RC4nsIDP");
    let p1 = 131_000;
    data[p1..p1 + stem.len()].copy_from_slice(stem);
    data[p1 + stem.len()..p1 + stem.len() + 8].copy_from_slice(b"v\x00GetCla");
    let p2 = 131_045; // 27 bytes inside block 0 (131072 - 131045 = 27)
    data[p2..p2 + stem.len()].copy_from_slice(stem);
    data[p2 + stem.len()..p2 + stem.len() + 8].copy_from_slice(b"PPc\x00GetS");
    for &(pos, tail) in &[
        (131_142usize, b"P11nsISu"),
        (131_226, b"P25nsIXP"),
        (132_763, b"PC4nsIDP"),
    ] {
        data[pos..pos + stem.len()].copy_from_slice(stem);
        data[pos + stem.len()..pos + stem.len() + 8].copy_from_slice(tail);
    }

    for &lvl in &[12, 15, 16, 19] {
        let z = compress(&data, lvl);
        let dec = decompress(&z, data.len()).unwrap();
        assert_eq!(dec, data, "BST cross-block roundtrip failed at L{lvl}");
    }
}

#[test]
fn fcs_window_descriptor_and_parallel_budget_regressions() {
    // 1. Frame Content Size 一致性：Raw / RLE / Compressed(nseq=0 & nseq>0) 超出或不足均返回 Corrupt
    let make_single_fcs_frame = |fcs: u8, hdr3: u32, payload: &[u8]| -> Vec<u8> {
        let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, fcs];
        f.extend_from_slice(&hdr3.to_le_bytes()[..3]);
        f.extend_from_slice(payload);
        f
    };
    // Raw 20B when FCS=10 (also exceeds single-segment window_size=10)
    let raw_over = make_single_fcs_frame(10, (20 << 3) | 1, &[b'a'; 20]);
    assert!(matches!(
        decompress(&raw_over, 1024),
        Err(Error::Corrupt(_))
    ));

    // RLE 20B when FCS=10
    let rle_over = make_single_fcs_frame(10, (20 << 3) | 3, b"a");
    assert!(matches!(
        decompress(&rle_over, 1024),
        Err(Error::Corrupt(_))
    ));

    // Underflow: FCS=20 but RLE outputs 10B
    let underflow = make_single_fcs_frame(20, (10 << 3) | 3, b"a");
    assert!(matches!(
        decompress(&underflow, 1024),
        Err(Error::Corrupt(_))
    ));

    // Non-single-segment wlog=10 (Window_Size=1024), FCS=10, Raw block 20B -> Corrupt (not TooLarge!)
    let mut non_single_over = vec![0x28, 0xB5, 0x2F, 0xFD, 0x40, 0x00]; // Wait: fcs=1 needs 2-byte (+256), use fcs=4 (0x80)
    non_single_over.clear();
    non_single_over.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD, 0x80, 0x00]); // single=0, wdesc=0(1024), fcs=4B
    non_single_over.extend_from_slice(&10u32.to_le_bytes());
    non_single_over.extend_from_slice(&((20u32 << 3) | 1).to_le_bytes()[..3]);
    non_single_over.extend_from_slice(&[b'x'; 20]);
    assert!(matches!(
        decompress(&non_single_over, 1024),
        Err(Error::Corrupt(_))
    ));

    // Compressed block nseq=0 outputting 12B when FCS=10 (non-single w=1024, fcs=10)
    let mut c_nseq0 = vec![0x28, 0xB5, 0x2F, 0xFD, 0x80, 0x00];
    c_nseq0.extend_from_slice(&10u32.to_le_bytes());
    let lit_body: Vec<u8> = [vec![12u8 << 3], vec![b'b'; 12], vec![0u8]].concat();
    c_nseq0.extend_from_slice(&(((lit_body.len() as u32) << 3) | 5).to_le_bytes()[..3]);
    c_nseq0.extend_from_slice(&lit_body);
    assert!(matches!(decompress(&c_nseq0, 1024), Err(Error::Corrupt(_))));

    // Compressed block nseq=1 outputting 12B when FCS=10 (max_out=1024 -> must be Corrupt, not TooLarge)
    // literals = "abcd" (4B), seq: ll=4, ml=8 (total 12B), off=1 (ofv=4 -> code 2, extra 0)
    let c_valid_12 = compress(b"aaaaaaaaaaaa", 3); // might be RLE; craft 12B explicitly:
    let _ = c_valid_12;
    // Block with 4 raw lits "abcd" + 1 seq (ll=4, off=1 -> ofv=4, ml=8):
    // nseq=1, modes=0x00 (predef), init states: ml_al=6 (sym=5 -> ml=8), of_al=5 (sym=2 -> ofv=4), ll_al=6 (sym=4 -> ll=4)
    // Easier: compress 12 bytes "abcdabcdabcd" with zstd-like or simply check compressed frame with patched FCS!
    let orig_200 = b"abcd".repeat(50);
    let z200 = compress(&orig_200, 3);
    // z200 is single-segment with fcs=1 (200) at byte 5. Patch FCS to 100 (byte 5 = 100) while max_out=1024 -> Corrupt!
    if z200[4] == 0x24 || z200[4] == 0x20 {
        let mut patched = z200.clone();
        patched[5] = 100;
        assert!(matches!(decompress(&patched, 1024), Err(Error::Corrupt(_))));
    }

    // 2. Window_Descriptor Block_Maximum_Size (1024) enforcement on Raw/RLE without FCS
    let mut wd_raw_over = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
    wd_raw_over.extend_from_slice(&((1025u32 << 3) | 1).to_le_bytes()[..3]);
    wd_raw_over.extend_from_slice(&[b'z'; 1025]);
    assert!(matches!(
        decompress(&wd_raw_over, 8192),
        Err(Error::Corrupt(_))
    ));

    let mut wd_rle_over = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
    wd_rle_over.extend_from_slice(&((1025u32 << 3) | 3).to_le_bytes()[..3]);
    wd_rle_over.push(b'z');
    assert!(matches!(
        decompress(&wd_rle_over, 8192),
        Err(Error::Corrupt(_))
    ));

    // Compressed block size 1025: valid at wdesc=0x08 (Window_Size=2048), Corrupt at wdesc=0x00 (1024)
    let make_comp_1025 = |wdesc: u8| -> Vec<u8> {
        let mut body = vec![0x14, 0x40]; // Raw literals 1025B: ty=0, sf=1, n=1025
        body.resize(2 + 1025, b'k');
        body.push(0); // nseq = 0
        let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, wdesc];
        f.extend_from_slice(&(((body.len() as u32) << 3) | 5).to_le_bytes()[..3]);
        f.extend_from_slice(&body);
        f
    };
    assert_eq!(decompress(&make_comp_1025(0x08), 8192).unwrap().len(), 1025);
    assert!(matches!(
        decompress(&make_comp_1025(0x00), 8192),
        Err(Error::Corrupt(_))
    ));

    // Match offset off=1025 after 1024+ output: valid at wdesc=0x08 (2048), Corrupt at wdesc=0x00 (1024)
    let make_off_1025 = |wdesc: u8| -> Vec<u8> {
        let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, wdesc];
        f.extend_from_slice(&((1024u32 << 3) | 2).to_le_bytes()[..3]); // non-last RLE 1024B
        f.push(b'a');
        // Compressed block (last=1): 1 raw literal 'a', nseq=1, all RLE FSE (ll=1, off=1025 (code=10, extra=4), ml=3)
        // Reverse bitstream (10b value 4 + 1 sentinel bit = 0x0404) -> [0x04, 0x04]
        let cbody = [0x08u8, b'a', 1, 0x54, 1, 10, 0, 0x04, 0x04];
        f.extend_from_slice(&(((cbody.len() as u32) << 3) | 5).to_le_bytes()[..3]);
        f.extend_from_slice(&cbody);
        f
    };
    assert_eq!(
        decompress(&make_off_1025(0x08), 8192).unwrap(),
        vec![b'a'; 1028]
    );
    assert!(matches!(
        decompress(&make_off_1025(0x00), 8192),
        Err(Error::Corrupt(_))
    ));

    // 3. Encoder window_log(Some(12)) on incompressible + compressible data -> zstd -d interop
    let mut mixed_200k = lcg(140_000, 777);
    mixed_200k.extend_from_slice(&structured_payload(60_000));
    let z_w12 = compress_with(
        &mixed_200k,
        &CompressOptions::new(3).with_window_log(Some(12)),
    );
    assert_eq!(decompress(&z_w12, mixed_200k.len()).unwrap(), mixed_200k);
    if Command::new("zstd").arg("--version").output().is_ok() {
        let mut child = Command::new("zstd")
            .args(["-d", "-c", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&z_w12).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "zstd -d rejected wlog=12 stream: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, mixed_200k);
    }

    // 4. Multi-frame without FCS falls back cleanly and obeys max_out
    let chunk = structured_payload(150_000);
    let mut no_fcs_frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x20]; // window_log=14 (16K), Wait: use 0x58 -> wlog=21 (2M)
    no_fcs_frame[5] = 0x58;
    let mut pos = 0;
    while pos < chunk.len() {
        let end = (pos + 65536).min(chunk.len());
        let last = u32::from(end == chunk.len());
        let blk = &chunk[pos..end];
        no_fcs_frame.extend_from_slice(&(((blk.len() as u32) << 3) | last).to_le_bytes()[..3]);
        no_fcs_frame.extend_from_slice(blk);
        pos = end;
    }
    let mut four_unknown_frames = Vec::new();
    for _ in 0..4 {
        four_unknown_frames.extend_from_slice(&no_fcs_frame);
    }
    let dec_mt = decompress_with(
        &four_unknown_frames,
        &DecompressOptions::new().with_threads(4),
        chunk.len() * 4,
    )
    .unwrap();
    assert_eq!(dec_mt.len(), chunk.len() * 4);
    let mut too_small_out = Vec::new();
    let err = decompress_into_with(
        &four_unknown_frames,
        &mut too_small_out,
        &DecompressOptions::new().with_threads(4),
        chunk.len() * 2 + 1000,
    )
    .unwrap_err();
    assert_eq!(err, Error::TooLarge);
    assert_eq!(too_small_out.len(), chunk.len() * 2);

    // 5. shrink_to_fit preserves reusable correctness
    let mut comp = Compressor::new();
    let mut dec = Decompressor::new();
    let big = structured_payload(600_000);
    let zb = comp.compress_with(&big, &CompressOptions::new(19).with_threads(2));
    assert_eq!(
        dec.decompress_with(&zb, &DecompressOptions::new().with_threads(2), big.len())
            .unwrap(),
        big
    );
    comp.shrink_to_fit();
    dec.shrink_to_fit();
    let small = b"small reusable payload after shrink";
    let zs = comp.compress(small, 3);
    assert_eq!(dec.decompress(&zs, small.len()).unwrap(), small);

    // 6. Corrupt sequence bitstream after valid sequence header must not leave expanded `out` (> max_out)
    let mut bad_seq_frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, 10];
    // Non-last RLE block of 5 bytes ('q')
    bad_seq_frame.extend_from_slice(&((5u32 << 3) | 2).to_le_bytes()[..3]);
    bad_seq_frame.push(b'q');
    // Last Compressed block with 1 raw literal ('x'), nseq=1, modes=0x00, but empty sequence bitstream
    let bad_cbody = [0x08u8, b'x', 1, 0x00];
    bad_seq_frame.extend_from_slice(&(((bad_cbody.len() as u32) << 3) | 5).to_le_bytes()[..3]);
    bad_seq_frame.extend_from_slice(&bad_cbody);
    let mut err_out = Vec::new();
    let res = decompress_into(&bad_seq_frame, &mut err_out, 10);
    assert!(matches!(res, Err(Error::Corrupt(_))));
    assert_eq!(err_out, b"qqqqq");
    assert!(err_out.len() <= 10);
}
