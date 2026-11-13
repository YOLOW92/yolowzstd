use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::Instant;
use zstd_pure_rs::prelude::*;

const SILESIA_FILES: &[&str] = &[
    "dickens", "mozilla", "mr", "nci", "ooffice", "osdb", "reymont", "samba", "sao", "webster",
    "x-ray", "xml",
];

fn load_silesia(dir: &Path) -> Vec<(String, Vec<u8>)> {
    SILESIA_FILES
        .iter()
        .map(|name| {
            let data = fs::read(dir.join(name)).expect("read silesia file");
            ((*name).to_string(), data)
        })
        .collect()
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn measure<F: FnMut()>(rounds: usize, mut f: F) -> f64 {
    f(); // warmup
    let mut times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64());
    }
    median(&mut times)
}

fn dec_yolow(dec: &mut yolowzstd::Decompressor, src: &[u8], out: &mut Vec<u8>, max_out: usize) {
    out.clear();
    dec.decompress_into(src, out, max_out).unwrap();
}

fn dec_c_zstd(dec: &mut zstd::bulk::Decompressor<'_>, src: &[u8], out: &mut Vec<u8>, cap: usize) {
    out.resize(cap, 0);
    let n = dec.decompress_to_buffer(src, out.as_mut_slice()).unwrap();
    out.truncate(n);
}

fn dec_rust_zstd(dec: &mut rust_zstd::Decompressor, src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    dec.decompress_into(src, out).unwrap();
}

fn dec_zstd_pure_rs(src: &[u8], out: &mut Vec<u8>, cap: usize) {
    out.resize(cap, 0);
    let n = ZSTD_decompress(out.as_mut_slice(), src);
    assert!(!ERR_isError(n));
    out.truncate(n);
}

fn dec_zstd_rs(dec: &mut zstd_rs::Decompressor, src: &[u8], out: &mut Vec<u8>, cap: usize) {
    out.clear();
    dec.decompress(src, None, cap, out).unwrap();
}

fn dec_ruzstd(dec: &mut ruzstd::decoding::FrameDecoder, src: &[u8], out: &mut Vec<u8>, cap: usize) {
    out.clear();
    out.reserve(cap);
    dec.decode_all_to_vec(src, out).unwrap();
}

fn dec_structured(
    dec: &mut structured_zstd::decoding::FrameDecoder,
    src: &[u8],
    out: &mut Vec<u8>,
    cap: usize,
) {
    out.clear();
    out.reserve(cap);
    dec.decode_all_to_vec(src, out).unwrap();
}

fn dec_oxiarc(src: &[u8], out: &mut Vec<u8>, cap: usize) {
    out.resize(cap, 0);
    let n = oxiarc_zstd::decompress_into(src, out.as_mut_slice()).unwrap();
    out.truncate(n);
}

