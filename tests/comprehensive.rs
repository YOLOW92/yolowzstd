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

fn verify_with_zstd_cli(z: &[u8], expected: &[u8], label: &str) {
    if Command::new("zstd").arg("--version").output().is_err() {
        return;
    }
    let mut child = Command::new("zstd")
        .args(["-d", "-c", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(z).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "zstd -d rejected {label}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, expected, "zstd -d mismatch on {label}");
}

#[test]
fn adversarial_huffman_fse_weights_uniform_and_skewed() {
    // 构造字面量分布，使 Huffman 权重表 shown 出现：
    // 1) 全相等权重（active == 1，例如 12 个符号各 10 次 + 1 个符号 40 次 -> shown = [1; 12]）
    // 2) 全相等权重且 > 128 个（192 个符号各 10 次 + 符号 192 出现 640 次 -> shown = [1; 192]）
    // 3) 仅 1 个 0 + 192 个 1（probs = [1/-1, 63]，存在大量 nb == 0 状态！）
    let shuffle = |mut v: Vec<u8>, mut s: u64| -> Vec<u8> {
        for i in (1..v.len()).rev() {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = ((s >> 32) as usize) % (i + 1);
            v.swap(i, j);
        }
        v
    };

    // Case 1: 12 symbols (0..12) x 40 + sym 12 x 160 = 640 bytes (shown = [1; 12], active == 1)
    let mut case1 = Vec::new();
    for s in 0..12u8 {
        case1.extend(std::iter::repeat_n(s, 40));
    }
    case1.extend(std::iter::repeat_n(12u8, 160));
    let case1 = shuffle(case1, 101);

    // Case 2: 192 symbols (0..192) x 10 + sym 192 x 640 = 2560 bytes (shown = [1; 192], active == 1)
    let mut case2 = Vec::new();
    for s in 0..192u8 {
        case2.extend(std::iter::repeat_n(s, 10));
    }
    case2.extend(std::iter::repeat_n(192u8, 640));
    let case2 = shuffle(case2, 202);

    // Case 3: 192 symbols (1..=192) x 10 + sym 193 x 640 -> weight[0] = 0, weight[1..=192] = 1 (probs ~ [-1/1, 63])
    let mut case3 = Vec::new();
    for s in 1..=192u8 {
        case3.extend(std::iter::repeat_n(s, 10));
    }
    case3.extend(std::iter::repeat_n(193u8, 640));
    let case3 = shuffle(case3, 303);

    // Case 4: weight[0..192] = 1, weight[192] = 0, weight[193] = 7 (0 at end of shown!)
    let mut case4 = Vec::new();
    for s in 0..192u8 {
        case4.extend(std::iter::repeat_n(s, 10));
    }
    case4.extend(std::iter::repeat_n(193u8, 640));
    let case4 = shuffle(case4, 404);

    // Case 5: 192 symbols with 1 zero at index 190 (so w[n-2] == 0, w[n-1] == 1) and vice versa
    let mut case5 = Vec::new();
    for s in 0..193u8 {
        if s != 191 {
            case5.extend(std::iter::repeat_n(s, 10));
        }
    }
    case5.extend(std::iter::repeat_n(193u8, 640));
    let case5 = shuffle(case5, 505);

    let mut failures = Vec::new();
    for (name, data) in [
        ("case1_12_uniform", &case1),
        ("case2_192_uniform", &case2),
        ("case3_1zero_192ones", &case3),
        ("case4_192ones_1zero", &case4),
        ("case5_zero_at_n_minus_2", &case5),
    ] {
        for &lvl in &[1, 3, 9, 19] {
            let z = compress(data, lvl);
            match decompress(&z, data.len()) {
                Ok(dec) if dec == *data => {
                    verify_with_zstd_cli(&z, data, &format!("{name} L{lvl}"));
                }
                Ok(_) => failures.push(format!("{name} L{lvl}: data mismatch")),
                Err(e) => failures.push(format!("{name} L{lvl}: decompress err {e:?}")),
            }
        }
    }
    assert!(failures.is_empty(), "failures:\n{}", failures.join("\n"));
}

#[test]
fn adversarial_tiny_window_all_levels_and_zstd_interop() {
    // 构造在精确偏移 1022/1023/1024/1025/2048 处重复的数据，测试 window_log(10) 与 window_log(11) 全档位
    let mut data = Vec::with_capacity(20_000);
    let mut s = 0x55AA_1234_9876_FEDC_u64;
    for _ in 0..20_000 {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((s >> 48) as u8);
    }
    for &off in &[1022usize, 1023, 1024, 1025, 2047, 2048, 2049] {
        let pos = off * 3;
        if pos + 200 < data.len() {
            data.copy_within(pos - off..pos - off + 200, pos);
        }
    }
    // 叠加长共享前缀（> 512B）
    let long_pat = b"LONG_SHARED_PREFIX_TEST_BLOCK_0123456789_ABCDEF_".repeat(15); // 720 bytes
    for p in (2500..18000).step_by(1024) {
        if p + long_pat.len() + 4 < data.len() {
            data[p..p + long_pat.len()].copy_from_slice(&long_pat);
            data[p + long_pat.len()] = (p / 1024) as u8;
        }
    }

    for wlog in [10u32, 11] {
        for lvl in 0..=22 {
            let opts = CompressOptions::new(lvl).with_window_log(Some(wlog));
            let z = compress_with(&data, &opts);
            let dec = decompress(&z, data.len())
                .unwrap_or_else(|e| panic!("decompress failed wlog={wlog} L{lvl}: {e:?}"));
            assert_eq!(dec, data, "mismatch wlog={wlog} L{lvl}");
            if matches!(lvl, 1 | 3 | 5 | 9 | 12 | 16 | 19 | 22) {
                verify_with_zstd_cli(&z, &data, &format!("wlog={wlog} L{lvl}"));
            }
        }
    }
}

#[test]
fn adversarial_opt_chunk_overflow_litlen_and_long_prefixes() {
    // 1) 超过 65536 字节连续不可压缩字面量 + 末尾可压缩段（测试 OptParser init_ll / litlen u16 截断）
    let mut big_lit = Vec::with_capacity(90_000);
    let mut s = 0xDEAD_BEEF_CAFE_BABEu64;
    for _ in 0..72_000 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        big_lit.push((s >> 32) as u8);
    }
    big_lit.extend_from_slice(&b"match_after_72k_literals_0123456789\n".repeat(400));

    // 2) 密集短匹配无超长匹配，强制 OptParser 跨越 OPT_CHUNK=4096 切分
    let mut dense = Vec::with_capacity(20_000);
    let mut rng = 0x1122_3344_5566_7788u64;
    for i in 0..2500 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let tok = (rng >> 56) % 16;
        dense.extend_from_slice(&format!("t{tok:02x}_").into_bytes());
        dense.push(b'a' + (i % 26) as u8);
        dense.push(b'0' + ((rng >> 48) % 10) as u8);
    }

    // 3) 跨 128 KiB 块边界的超长前缀 (> 600B) 且直达 EOF 的精确重复
    let mut eof_rep = Vec::with_capacity(140_000);
    let stem = b"ZSTD_OPT_BST_SPLICING_STRESS_TEST_STEM_".repeat(18); // 702 bytes
    for i in 0..195u32 {
        eof_rep.extend_from_slice(&stem);
        eof_rep.push((i & 0xFF) as u8);
        eof_rep.push(((i * 7) & 0xFF) as u8);
    }
    eof_rep.extend_from_slice(&stem); // ends right at EOF with 702B match

    for (name, data) in [
        ("big_lit_72k", &big_lit),
        ("dense_opt_chunk", &dense),
        ("eof_long_prefix", &eof_rep),
    ] {
        for &lvl in &[12, 16, 19, 22] {
            let z = compress(data, lvl);
            let dec = decompress(&z, data.len()).unwrap();
            assert_eq!(dec, *data, "{name} L{lvl}");
            verify_with_zstd_cli(&z, data, &format!("{name} L{lvl}"));
        }
    }
}

