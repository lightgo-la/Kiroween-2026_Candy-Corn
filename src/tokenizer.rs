//! Token counts and cost based on tiktoken-rs (o200k_base).
//! This is an approximation that can differ from the real token counts of other vendors' models.

use crate::profiler::csv_profiler::io_error_text;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const SAMPLE_BYTES: usize = 1_000_000;

pub fn count(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton().encode_ordinary(text).len()
}

/// Loads the tokenizer (o200k_base) vocabulary ahead of time. The first load takes ~85 ms, so the app calls this on a
/// background thread at startup and the first compile does not have to wait.
pub fn warm_up() {
    let _ = count("warm up");
}

/// Estimates the token count of a raw file: tokenizes only the first 1 MB and scales by the file size.
pub fn estimate_file_tokens(path: &Path) -> Result<usize, String> {
    let read_err = |e: std::io::Error| trf!("cannot read file: {}", io_error_text(&e));
    let file = File::open(path).map_err(read_err)?;
    let total = file.metadata().map_err(read_err)?.len() as usize;
    let mut buf = Vec::with_capacity(SAMPLE_BYTES.min(total));
    file.take(SAMPLE_BYTES as u64).read_to_end(&mut buf).map_err(read_err)?;
    if buf.is_empty() {
        return Ok(0);
    }
    // Decoding with the same decision as the profiler (the BOM's encoding if there is one, else UTF-8/CP949) keeps
    // the token count of Korean text from being distorted. Reading a whole UTF-8 file as CP949 because of one broken
    // byte multiplies the token count.
    let enc = crate::profiler::csv_profiler::detect_text_encoding(&buf, buf.len() >= total);
    let tokens = count(&enc.decode_with_bom_removal(&buf).0);
    Ok((tokens as f64 * total as f64 / buf.len() as f64).round() as usize)
}

pub fn cost_usd(tokens: usize, usd_per_million: f64) -> f64 {
    tokens as f64 * usd_per_million / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_bom_file_is_decoded_before_counting() {
        let text = ko_fixture!("core.tokenizer.small_csv");
        let utf8 = std::env::temp_dir().join(format!("cc_tok_u8_{}.csv", std::process::id()));
        let utf16 = std::env::temp_dir().join(format!("cc_tok_u16_{}.csv", std::process::id()));
        std::fs::write(&utf8, text).unwrap();
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&utf16, &bytes).unwrap();
        let (a, b) = (
            estimate_file_tokens(&utf8).unwrap(),
            estimate_file_tokens(&utf16).unwrap(),
        );
        let _ = (std::fs::remove_file(utf8), std::fs::remove_file(utf16));
        // A file smaller than 1 MB is counted in full, so the same content gives the same token count in any encoding.
        assert!(a > 0);
        assert_eq!(a, b);
    }

    /// Review repro: with one 0xFF byte in a 400-row UTF-8 Korean CSV, the profile was fine but the raw token estimate
    /// (the pre-compression baseline of requirement 7) read the whole file as CP949 and jumped from 10,456 to 28,864.
    /// With the same decision as the profiler it may differ only by the one changed character.
    #[test]
    fn one_bad_byte_does_not_inflate_the_token_baseline() {
        let (region, memo) = (
            ko_fixture!("core.korean_csv.region"),
            ko_fixture!("core.korean_csv.memo"),
        );
        let mut text = format!("{}\n", ko_fixture!("core.korean_csv.header"));
        for i in 1..=400 {
            text += &format!("{i},{region},{},{memo} {i}\n", 1000 + i * 7);
        }
        let mut damaged = text.clone().into_bytes();
        damaged.insert(text.find("\n1,").unwrap() + 1, 0xFF);
        let clean = std::env::temp_dir().join(format!("cc_tok_clean_{}.csv", std::process::id()));
        let bad = std::env::temp_dir().join(format!("cc_tok_bad_{}.csv", std::process::id()));
        std::fs::write(&clean, &text).unwrap();
        std::fs::write(&bad, &damaged).unwrap();
        let (a, b) = (
            estimate_file_tokens(&clean).unwrap(),
            estimate_file_tokens(&bad).unwrap(),
        );
        let profiled = crate::profiler::csv_profiler::profile_file(&bad, 0).map(|d| d.columns.len());
        let _ = (std::fs::remove_file(clean), std::fs::remove_file(bad));
        assert_eq!(profiled, Ok(4), "the profiler reads it as UTF-8");
        assert!(a > 5000, "{a}");
        assert!(
            b.abs_diff(a) <= 4,
            "one broken byte moved the token estimate {a} -> {b}"
        );
    }

    #[test]
    fn read_errors_follow_the_language() {
        use crate::i18n::{Lang, with_lang};
        let p = std::env::temp_dir().join(format!("cc_tok_missing_{}.csv", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert_eq!(
            with_lang(Lang::En, || estimate_file_tokens(&p)),
            Err("cannot read file: no such file or directory (os error 2)".to_string())
        );
        let os = File::open(&p).unwrap_err();
        assert_eq!(
            with_lang(Lang::Ko, || estimate_file_tokens(&p)),
            Err(format!(ko_text!("cannot read file: {}"), os))
        );
    }
}