fn cmd_smoke(dir: &Path) {
    let files = load_silesia(dir);
    let sample = &files[0].1[..256 * 1024]; // 256 KiB dickens
    let cz = zstd::bulk::compress(sample, 3).unwrap();

    let mut yz_dec = yolowzstd::Decompressor::new();
    // Decode check
    let mut out = Vec::new();
    dec_yolow(&mut yz_dec, &cz, &mut out, sample.len());
    assert_eq!(out, sample, "yolowzstd dec");

    let mut rz = rust_zstd::Decompressor::new();
    dec_rust_zstd(&mut rz, &cz, &mut out);
    assert_eq!(out, sample, "rust-zstd dec");

    dec_zstd_pure_rs(&cz, &mut out, sample.len());
    assert_eq!(out, sample, "zstd-pure-rs dec");

    let mut zrs = zstd_rs::Decompressor::new();
    dec_zstd_rs(&mut zrs, &cz, &mut out, sample.len());
    assert_eq!(out, sample, "zstd-rs dec");

    let mut ruz = ruzstd::decoding::FrameDecoder::new();
    dec_ruzstd(&mut ruz, &cz, &mut out, sample.len());
    assert_eq!(out, sample, "ruzstd dec");

    let mut sz = structured_zstd::decoding::FrameDecoder::new();
    dec_structured(&mut sz, &cz, &mut out, sample.len());
    assert_eq!(out, sample, "structured-zstd dec");

    dec_oxiarc(&cz, &mut out, sample.len());
    assert_eq!(out, sample, "oxiarc-zstd dec");

    // Encode interoperability check via C zstd decoder
    assert_eq!(zstd::bulk::decompress(&yolowzstd::compress(sample, 3), sample.len()).unwrap(), sample);
    assert_eq!(
        zstd::bulk::decompress(&rust_zstd::compress_to_vec(sample), sample.len()).unwrap(),
        sample
    );
    let bound = sample.len() + (sample.len() >> 8) + 128;
    let mut cbuf = vec![0u8; bound];
    let n = ZSTD_compress(&mut cbuf, sample, 3);
    assert!(!ERR_isError(n));
    assert_eq!(zstd::bulk::decompress(&cbuf[..n], sample.len()).unwrap(), sample);

    let mut zrse = zstd_rs::Compressor::new(zstd_rs::CompressionConfig::DEFAULT).unwrap();
    let mut zout = Vec::new();
    zrse.compress(sample, None, &mut zout).unwrap();
    assert_eq!(zstd::bulk::decompress(&zout, sample.len()).unwrap(), sample);

    let ruz_enc =
        ruzstd::encoding::compress_to_vec(sample, ruzstd::encoding::CompressionLevel::Fastest);
    assert_eq!(zstd::bulk::decompress(&ruz_enc, sample.len()).unwrap(), sample);

    // Cross-codec sanity check
    let mut ld_c = libdeflater::Compressor::new(libdeflater::CompressionLvl::default());
    let mut ld_out = vec![0u8; ld_c.deflate_compress_bound(sample.len())];
    let ld_n = ld_c.deflate_compress(sample, &mut ld_out).unwrap();
    let mut ld_d = libdeflater::Decompressor::new();
    let mut ld_rec = vec![0u8; sample.len()];
    assert_eq!(ld_d.deflate_decompress(&ld_out[..ld_n], &mut ld_rec).unwrap(), sample.len());
    assert_eq!(ld_rec, sample);

    let mut zr_out = vec![0u8; zlib_rs::compress_bound(sample.len())];
    let (zr_c, rc) = zlib_rs::compress_slice(&mut zr_out, sample, zlib_rs::DeflateConfig::new(6));
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    let mut zr_rec = vec![0u8; sample.len()];
    let (zr_d, rc2) = zlib_rs::decompress_slice(&mut zr_rec, zr_c, zlib_rs::InflateConfig::default());
    assert_eq!(rc2, zlib_rs::ReturnCode::Ok);
    assert_eq!(zr_d, sample);

    let lz4_c = lz4_flex::block::compress(sample);
    let lz4_rec = lz4_flex::block::decompress(&lz4_c, sample.len()).unwrap();
    assert_eq!(lz4_rec, sample);

    let sz_enc = structured_zstd::encoding::compress_slice_to_vec(
        sample,
        structured_zstd::encoding::CompressionLevel::from_level(3),
    );
    assert_eq!(zstd::bulk::decompress(&sz_enc, sample.len()).unwrap(), sample);

    let ox_enc = oxiarc_zstd::compress_with_level(sample, 3).unwrap();
    assert_eq!(zstd::bulk::decompress(&ox_enc, sample.len()).unwrap(), sample);

    println!("SMOKE PASS: all 8 Zstd implementations verified against C libzstd!");
}

