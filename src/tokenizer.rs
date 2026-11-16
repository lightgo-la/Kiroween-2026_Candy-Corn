//! tiktoken-rs(o200k_base) 기반 토큰 수 / 비용 계산.
//! 다른 벤더 모델의 실제 토큰 수와는 차이가 있을 수 있는 근사치다.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const SAMPLE_BYTES: usize = 1_000_000;

pub fn count(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton().encode_ordinary(text).len()
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
    // UTF-8이 아니면 CP949로 디코딩해야 한글 토큰 수가 왜곡되지 않는다.
    let text = if crate::profiler::csv_profiler::looks_utf8(&buf) {
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        encoding_rs::EUC_KR.decode(&buf).0.into_owned()
    };
    let tokens = count(&text);
    Ok((tokens as f64 * total as f64 / buf.len() as f64).round() as usize)
}

pub fn cost_usd(tokens: usize, usd_per_million: f64) -> f64 {
    tokens as f64 * usd_per_million / 1_000_000.0
}