#[test]
fn adversarial_skewed_fse_minus_one_and_compressor_reuse() {
    // 构造极偏斜序列分布（大量重复 ml=4 + 48 种长匹配各出现 1 次），迫使 FSE 产生大量 -1 极低概率符号
    let mut skewed_seqs = Vec::new();
    let seed_chunk = b"ABCD____EFGH____IJKL____MNOP____";
    skewed_seqs.extend_from_slice(seed_chunk);
    // 生成大量长度为 4 的匹配
    for i in 0..6000u32 {
        skewed_seqs.extend_from_slice(b"ABCD");
        skewed_seqs.push((i % 251) as u8);
    }
    // 插入 48 种不同长度的匹配（5..=500），触发极偏斜 ML 符号分布
    let big_pat: Vec<u8> = (0..600).map(|i| ((i * 73 + 19) % 251) as u8).collect();
    skewed_seqs.extend_from_slice(&big_pat);
    for k in 0..48usize {
        let len = 5 + k * 10;
        skewed_seqs.push(252 + (k % 3) as u8);
        skewed_seqs.extend_from_slice(&big_pat[..len]);
    }
    // 第二块复用/切换，测试 Repeat vs Custom
    skewed_seqs.extend_from_slice(&skewed_seqs.clone());

    for &lvl in &[1, 3, 5, 9, 12, 16, 19, 22] {
        let z = compress(&skewed_seqs, lvl);
        let dec = decompress(&z, skewed_seqs.len()).unwrap();
        assert_eq!(dec, skewed_seqs, "skewed_seqs L{lvl}");
        verify_with_zstd_cli(&z, &skewed_seqs, &format!("skewed_seqs L{lvl}"));
    }

    // 单个 Compressor 在大小帧、全档位间剧烈切换复用
    let mut comp = Compressor::new();
    let mut dec = Decompressor::new();
    let sizes = [
        200_000usize,
        0,
        1,
        3,
        4,
        7,
        8,
        63,
        64,
        65,
        65535,
        65536,
        65537,
        131_072,
        12,
        100_000,
        9,
    ];
    for (idx, &sz) in sizes.iter().enumerate() {
        let payload = if idx % 2 == 0 {
            structured_payload(sz)
        } else {
            lcg(sz, idx as u64 + 17)
        };
        for &lvl in &[1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 15, 16, 19, 22] {
            let z = comp.compress(&payload, lvl);
            let out = dec.decompress(&z, sz.max(1)).unwrap();
            assert_eq!(out, payload, "reuse sz={sz} lvl={lvl}");
        }
    }
}