fn cmd_decode(dir: &Path, level: i32, rounds: usize) {
    let files = load_silesia(dir);
    let mut yz_dec = yolowzstd::Decompressor::new();
    let mut c_dec = zstd::bulk::Decompressor::new().unwrap();
    let mut rz_dec = rust_zstd::Decompressor::new();
    let mut zrs_dec = zstd_rs::Decompressor::new();
    let mut ruz_dec = ruzstd::decoding::FrameDecoder::new();
    let mut sz_dec = structured_zstd::decoding::FrameDecoder::new();
    let mut out = Vec::with_capacity(64 << 20);

    println!("=== DECODE (source compressed by C libzstd level {level}, median of {rounds} rounds, MB/s) ===");
    println!("file\tsize_b\tyolowzstd\tc_libzstd\trust_zstd\tzstd_pure_rs\tstructured\truzstd\tzstd_rs\toxiarc");

    let mut tot_bytes = 0usize;
    let mut sums = [0.0f64; 8];
    for (name, raw) in &files {
        let comp = zstd::bulk::compress(raw, level).unwrap();
        let mb = raw.len() as f64 / 1e6;
        tot_bytes += raw.len();

        let t0 = measure(rounds, || {
            dec_yolow(&mut yz_dec, &comp, &mut out, raw.len());
            black_box(&out);
        });
        assert_eq!(out.as_slice(), raw.as_slice());

        let t1 = measure(rounds, || {
            dec_c_zstd(&mut c_dec, &comp, &mut out, raw.len());
            black_box(&out);
        });

        let t2 = measure(rounds, || {
            dec_rust_zstd(&mut rz_dec, &comp, &mut out);
            black_box(&out);
        });

        let t3 = measure(rounds, || {
            dec_zstd_pure_rs(&comp, &mut out, raw.len());
            black_box(&out);
        });

        let t4 = measure(rounds, || {
            dec_structured(&mut sz_dec, &comp, &mut out, raw.len());
            black_box(&out);
        });

        let t5 = measure(rounds, || {
            dec_ruzstd(&mut ruz_dec, &comp, &mut out, raw.len());
            black_box(&out);
        });

        let t6 = measure(rounds, || {
            dec_zstd_rs(&mut zrs_dec, &comp, &mut out, raw.len());
            black_box(&out);
        });

        let t7 = measure(rounds, || {
            dec_oxiarc(&comp, &mut out, raw.len());
            black_box(&out);
        });

        let ts = [t0, t1, t2, t3, t4, t5, t6, t7];
        for (s, t) in sums.iter_mut().zip(ts.iter()) {
            *s += *t;
        }
        println!(
            "{name}\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
            raw.len(),
            mb / t0,
            mb / t1,
            mb / t2,
            mb / t3,
            mb / t4,
            mb / t5,
            mb / t6,
            mb / t7,
        );
    }
    let tot_mb = tot_bytes as f64 / 1e6;
    println!(
        "TOTAL\t{tot_bytes}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
        tot_mb / sums[0],
        tot_mb / sums[1],
        tot_mb / sums[2],
        tot_mb / sums[3],
        tot_mb / sums[4],
        tot_mb / sums[5],
        tot_mb / sums[6],
        tot_mb / sums[7],
    );
}

fn cmd_encode(dir: &Path, level: i32, rounds: usize) {
    let files = load_silesia(dir);
    let yz_opts = yolowzstd::CompressOptions::new(level);
    let mut yz_comp = yolowzstd::Compressor::new();
    let mut c_comp = zstd::bulk::Compressor::new(level).unwrap();
    let mut sz_comp: structured_zstd::encoding::FrameCompressor =
        structured_zstd::encoding::FrameCompressor::new(
            structured_zstd::encoding::CompressionLevel::from_level(level),
        );
    let rz_opts = rust_zstd::CompressOptions {
        level,
        checksum: true,
        ..rust_zstd::CompressOptions::default()
    };
    let mut rz_comp = rust_zstd::Compressor::new(rz_opts.clone());
    let zrs_cfg = zstd_rs::CompressionConfig {
        level,
        checksum: true,
        ..zstd_rs::CompressionConfig::DEFAULT
    };
    let mut zrs_comp = zstd_rs::Compressor::new(zrs_cfg).unwrap();

    println!("=== ENCODE level {level} (reused ctx where supported, median of {rounds} rounds) ===");
    println!("file\traw_b\tyolow_MBs\tyolow_rat\tc_MBs\tc_rat\trz_MBs\trz_rat\tpure_MBs\tpure_rat\tsz_MBs\tsz_rat\tzrs_MBs\tzrs_rat\tox_MBs\tox_rat");

    let mut tot_raw = 0usize;
    let mut times = [0.0f64; 7];
    let mut sizes = [0usize; 7];
    let mut buf = Vec::with_capacity(64 << 20);
    for (name, raw) in &files {
        let mb = raw.len() as f64 / 1e6;
        tot_raw += raw.len();
        let bound = raw.len() + (raw.len() >> 7) + 4096;

        let mut s0 = 0;
        let t0 = measure(rounds, || {
            buf.clear();
            yz_comp.compress_into(raw, &mut buf, &yz_opts);
            s0 = buf.len();
            black_box(&buf);
        });

        let mut s1 = 0;
        let t1 = measure(rounds, || {
            buf.resize(bound, 0);
            s1 = c_comp.compress_to_buffer(raw, &mut buf).unwrap();
            black_box(&buf[..s1]);
        });

        let mut s2 = 0;
        let t2 = measure(rounds, || {
            buf.clear();
            rz_comp.compress(raw, &mut buf);
            s2 = buf.len();
            black_box(&buf);
        });

        let mut s3 = 0;
        let t3 = measure(rounds, || {
            buf.resize(bound, 0);
            s3 = ZSTD_compress(&mut buf, raw, level);
            assert!(!ERR_isError(s3));
            black_box(&buf[..s3]);
        });

        let mut s4 = 0;
        let t4 = measure(rounds, || {
            buf.clear();
            sz_comp.compress_independent_frame_into(raw, &mut buf);
            s4 = buf.len();
            black_box(&buf);
        });

        let mut s5 = 0;
        let t5 = measure(rounds, || {
            buf.clear();
            zrs_comp.compress(raw, None, &mut buf).unwrap();
            s5 = buf.len();
            black_box(&buf);
        });

        let mut s6 = 0;
        let t6 = measure(rounds, || {
            let z = oxiarc_zstd::compress_with_level(raw, level).unwrap();
            s6 = z.len();
            black_box(z);
        });

        let ts = [t0, t1, t2, t3, t4, t5, t6];
        let ss = [s0, s1, s2, s3, s4, s5, s6];
        for i in 0..7 {
            times[i] += ts[i];
            sizes[i] += ss[i];
        }
        println!(
            "{name}\t{}\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x",
            raw.len(),
            mb / t0, raw.len() as f64 / s0 as f64,
            mb / t1, raw.len() as f64 / s1 as f64,
            mb / t2, raw.len() as f64 / s2 as f64,
            mb / t3, raw.len() as f64 / s3 as f64,
            mb / t4, raw.len() as f64 / s4 as f64,
            mb / t5, raw.len() as f64 / s5 as f64,
            mb / t6, raw.len() as f64 / s6 as f64,
        );
    }
    let tot_mb = tot_raw as f64 / 1e6;
    println!(
        "TOTAL\t{}\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x\t{:.1}\t{:.2}x",
        tot_raw,
        tot_mb / times[0], tot_raw as f64 / sizes[0] as f64,
        tot_mb / times[1], tot_raw as f64 / sizes[1] as f64,
        tot_mb / times[2], tot_raw as f64 / sizes[2] as f64,
        tot_mb / times[3], tot_raw as f64 / sizes[3] as f64,
        tot_mb / times[4], tot_raw as f64 / sizes[4] as f64,
        tot_mb / times[5], tot_raw as f64 / sizes[5] as f64,
        tot_mb / times[6], tot_raw as f64 / sizes[6] as f64,
    );
}

