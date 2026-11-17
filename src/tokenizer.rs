//! tiktoken-rs(o200k_base) 기반 토큰 수 / 비용 계산.
//! 다른 벤더 모델의 실제 토큰 수와는 차이가 있을 수 있는 근사치다.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const SAMPLE_BYTES: usize = 1_000_000;

pub fn count(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton().encode_ordinary(text).len()
}

/// 토크나이저(o200k_base) 사전을 미리 불러 둔다. 처음 한 번 ~85ms가 걸리므로
/// 앱 시작 때 백그라운드 스레드에서 호출해 첫 컴파일이 기다리지 않게 한다.
pub fn warm_up() {
    let _ = count("warm up");
}

/// 원본 파일의 토큰 수 추정. 앞 1MB만 토크나이즈하고 파일 크기 비율로 환산한다.
pub fn estimate_file_tokens(path: &Path) -> Result<usize, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let total = file.metadata().map_err(|e| e.to_string())?.len() as usize;
    let mut buf = Vec::with_capacity(SAMPLE_BYTES.min(total));
    file.take(SAMPLE_BYTES as u64)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Ok(0);
    }
    // 프로파일러와 같은 판정(BOM이 있으면 그 인코딩, 없으면 UTF-8/CP949)으로 디코딩해야 한글 토큰 수가 왜곡되지 않는다.
    // 깨진 바이트 하나 때문에 UTF-8 파일 전체를 CP949로 읽으면 토큰 수가 몇 배로 뛴다.
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
        let text = "id,이름\n1,가나다\n2,라마바\n";
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
        // 1MB보다 작은 파일은 전체를 세므로, 같은 내용이면 인코딩과 관계없이 토큰 수가 같다.
        assert!(a > 0);
        assert_eq!(a, b);
    }

    /// 리뷰 재현: 400행 UTF-8 한글 CSV에 0xFF 한 바이트가 섞이면 프로파일은 멀쩡한데 원본 토큰 추정(요구사항 7의
    /// 압축 전 기준선)이 파일 전체를 CP949로 읽어 10,456에서 28,864로 뛰었다. 프로파일러와 같은 판정을 쓰므로
    /// 바뀐 한 글자만큼만 달라야 한다.
    #[test]
    fn one_bad_byte_does_not_inflate_the_token_baseline() {
        let mut text = String::from("id,지역,금액,메모\n");
        for i in 1..=400 {
            text += &format!("{i},서울특별시 강남구,{},정상 거래 확인 완료 메모 {i}\n", 1000 + i * 7);
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
        assert_eq!(profiled, Ok(4), "프로파일러는 UTF-8로 읽는다");
        assert!(a > 5000, "{a}");
        assert!(b.abs_diff(a) <= 4, "깨진 바이트 하나로 토큰 추정이 {a} -> {b}");
    }
}