#[test]
fn adversarial_huff_pair_table_repeat_and_mutation_safety() {
    // Block 1: 高偏斜 >= 10 KiB 字面量触发 pair_worth + pair_ready=true
    // Block 2..N: 使用 Repeat 树且字面量段很小（40..4100B），测试 decode_4x2 在小段上的所有边界
    let make_skewed = |n: usize, seed: u64| -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let x = (s >> 48) as usize % 100;
                if x < 55 {
                    b'e'
                } else if x < 80 {
                    b't'
                } else if x < 92 {
                    b'a'
                } else {
                    b'0' + (x % 8) as u8
                }
            })
            .collect()
    };
    let mut stream = make_skewed(128 * 1024, 1);
    for &tail_sz in &[40usize, 41, 42, 43, 44, 45, 256, 257, 300, 4095, 4096, 4097] {
        let mut d = stream.clone();
        d.extend_from_slice(&make_skewed(tail_sz, tail_sz as u64 + 7));
        let z = compress(&d, 1);
        assert_eq!(decompress(&z, d.len()).unwrap(), d, "tail_sz={tail_sz}");
        verify_with_zstd_cli(&z, &d, &format!("huff_pair tail_sz={tail_sz}"));
    }
    // 变异命中 decode_4x2 的压缩帧不应 panic
    let z = compress(&stream, 3);
    let mut rng = 0xABCD_1234_5678_90EFu64;
    let mut dec = Decompressor::new();
    let mut out = Vec::new();
    for _ in 0..1500 {
        let mut m = z.clone();
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let flips = 1 + ((rng >> 56) as usize % 3);
        for _ in 0..flips {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let idx = (rng as usize) % m.len();
            m[idx] ^= ((rng >> 40) as u8).max(1);
        }
        out.clear();
        let _ = dec.decompress_into(&m, &mut out, stream.len() + 4096);
    }
    stream.clear();
}