fn cmd_cross(dir: &Path, rounds: usize) {
    let files = load_silesia(dir);
    let yz_opts = yolowzstd::CompressOptions::new(3);
    let mut yz_comp = yolowzstd::Compressor::new();
    let mut yz_dec = yolowzstd::Decompressor::new();
    let mut c_comp = zstd::bulk::Compressor::new(3).unwrap();
    let mut c_dec = zstd::bulk::Decompressor::new().unwrap();
    let mut ld_c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
    let mut ld_d = libdeflater::Decompressor::new();
    let mut out = Vec::with_capacity(64 << 20);
    let mut cbuf = Vec::with_capacity(64 << 20);

    println!("=== CROSS-CODEC ON SILESIA (median of {rounds} rounds) ===");
    println!("file\traw_b\tyz_enc\tyz_dec\tyz_rat\tcz_enc\tcz_dec\tcz_rat\tld_enc\tld_dec\tld_rat\tzr_enc\tzr_dec\tzr_rat\tlz4_enc\tlz4_dec\tlz4_rat");

    let mut tot_raw = 0usize;
    let mut enc_t = [0.0f64; 5];
    let mut dec_t = [0.0f64; 5];
    let mut c_sz = [0usize; 5];

    for (name, raw) in &files {
        let mb = raw.len() as f64 / 1e6;
        tot_raw += raw.len();

        // 0: yolowzstd L3
        let mut syz = 0;
        let ty_e = measure(rounds, || {
            cbuf.clear();
            yz_comp.compress_into(raw, &mut cbuf, &yz_opts);
            syz = cbuf.len();
            black_box(&cbuf);
        });
        let yz_c = yz_comp.compress(raw, 3);
        let ty_d = measure(rounds, || {
            dec_yolow(&mut yz_dec, &yz_c, &mut out, raw.len());
            black_box(&out);
        });

        // 1: C libzstd L3
        let bound_z = raw.len() + (raw.len() >> 7) + 4096;
        let mut scz = 0;
        let tc_e = measure(rounds, || {
            cbuf.resize(bound_z, 0);
            scz = c_comp.compress_to_buffer(raw, &mut cbuf).unwrap();
            black_box(&cbuf[..scz]);
        });
        let cz_c = zstd::bulk::compress(raw, 3).unwrap();
        let tc_d = measure(rounds, || {
            dec_c_zstd(&mut c_dec, &cz_c, &mut out, raw.len());
            black_box(&out);
        });

        // 2: libdeflater (C libdeflate L6)
        let bound_ld = ld_c.deflate_compress_bound(raw.len());
        let mut sld = 0;
        let tld_e = measure(rounds, || {
            cbuf.resize(bound_ld, 0);
            sld = ld_c.deflate_compress(raw, &mut cbuf).unwrap();
            black_box(&cbuf[..sld]);
        });
        let ld_c_bytes = {
            cbuf.resize(bound_ld, 0);
            let n = ld_c.deflate_compress(raw, &mut cbuf).unwrap();
            cbuf[..n].to_vec()
        };
        let tld_d = measure(rounds, || {
            out.resize(raw.len(), 0);
            let n = ld_d.deflate_decompress(&ld_c_bytes, &mut out).unwrap();
            assert_eq!(n, raw.len());
            black_box(&out);
        });

        // 3: zlib-rs L6
        let bound_zr = zlib_rs::compress_bound(raw.len());
        let mut szr = 0;
        let tzr_e = measure(rounds, || {
            cbuf.resize(bound_zr, 0);
            let (sl, _) = zlib_rs::compress_slice(&mut cbuf, raw, zlib_rs::DeflateConfig::new(6));
            szr = sl.len();
            black_box(szr);
        });
        let zr_c_bytes = {
            cbuf.resize(bound_zr, 0);
            let (sl, _) = zlib_rs::compress_slice(&mut cbuf, raw, zlib_rs::DeflateConfig::new(6));
            sl.to_vec()
        };
        let tzr_d = measure(rounds, || {
            out.resize(raw.len(), 0);
            let (sl, _) = zlib_rs::decompress_slice(&mut out, &zr_c_bytes, zlib_rs::InflateConfig::default());
            assert_eq!(sl.len(), raw.len());
            black_box(&out);
        });

        // 4: lz4_flex block
        let bound_lz4 = lz4_flex::block::get_maximum_output_size(raw.len());
        let mut slz4 = 0;
        let tlz4_e = measure(rounds, || {
            cbuf.resize(bound_lz4, 0);
            slz4 = lz4_flex::block::compress_into(raw, &mut cbuf).unwrap();
            black_box(&cbuf[..slz4]);
        });
        let lz4_c_bytes = lz4_flex::block::compress(raw);
        let tlz4_d = measure(rounds, || {
            out.resize(raw.len(), 0);
            let n = lz4_flex::block::decompress_into(&lz4_c_bytes, &mut out).unwrap();
            assert_eq!(n, raw.len());
            black_box(&out);
        });

        let et = [ty_e, tc_e, tld_e, tzr_e, tlz4_e];
        let dt = [ty_d, tc_d, tld_d, tzr_d, tlz4_d];
        let sz = [syz, scz, sld, szr, slz4];
        for i in 0..5 {
            enc_t[i] += et[i];
            dec_t[i] += dt[i];
            c_sz[i] += sz[i];
        }
        println!(
            "{name}\t{}\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x",
            raw.len(),
            mb / ty_e, mb / ty_d, raw.len() as f64 / syz as f64,
            mb / tc_e, mb / tc_d, raw.len() as f64 / scz as f64,
            mb / tld_e, mb / tld_d, raw.len() as f64 / sld as f64,
            mb / tzr_e, mb / tzr_d, raw.len() as f64 / szr as f64,
            mb / tlz4_e, mb / tlz4_d, raw.len() as f64 / slz4 as f64,
        );
    }
    let tot_mb = tot_raw as f64 / 1e6;
    println!(
        "TOTAL\t{tot_raw}\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.2}x",
        tot_mb / enc_t[0], tot_mb / dec_t[0], tot_raw as f64 / c_sz[0] as f64,
        tot_mb / enc_t[1], tot_mb / dec_t[1], tot_raw as f64 / c_sz[1] as f64,
        tot_mb / enc_t[2], tot_mb / dec_t[2], tot_raw as f64 / c_sz[2] as f64,
        tot_mb / enc_t[3], tot_mb / dec_t[3], tot_raw as f64 / c_sz[3] as f64,
        tot_mb / enc_t[4], tot_mb / dec_t[4], tot_raw as f64 / c_sz[4] as f64,
    );
}

