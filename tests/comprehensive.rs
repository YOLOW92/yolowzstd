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