#[test]
fn adversarial_fibonacci_huff_depth_and_many_3byte_sequences() {
    // 1) 斐波那契频次（深度 > 11）触发 O(n) Kraft 截断，并经官方 zstd -d 互操作校验
    let mut fib_data = Vec::new();
    let (mut a, mut b) = (1usize, 1usize);
    for s in 0..22u8 {
        fib_data.extend(std::iter::repeat_n(s, a));
        let next = a + b;
        a = b;
        b = next;
    }
    let mut rng = 0x7788_99AA_BBCC_DDEEu64;
    for i in (1..fib_data.len()).rev() {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((rng >> 32) as usize) % (i + 1);
        fib_data.swap(i, j);
    }
    for &lvl in &[1, 3, 9, 19] {
        let z = compress(&fib_data, lvl);
        assert_eq!(decompress(&z, fib_data.len()).unwrap(), fib_data);
        verify_with_zstd_cli(&z, &fib_data, &format!("fibonacci L{lvl}"));
    }

    // 2) 单块超过 32512 (0x7F00) 条 3B 匹配序列（触发 3 字节序列数编码与 3B rep/Hash3）
    let mut many_seqs = Vec::with_capacity(131_072);
    let dict: [[u8; 3]; 16] = [
        *b"a0!", *b"b1@", *b"c2#", *b"d3$", *b"e4%", *b"f5^", *b"g6&", *b"h7*", *b"i8(", *b"j9)",
        *b"kA-", *b"lB=", *b"mC+", *b"nD[", *b"oE]", *b"pF;",
    ];
    for t in &dict {
        many_seqs.extend_from_slice(t);
    }
    let mut s = 0x1357_9BDF_2468_ACE0u64;
    while many_seqs.len() + 3 <= 131_072 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let idx = ((s >> 56) as usize) & 15;
        many_seqs.extend_from_slice(&dict[idx]);
    }
    for &lvl in &[5, 9, 16, 19] {
        let z = compress(&many_seqs, lvl);
        assert_eq!(decompress(&z, many_seqs.len()).unwrap(), many_seqs);
        verify_with_zstd_cli(&z, &many_seqs, &format!("many_3byte_seqs L{lvl}"));
    }
}

#[test]
fn adversarial_zero_copy_mt_decode_boundaries() {
    // 构造 4 帧（总压缩 > 128 KiB 以触发 mt::decompress_parallel 零拷贝 split_at_mut 路径），
    // 每帧末尾 32 字节精确覆盖 off=1..=15 重叠匹配、大字面量尾段与校验和。
    let mut frames = Vec::new();
    let mut expected = Vec::new();
    for fi in 0..4usize {
        let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ (fi as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let mut p: Vec<u8> = (0..40_000 + fi * 13)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 40) as u8
            })
            .collect();
        p.extend_from_slice(&structured_payload(40_000));
        // 末尾 32 字节制造短偏移重叠匹配（off = 1, 2, 3, 7）
        let pat_len = [1usize, 2, 3, 7][fi];
        let seed: Vec<u8> = (0..pat_len).map(|k| b'A' + ((fi * 5 + k) as u8)).collect();
        for _ in 0..(28 / pat_len) {
            p.extend_from_slice(&seed);
        }
        let opts = CompressOptions::new(3)
            .with_threads(1)
            .with_checksum(fi % 2 == 0);
        let z = compress_with(&p, &opts);
        frames.extend_from_slice(&z);
        expected.extend_from_slice(&p);
    }
    assert!(frames.len() >= 128 * 1024);

    // 1) 非空 out 前缀 + 多线程零拷贝解压
    let mut out = b"PREFIX!".to_vec();
    let dec_opts = DecompressOptions::new().with_threads(4);
    let exact_cap = out.len() + expected.len();
    decompress_into_with(&frames, &mut out, &dec_opts, exact_cap).unwrap();
    assert_eq!(&out[..7], b"PREFIX!");
    assert_eq!(&out[7..], expected.as_slice());

    // 2) max_out 精确边界截断返回 TooLarge，且与单线程解码保留前缀/完整块完全一致
    let mut out_mt = b"KEEP".to_vec();
    let mut out_st = b"KEEP".to_vec();
    let small_cap = out_mt.len() + expected.len() - 1;
    let err_mt = decompress_into_with(&frames, &mut out_mt, &dec_opts, small_cap).unwrap_err();
    let err_st = decompress_into(&frames, &mut out_st, small_cap).unwrap_err();
    assert!(matches!(err_mt, Error::TooLarge));
    assert!(matches!(err_st, Error::TooLarge));
    assert_eq!(out_mt, out_st);

    // 3) 中途损坏帧返回 Err，且与单线程解码保留的前缀完整块完全一致
    let mut bad = frames.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0xFF;
    let mut out_bad_mt = b"ORIG".to_vec();
    let mut out_bad_st = b"ORIG".to_vec();
    assert!(decompress_into_with(&bad, &mut out_bad_mt, &dec_opts, 4 * 1024 * 1024).is_err());
    assert!(decompress_into(&bad, &mut out_bad_st, 4 * 1024 * 1024).is_err());
    assert_eq!(out_bad_mt, out_bad_st);
}