fn cmd_short(dir: &Path, rounds: usize) {
    let dickens = fs::read(dir.join("dickens")).expect("read dickens");
    let yz_opts = yolowzstd::CompressOptions::new(3);
    let mut yz_comp = yolowzstd::Compressor::new();
    let mut yz_dec = yolowzstd::Decompressor::new();
    let mut c_comp = zstd::bulk::Compressor::new(3).unwrap();
    let mut c_dec = zstd::bulk::Decompressor::new().unwrap();
    let mut rz_dec = rust_zstd::Decompressor::new();
    let mut rz_comp = rust_zstd::Compressor::new(rust_zstd::CompressOptions::default());
    let mut sz_dec = structured_zstd::decoding::FrameDecoder::new();
    let mut sz_comp: structured_zstd::encoding::FrameCompressor =
        structured_zstd::encoding::FrameCompressor::new(
            structured_zstd::encoding::CompressionLevel::from_level(3),
        );
    let mut out = Vec::with_capacity(128 * 1024);
    let mut cbuf = Vec::with_capacity(128 * 1024);

    println!("=== SHORT PAYLOADS (dickens prefixes, median of {rounds} batch loops, MB/s) ===");
    println!("size_b\tyz_dec\tcz_dec\trz_dec\tsz_dec\tyz_enc\tcz_enc\trz_enc\tsz_enc");
    for &sz in &[64usize, 1024, 4096, 65536] {
        let iters = (4 * 1024 * 1024 / sz).max(50);
        let raw = &dickens[..sz];
        let comp = zstd::bulk::compress(raw, 3).unwrap();
        let tot_mb = (sz * iters) as f64 / 1e6;

        let td_yz = measure(rounds, || {
            for _ in 0..iters {
                dec_yolow(&mut yz_dec, &comp, &mut out, raw.len());
                black_box(&out);
            }
        });
        let td_cz = measure(rounds, || {
            for _ in 0..iters {
                dec_c_zstd(&mut c_dec, &comp, &mut out, raw.len());
                black_box(&out);
            }
        });
        let td_rz = measure(rounds, || {
            for _ in 0..iters {
                dec_rust_zstd(&mut rz_dec, &comp, &mut out);
                black_box(&out);
            }
        });
        let td_sz = measure(rounds, || {
            for _ in 0..iters {
                dec_structured(&mut sz_dec, &comp, &mut out, raw.len());
                black_box(&out);
            }
        });

        let te_yz = measure(rounds, || {
            for _ in 0..iters {
                cbuf.clear();
                yz_comp.compress_into(raw, &mut cbuf, &yz_opts);
                black_box(&cbuf);
            }
        });
        let te_cz = measure(rounds, || {
            for _ in 0..iters {
                cbuf.resize(sz + 1024, 0);
                let n = c_comp.compress_to_buffer(raw, &mut cbuf).unwrap();
                black_box(n);
            }
        });
        let te_rz = measure(rounds, || {
            for _ in 0..iters {
                cbuf.clear();
                rz_comp.compress(raw, &mut cbuf);
                black_box(&cbuf);
            }
        });
        let te_sz = measure(rounds, || {
            for _ in 0..iters {
                cbuf.clear();
                sz_comp.compress_independent_frame_into(raw, &mut cbuf);
                black_box(&cbuf);
            }
        });

        println!(
            "{sz}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
            tot_mb / td_yz,
            tot_mb / td_cz,
            tot_mb / td_rz,
            tot_mb / td_sz,
            tot_mb / te_yz,
            tot_mb / te_cz,
            tot_mb / te_rz,
            tot_mb / te_sz,
        );
    }
}

