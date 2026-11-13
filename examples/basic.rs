use yolowzstd::{compress, decompress, decompress_into};

fn main() -> Result<(), yolowzstd::Error> {
    let input =
        b"yolowzstd: pure-Rust RFC 8878 Zstandard compression and decompression! ".repeat(64);
    let compressed = compress(&input, 3);
    let decompressed = decompress(&compressed, input.len())?;
    assert_eq!(decompressed, input);

    let mut reused = Vec::new();
    decompress_into(&compressed, &mut reused, input.len())?;
    assert_eq!(reused, input);

    println!(
        "ok: {} bytes -> {} bytes ({:.2}x)",
        input.len(),
        compressed.len(),
        input.len() as f64 / compressed.len() as f64
    );
    Ok(())
}