#[test]
fn adversarial_mt_multiblock_tail_tiny_frames_and_forged_fcs() {
    let mut frames = Vec::new();
    let mut expected = Vec::new();

    // 1) 多块帧：倒数第二块恰好落入帧末 32 字节内（131072 + {1, 7, 16, 31, 32, 33}），
    //    并在帧内混合 Raw / RLE / Compressed 块与末尾短偏移重叠匹配
    for (fi, &extra) in [1usize, 7, 16, 31, 32, 33].iter().enumerate() {
        let mut p = Vec::with_capacity(131_072 + extra);
        if fi % 2 == 0 {
            // 第 1 块含高熵不可压缩段（确保总压缩字节 >= 128 KiB 触发 mt 路径）+ 结构化可压缩段
            let mut xs =
                0x243F_6A88_85A3_08D3u64 ^ (fi as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for _ in 0..60_000 {
                xs ^= xs << 13;
                xs ^= xs >> 7;
                xs ^= xs << 17;
                p.push((xs >> 40) as u8);
            }
            p.extend_from_slice(&structured_payload(71_072));
        } else {
            // 第 1 块含纯 RLE 特征再接可压缩块
            p.extend(std::iter::repeat_n(b'Z', 131_072));
        }
        let tail_pat = &b"abcaabbcc1234567890abcdefghijklmno"[..extra];
        p.extend_from_slice(tail_pat);
        let z = compress_with(
            &p,
            &CompressOptions::new(3)
                .with_threads(1)
                .with_checksum(fi % 2 == 0),
        );
        frames.extend_from_slice(&z);
        expected.extend_from_slice(&p);

        // 穿插跳过帧（skippable frame）
        frames.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
        frames.extend_from_slice(&4u32.to_le_bytes());
        frames.extend_from_slice(b"SKIP");
    }

    // 2) 小于 32B 的极短帧（0, 1, 3, 15, 31, 32, 33, 64B）与 window_log=10 多小块帧混拼
    for &tiny_len in &[0usize, 1, 3, 15, 31, 32, 33, 64] {
        let p = structured_payload(tiny_len);
        let z = compress_with(&p, &CompressOptions::new(3).with_threads(1));
        frames.extend_from_slice(&z);
        expected.extend_from_slice(&p);
    }
    let small_win_payload = structured_payload(160 * 1024 + 19);
    let z_sw = compress_with(
        &small_win_payload,
        &CompressOptions::new(3)
            .with_threads(1)
            .with_window_log(Some(10)),
    );
    frames.extend_from_slice(&z_sw);
    expected.extend_from_slice(&small_win_payload);
    assert!(frames.len() >= 128 * 1024);

    let dec_opts = DecompressOptions::new().with_threads(4);
    let mut dec = Decompressor::new();
    let mut out_mt = b"HDR:".to_vec();
    let exact_mt_cap = out_mt.len() + expected.len();
    dec.decompress_into_with(&frames, &mut out_mt, &dec_opts, exact_mt_cap)
        .unwrap();
    assert_eq!(&out_mt[..4], b"HDR:");
    assert_eq!(&out_mt[4..], expected.as_slice());

    // 与单线程解码严格一致
    let out_st = decompress(&frames, expected.len()).unwrap();
    assert_eq!(out_st, expected);

    // 3) 跨帧状态机（Predef -> RLE -> Repeat -> Custom -> 损坏帧 -> Raw -> Predef）复用校验
    let mut corrupt_mid = frames.clone();
    corrupt_mid[frames.len() / 3] ^= 0x5A;
    let mut tmp = Vec::new();
    let _ = dec.decompress_into_with(&corrupt_mid, &mut tmp, &dec_opts, expected.len() + 4096);
    let raw_only = compress(&lcg(40, 999), 0);
    assert_eq!(dec.decompress(&raw_only, 64).unwrap(), lcg(40, 999));
    let mut out_after = Vec::new();
    dec.decompress_into_with(&frames, &mut out_after, &dec_opts, expected.len())
        .unwrap();
    assert_eq!(out_after, expected);

    // 4) 伪造超大 Frame_Content_Size 防 OOM / 防巨额清零 DoS 校验：
    //    两帧各 1 个小压缩块却声称各 512 MiB（总计 1 GiB），总输入 > 128 KiB 进入 mt 路径，
    //    scan_frames 必须按块上限直接拒绝对 1 GiB 预分配！
    let mut forged = Vec::with_capacity(140 * 1024);
    for _ in 0..2 {
        // 合法魔数 + Single_Segment=0, FCS=4B (0x80), Window_Desc=0x50 (1 MiB), FCS=512 MiB
        forged.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        forged.push(0x80);
        forged.push(0x50);
        forged.extend_from_slice(&(512u32 << 20).to_le_bytes());
        // 单个 70 KiB 压缩块（last=1, type=2, size=70 KiB），总块上限仅 128 KiB << 512 MiB
        let bsz = 70 * 1024usize;
        let bh = ((bsz << 3) | 4 | 1) as u32;
        forged.extend_from_slice(&bh.to_le_bytes()[..3]);
        forged.extend(std::iter::repeat_n(0u8, bsz));
    }
    assert!(forged.len() >= 128 * 1024);
    let mut out_forged = b"SAFE".to_vec();
    let err = dec
        .decompress_into_with(&forged, &mut out_forged, &dec_opts, 2usize << 30)
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)));
    assert!(
        out_forged.capacity() < (64 << 20),
        "forged FCS caused unexpected giant pre-allocation: {} bytes",
        out_forged.capacity()
    );
    assert_eq!(&out_forged[..4], b"SAFE");

    // 5) 跳过帧 u32::MAX 长度溢出边界（覆盖 32 位算术安全路径）
    for &skip_sz in &[u32::MAX - 3, u32::MAX - 1, u32::MAX] {
        let mut bad_skip = Vec::new();
        bad_skip.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
        bad_skip.extend_from_slice(&skip_sz.to_le_bytes());
        assert!(matches!(
            decompress(&bad_skip, 1024),
            Err(Error::Corrupt("skip frame size"))
        ));
    }

    // 6) 多帧并行变异 fuzz：300 次随机位翻转不得 panic，且与单线程严格等价
    let mut rng = 0xD1B5_4A32_D192_ED03u64;
    let mut dec_mt = Decompressor::new();
    let mut dec_st = Decompressor::new();
    let mut buf_mt = Vec::new();
    let mut buf_st = Vec::new();
    for _ in 0..300 {
        let mut mutated = frames.clone();
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let flips = 1 + ((rng >> 56) as usize % 3);
        for _ in 0..flips {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let idx = (rng as usize) % mutated.len();
            mutated[idx] ^= ((rng >> 40) as u8).max(1);
        }
        buf_mt.clear();
        buf_mt.extend_from_slice(b"PFX");
        buf_st.clear();
        buf_st.extend_from_slice(b"PFX");
        let cap = expected.len() + 64 * 1024;
        let r_mt = dec_mt.decompress_into_with(&mutated, &mut buf_mt, &dec_opts, cap);
        let r_st = dec_st.decompress_into(&mutated, &mut buf_st, cap);
        assert_eq!(r_mt.is_ok(), r_st.is_ok());
        assert_eq!(buf_mt, buf_st);
    }
}