fn read_vm_hwm_kib() -> usize {
    let s = fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            if let Some(tok) = rest.split_whitespace().next() {
                return tok.parse().unwrap_or(0);
            }
        }
    }
    0
}

fn cmd_rss_one(dir: &Path, mode: &str, lib: &str, file: &str, level: i32) {
    let raw = fs::read(dir.join(file)).expect("read file");
    let comp = zstd::bulk::compress(&raw, 3).unwrap();
    let before = read_vm_hwm_kib();
    let t0 = Instant::now();
    match (mode, lib) {
        ("dec", "yolow") => {
            let mut out = Vec::new();
            yolowzstd::decompress_into(&comp, &mut out, raw.len()).unwrap();
            black_box(out);
        }
        ("dec", "c_libzstd") => {
            let mut dec = zstd::bulk::Decompressor::new().unwrap();
            let mut out = vec![0u8; raw.len()];
            dec.decompress_to_buffer(&comp, &mut out).unwrap();
            black_box(out);
        }
        ("dec", "rust_zstd") => {
            let mut dec = rust_zstd::Decompressor::new();
            let mut out = Vec::new();
            dec.decompress_into(&comp, &mut out).unwrap();
            black_box(out);
        }
        ("dec", "zstd_pure_rs") => {
            let mut out = vec![0u8; raw.len()];
            let n = ZSTD_decompress(&mut out, &comp);
            assert!(!ERR_isError(n));
            black_box(out);
        }
        ("dec", "structured") => {
            let mut dec = structured_zstd::decoding::FrameDecoder::new();
            let mut out = Vec::with_capacity(raw.len());
            dec.decode_all_to_vec(&comp, &mut out).unwrap();
            black_box(out);
        }
        ("dec", "ruzstd") => {
            let mut dec = ruzstd::decoding::FrameDecoder::new();
            let mut out = Vec::with_capacity(raw.len());
            dec.decode_all_to_vec(&comp, &mut out).unwrap();
            black_box(out);
        }
        ("enc", "yolow") => {
            let z = yolowzstd::compress(&raw, level);
            black_box(z);
        }
        ("enc", "c_libzstd") => {
            let mut c = zstd::bulk::Compressor::new(level).unwrap();
            let mut out = vec![0u8; raw.len() + (raw.len() >> 7) + 4096];
            let n = c.compress_to_buffer(&raw, &mut out).unwrap();
            black_box(n);
        }
        ("enc", "rust_zstd") => {
            let opts = rust_zstd::CompressOptions {
                level,
                ..rust_zstd::CompressOptions::default()
            };
            let mut c = rust_zstd::Compressor::new(opts);
            let mut out = Vec::new();
            c.compress(&raw, &mut out);
            black_box(out);
        }
        ("enc", "zstd_pure_rs") => {
            let mut out = vec![0u8; raw.len() + (raw.len() >> 7) + 4096];
            let n = ZSTD_compress(&mut out, &raw, level);
            assert!(!ERR_isError(n));
            black_box(n);
        }
        ("enc", "structured") => {
            let mut c: structured_zstd::encoding::FrameCompressor =
                structured_zstd::encoding::FrameCompressor::new(
                    structured_zstd::encoding::CompressionLevel::from_level(level),
                );
            let mut out = Vec::new();
            c.compress_independent_frame_into(&raw, &mut out);
            black_box(out);
        }
        _ => panic!("unknown mode/lib: {mode} {lib}"),
    }
    let dt = t0.elapsed().as_secs_f64() * 1000.0;
    let after = read_vm_hwm_kib();
    println!(
        "{mode}\t{lib}\t{file}\tL{level}\tbefore_kib={before}\tafter_kib={after}\tdelta_kib={}\tms={dt:.2}",
        after.saturating_sub(before)
    );
}

fn c_zstd_mt_compress(raw: &[u8], level: i32, workers: u32, out: &mut Vec<u8>) {
    use std::io::Write;
    out.clear();
    let mut enc = zstd::stream::Encoder::new(out, level).unwrap();
    enc.multithread(workers).unwrap();
    enc.include_checksum(true).unwrap();
    enc.set_pledged_src_size(Some(raw.len() as u64)).unwrap();
    enc.write_all(raw).unwrap();
    enc.finish().unwrap();
}

fn cmd_mt(dir: &Path, level: i32, rounds: usize) {
    let files = load_silesia(dir);
    let mut yz_comp = yolowzstd::Compressor::new();
    let mut yz_dec = yolowzstd::Decompressor::new();
    let mut c_dec = zstd::bulk::Decompressor::new().unwrap();
    let mut out = Vec::with_capacity(64 << 20);
    let mut rec = Vec::with_capacity(64 << 20);

    let yz_auto_opts = yolowzstd::CompressOptions::new(level).with_threads(0);
    let yz_8_opts = yolowzstd::CompressOptions::new(level).with_threads(8);
    let yz_16_opts = yolowzstd::CompressOptions::new(level).with_threads(16);
    let yz_dec_auto = yolowzstd::DecompressOptions::new().with_threads(0);
    let yz_dec_8 = yolowzstd::DecompressOptions::new().with_threads(8);
    let yz_dec_16 = yolowzstd::DecompressOptions::new().with_threads(16);

    println!(
        "=== MULTI-CORE BENCH (level {level}, median of {rounds} rounds, whole-machine, no taskset) ==="
    );
    println!(
        "file\traw_b\tyz_auto_MBs\tyz_auto_rat\tyz_8t_MBs\tyz_16t_MBs\tc_8t_MBs\tc_8t_rat\tc_16t_MBs\tdec_yz_auto\tdec_yz_8t\tdec_yz_16t\tdec_c_1t"
    );

    let mut tot_bytes = 0usize;
    let mut sums = [0.0f64; 9];
    let mut comp_bytes = [0usize; 2];

    for (name, raw) in &files {
        let mb = raw.len() as f64 / 1e6;
        tot_bytes += raw.len();

        let t_ya = measure(rounds, || {
            out.clear();
            yz_comp.compress_into(raw, &mut out, &yz_auto_opts);
            black_box(&out);
        });
        let yz_comp_data = out.clone();
        comp_bytes[0] += yz_comp_data.len();

        let t_y8 = measure(rounds, || {
            out.clear();
            yz_comp.compress_into(raw, &mut out, &yz_8_opts);
            black_box(&out);
        });

        let t_y16 = measure(rounds, || {
            out.clear();
            yz_comp.compress_into(raw, &mut out, &yz_16_opts);
            black_box(&out);
        });

        let t_c8 = measure(rounds, || {
            c_zstd_mt_compress(raw, level, 8, &mut out);
            black_box(&out);
        });
        let c8_len = out.len();
        comp_bytes[1] += c8_len;

        let t_c16 = measure(rounds, || {
            c_zstd_mt_compress(raw, level, 16, &mut out);
            black_box(&out);
        });

        let t_da = measure(rounds, || {
            rec.clear();
            yz_dec
                .decompress_into_with(&yz_comp_data, &mut rec, &yz_dec_auto, raw.len())
                .unwrap();
            black_box(&rec);
        });
        assert_eq!(rec.as_slice(), raw.as_slice());

        let t_d8 = measure(rounds, || {
            rec.clear();
            yz_dec
                .decompress_into_with(&yz_comp_data, &mut rec, &yz_dec_8, raw.len())
                .unwrap();
            black_box(&rec);
        });

        let t_d16 = measure(rounds, || {
            rec.clear();
            yz_dec
                .decompress_into_with(&yz_comp_data, &mut rec, &yz_dec_16, raw.len())
                .unwrap();
            black_box(&rec);
        });

        let t_dc = measure(rounds, || {
            dec_c_zstd(&mut c_dec, &yz_comp_data, &mut rec, raw.len());
            black_box(&rec);
        });
        assert_eq!(rec.as_slice(), raw.as_slice());

        let ts = [t_ya, t_y8, t_y16, t_c8, t_c16, t_da, t_d8, t_d16, t_dc];
        for (s, t) in sums.iter_mut().zip(ts.iter()) {
            *s += *t;
        }
        println!(
            "{name}\t{}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
            raw.len(),
            mb / t_ya,
            raw.len() as f64 / yz_comp_data.len() as f64,
            mb / t_y8,
            mb / t_y16,
            mb / t_c8,
            raw.len() as f64 / c8_len.max(1) as f64,
            mb / t_c16,
            mb / t_da,
            mb / t_d8,
            mb / t_d16,
            mb / t_dc,
        );
    }
    let tot_mb = tot_bytes as f64 / 1e6;
    println!(
        "TOTAL\t{tot_bytes}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.1}\t{:.2}x\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
        tot_mb / sums[0],
        tot_bytes as f64 / comp_bytes[0] as f64,
        tot_mb / sums[1],
        tot_mb / sums[2],
        tot_mb / sums[3],
        tot_bytes as f64 / comp_bytes[1] as f64,
        tot_mb / sums[4],
        tot_mb / sums[5],
        tot_mb / sums[6],
        tot_mb / sums[7],
        tot_mb / sums[8],
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "smoke".to_string());
    if cmd == "rss_one" {
        let dir_s = args.next().unwrap_or_else(|| "../corpus/silesia".to_string());
        let mode = args.next().expect("mode");
        let lib = args.next().expect("lib");
        let file = args.next().unwrap_or_else(|| "mozilla".to_string());
        let level = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
        cmd_rss_one(Path::new(&dir_s), &mode, &lib, &file, level);
        return;
    }
    let dir_s = args
        .next()
        .unwrap_or_else(|| "../corpus/silesia".to_string());
    let dir = Path::new(&dir_s);
    match cmd.as_str() {
        "smoke" => cmd_smoke(dir),
        "decode" => {
            let level = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
            let rounds = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
            cmd_decode(dir, level, rounds);
        }
        "encode" => {
            let level = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
            let rounds = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
            cmd_encode(dir, level, rounds);
        }
        "cross" => {
            let rounds = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
            cmd_cross(dir, rounds);
        }
        "short" => {
            let rounds = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
            cmd_short(dir, rounds);
        }
        "mt" => {
            let level = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
            let rounds = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
            cmd_mt(dir, level, rounds);
        }
        _ => panic!("unknown cmd: {cmd}"),
    }
}