#[test]
fn adversarial_opt_chunk_tail_truncation_exact_boundaries() {
    // 针对 OptParser OPT_CHUNK=4096 边界修正（idx.min(last_pos).min(end - cur)）：
    // 构造不含 sufficient 长匹配的密集短重复序列，在 4095/4096/4097/4105/8192/16384 块边界验证闭环
    let make_dense = |len: usize, seed: u64| -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut rng = seed;
        let toks: [&[u8]; 8] = [
            b"alpha_", b"beta__", b"gamma_", b"delta_", b"eps___", b"zeta__", b"eta___", b"theta_",
        ];
        while out.len() < len {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let t = toks[((rng >> 56) as usize) & 7];
            out.extend_from_slice(t);
            out.push(b'0' + ((rng >> 48) % 10) as u8);
        }
        out.truncate(len);
        out
    };

    for &sz in &[4095usize, 4096, 4097, 4105, 4130, 8192, 12_300] {
        let data = make_dense(sz, sz as u64 ^ 0x55AA_1122_3344_7788);
        for &lvl in &[16, 19, 22] {
            let z = compress(&data, lvl);
            let dec = decompress(&z, data.len()).unwrap();
            assert_eq!(dec, data, "opt_chunk boundary sz={sz} L{lvl}");
            if sz == 4096 || sz == 4105 {
                verify_with_zstd_cli(&z, &data, &format!("opt_chunk sz={sz} L{lvl}"));
            }
        }
    }
}
