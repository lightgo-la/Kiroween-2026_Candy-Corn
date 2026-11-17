//! UI 언어(한국어/영어).
//!
//! 문자열은 호출 위치에 두 언어를 함께 적는다.
//! - 고정 문구: `tr!("로드 완료", "Loaded")` → `&'static str`
//! - 서식 문구: `trf!("{n}개 제약", "{n} constraints")` → `String` (`format!`과 같은 인자, 변수 직접 참조도 된다)
//!
//! 현재 언어는 프로세스 전역 값이고(시작할 때 `--lang` > 저장된 설정 > 한국어 순으로 정한다),
//! 테스트는 [`with_lang`]으로 그 스레드에서만 바꾼다(병렬 테스트끼리 전역 값을 건드리지 않게).

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Ko,
    En,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::Ko, Lang::En];

    pub fn code(self) -> &'static str {
        match self {
            Lang::Ko => "ko",
            Lang::En => "en",
        }
    }

    /// 언어 선택 목록에 보여 줄 이름 (각 언어로 적는다)
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::Ko => "한국어", // i18n-data
            Lang::En => "English",
        }
    }

    pub fn parse(s: &str) -> Option<Lang> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ko" | "kr" | "kor" | "korean" | "ko-kr" => Some(Lang::Ko),
            "en" | "eng" | "english" | "en-us" | "en-gb" => Some(Lang::En),
            _ if s.trim() == "한국어" => Some(Lang::Ko), // i18n-data
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Lang::Ko => 0,
            Lang::En => 1,
        }
    }

    fn from_u8(v: u8) -> Lang {
        if v == 1 { Lang::En } else { Lang::Ko }
    }
}

static GLOBAL: AtomicU8 = AtomicU8::new(0);

thread_local! {
    static OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// 지금 쓰는 언어 (이 스레드의 테스트용 덮어쓰기가 있으면 그것).
pub fn current() -> Lang {
    OVERRIDE
        .with(Cell::get)
        .unwrap_or_else(|| Lang::from_u8(GLOBAL.load(Ordering::Relaxed)))
}

/// 프로세스 전체 언어를 바꾼다. 앱 시작과 사용자의 언어 전환에서만 부른다(테스트는 [`with_lang`]).
pub fn set(lang: Lang) {
    GLOBAL.store(lang.to_u8(), Ordering::Relaxed);
}

pub fn is_en() -> bool {
    current() == Lang::En
}

/// 현재 언어에 맞는 쪽을 고른다. `tr!`가 쓴다.
pub fn pick<T>(ko: T, en: T) -> T {
    if is_en() { en } else { ko }
}

/// 상수 표(프리셋 이름, 템플릿, 테마 이름 등)에 넣는 두 언어 문구. 쓸 때 [`Tr::get`]으로 고른다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tr(pub &'static str, pub &'static str);

impl Tr {
    pub fn get(self) -> &'static str {
        pick(self.0, self.1)
    }

    /// 두 언어 문구를 함께 검사하는 테스트용 (화면은 `get`을 쓴다)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ko(self) -> &'static str {
        self.0
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn en(self) -> &'static str {
        self.1
    }
}

/// 이 스레드에서만 언어를 바꿔 `f`를 실행하고 원래대로 되돌린다(패닉이 나도 되돌린다).
#[cfg_attr(not(test), allow(dead_code))]
pub fn with_lang<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Lang>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(self.0));
        }
    }
    let _restore = Restore(OVERRIDE.with(|o| o.replace(Some(lang))));
    f()
}

/// 고정 문구: `tr!("한국어", "English")` → `&'static str`. 자리표시자(`{}`)가 있으면 `trf!`를 쓴다.
macro_rules! tr {
    ($ko:literal, $en:literal $(,)?) => {
        $crate::i18n::pick::<&'static str>($ko, $en)
    };
}

/// 서식 문구: `trf!("{n}개", "{n} items")`, `trf!("{}행", "{} rows", n)` → `String`.
/// 두 문구가 같은 인자를 써야 한다(어순이 다르면 `{0}`/`{1}`이나 변수 이름으로 적는다).
macro_rules! trf {
    ($ko:literal, $en:literal $(, $($arg:tt)*)?) => {
        if $crate::i18n::is_en() {
            format!($en $(, $($arg)*)?)
        } else {
            format!($ko $(, $($arg)*)?)
        }
    };
}

// ---------- 설정 파일 ----------

/// 사용자 설정 파일 위치. `CANDY_CORN_SETTINGS`가 있으면 그 경로(빈 값이면 설정 파일을 쓰지 않음),
/// 없으면 `%APPDATA%\Candy-Corn\settings.json`.
pub fn settings_path() -> Option<PathBuf> {
    match std::env::var_os("CANDY_CORN_SETTINGS") {
        Some(p) if p.is_empty() => None,
        Some(p) => Some(PathBuf::from(p)),
        None => std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Candy-Corn").join("settings.json")),
    }
}

/// 설정 파일에 저장된 언어. 파일이 없거나 깨졌으면 None.
pub fn load_saved(path: &Path) -> Option<Lang> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    Lang::parse(v.get("lang")?.as_str()?)
}

/// 언어를 설정 파일에 저장한다. 다른 설정 키는 그대로 두고, 임시 파일에 쓴 뒤 바꿔 끼운다.
pub fn save(path: &Path, lang: Lang) -> Result<(), String> {
    let mut v = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t.trim_start_matches('\u{feff}')).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    v["lang"] = serde_json::json!(lang.code());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// 기본 위치의 설정 파일에 언어를 저장한다(설정 파일을 쓰지 않는 환경이면 아무것도 하지 않는다).
pub fn save_default(lang: Lang) -> Result<(), String> {
    match settings_path() {
        Some(p) => save(&p, lang),
        None => Ok(()),
    }
}

/// 명령줄에서 `--lang X` / `--lang=X`를 미리 읽는다(clap 도움말을 그 언어로 만들기 위해 파싱 전에 필요).
/// 값이 잘못됐으면 None을 돌려주고, 오류는 clap이 알린다.
pub fn lang_arg(args: &[OsString]) -> Option<Lang> {
    let mut it = args.iter().skip(1).map(|a| a.to_string_lossy());
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if a == "--lang" {
            return it.next().and_then(|v| Lang::parse(&v));
        }
        if let Some(v) = a.strip_prefix("--lang=") {
            return Lang::parse(v);
        }
    }
    None
}

/// 시작 언어: 명령줄 `--lang` > 저장된 설정 > 한국어.
pub fn startup_lang(arg: Option<Lang>) -> Lang {
    arg.or_else(|| settings_path().and_then(|p| load_saved(&p)))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tr_and_trf_follow_the_thread_language() {
        let n = 3;
        let rows = 7;
        with_lang(Lang::Ko, || {
            assert_eq!(tr!("로드 완료", "Loaded"), "로드 완료");
            assert_eq!(trf!("{n}개 제약", "{n} constraints"), "3개 제약");
            assert_eq!(trf!("{0}행 중 {1}", "{1} of {0} rows", rows, n), "7행 중 3");
        });
        with_lang(Lang::En, || {
            assert_eq!(tr!("로드 완료", "Loaded"), "Loaded");
            assert_eq!(trf!("{n}개 제약", "{n} constraints"), "3 constraints");
            assert_eq!(trf!("{0}행 중 {1}", "{1} of {0} rows", rows, n), "3 of 7 rows");
            // 중첩해도 안쪽이 끝나면 바깥 언어로 돌아온다
            with_lang(Lang::Ko, || assert!(!is_en()));
            assert!(is_en());
        });
    }

    #[test]
    fn with_lang_restores_after_panic() {
        let before = current();
        let r = std::panic::catch_unwind(|| with_lang(Lang::En, || panic!("x")));
        assert!(r.is_err());
        assert_eq!(current(), before);
    }

    #[test]
    fn tr_pairs_pick_by_language() {
        const T: Tr = Tr("저장", "Save");
        assert_eq!(with_lang(Lang::Ko, || T.get()), "저장");
        assert_eq!(with_lang(Lang::En, || T.get()), "Save");
        assert_eq!((T.ko(), T.en()), ("저장", "Save"));
    }

    #[test]
    fn parse_codes_and_names() {
        for (s, want) in [
            ("ko", Some(Lang::Ko)),
            ("KO", Some(Lang::Ko)),
            ("한국어", Some(Lang::Ko)),
            ("en", Some(Lang::En)),
            ("English", Some(Lang::En)),
            ("en-US", Some(Lang::En)),
            ("fr", None),
        ] {
            assert_eq!(Lang::parse(s), want, "{s}");
        }
        for l in Lang::ALL {
            assert_eq!(Lang::parse(l.code()), Some(l));
        }
    }

    #[test]
    fn lang_arg_reads_both_forms() {
        let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(lang_arg(&a(&["exe", "--cli", "--lang", "en"])), Some(Lang::En));
        assert_eq!(lang_arg(&a(&["exe", "--lang=ko", "--cli"])), Some(Lang::Ko));
        assert_eq!(lang_arg(&a(&["exe", "--lang", "xx"])), None);
        assert_eq!(lang_arg(&a(&["exe", "--cli"])), None);
        // `--` 뒤는 옵션이 아니다
        assert_eq!(lang_arg(&a(&["exe", "--", "--lang", "en"])), None);
    }

    #[test]
    fn settings_round_trip_keeps_other_keys() {
        let dir = std::env::temp_dir().join(format!("cc_i18n_{}", std::process::id()));
        let path = dir.join("sub").join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_saved(&path), None);
        save(&path, Lang::En).unwrap();
        assert_eq!(load_saved(&path), Some(Lang::En));
        // 다른 키(예: 나중에 생길 설정)는 보존하고, BOM이 있어도 읽는다
        std::fs::write(&path, "\u{feff}{\"lang\":\"en\",\"other\":1}").unwrap();
        save(&path, Lang::Ko).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((v["lang"].as_str(), v["other"].as_i64()), (Some("ko"), Some(1)));
        // 깨진 파일은 무시하고 새로 쓴다
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_saved(&path), None);
        save(&path, Lang::En).unwrap();
        assert_eq!(load_saved(&path), Some(Lang::En));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 번역 누락 검사: 소스의 한국어 문자열 리터럴이 모두 `tr!`/`trf!`(또는 `pick(`)의 첫 인자인지 본다.
///
/// 검사하지 않는 것: 주석, 속성(`#[...]`, clap 도움말은 main.rs `EN_HELP`가 따로 번역), `#[cfg(test)]` 이후의 테스트 모듈,
/// `.expect(`/`panic!(` 같은 개발자용 문구, 그리고 `i18n-data` 표시가 붙은 곳(린터 어휘처럼 번역 대상이 아닌 한국어 데이터).
/// - 줄 끝 주석 `// i18n-data`: 그 줄의 리터럴
/// - 단독 주석 줄 `// i18n-data`: 바로 다음 항목(`;` 또는 닫는 `}`까지)
#[cfg(test)]
pub(crate) mod coverage {
    use std::path::{Path, PathBuf};

    fn is_hangul(c: char) -> bool {
        matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
    }

    const ALLOWED_BEFORE: &[&str] = &["tr!(", "trf!(", "pick(", "Tr(", "expect(", "panic!(", "unreachable!("];

    /// 번역되지 않은 한국어 리터럴의 (줄 번호, 내용).
    pub fn untranslated(src: &str) -> Vec<(usize, String)> {
        let lines: Vec<&str> = src.lines().collect();
        let marker_line = |n: usize| lines.get(n - 1).is_some_and(|l| l.contains("i18n-data"));
        let own_line_marker = |n: usize| {
            lines
                .get(n - 1)
                .is_some_and(|l| l.trim_start().starts_with("//") && l.contains("i18n-data"))
        };
        let chars: Vec<char> = src.chars().collect();
        let mut out = vec![];
        let (mut i, mut line) = (0usize, 1usize);
        let mut depth: i32 = 0;
        let mut recent = String::new(); // 최근 코드 글자(공백 제외)
        let mut attr_depth: Option<i32> = None; // #[...] 안
        let mut pending_item = false; // 단독 표시 줄 다음 항목을 기다리는 중
        let mut item_base: Option<(i32, bool)> = None; // (시작 깊이, 더 깊이 들어갔는지)
        let push_code = |recent: &mut String, c: char| {
            recent.push(c);
            if recent.len() > 48 {
                let cut = recent
                    .char_indices()
                    .nth(recent.chars().count() - 24)
                    .map_or(0, |(k, _)| k);
                recent.drain(..cut);
            }
        };
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            // 주석
            if c == '/' && next == Some('/') {
                if own_line_marker(line) {
                    pending_item = true;
                }
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            if c == '/' && next == Some('*') {
                let mut d = 0;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        d += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        d -= 1;
                        i += 2;
                        if d == 0 {
                            break;
                        }
                    } else {
                        if chars[i] == '\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
                continue;
            }
            if c == '\n' {
                line += 1;
                i += 1;
                continue;
            }
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            // 테스트 모듈부터는 보지 않는다
            if c == '#' && chars[i..].iter().take(12).collect::<String>() == "#[cfg(test)]" {
                let rest: String = chars[i + 12..].iter().take(200).collect();
                if rest.trim_start().starts_with("mod ") || rest.trim_start().starts_with("pub mod ") {
                    break;
                }
                pending_item = true; // 테스트 전용 항목 하나는 건너뛴다
            }
            // 속성
            if c == '#'
                && (next == Some('[') || (next == Some('!') && chars.get(i + 2) == Some(&'[')))
                && attr_depth.is_none()
            {
                attr_depth = Some(depth);
            }
            // 문자열 리터럴 (r"", r#""#, b"" 포함)
            let prev_ident = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
            let raw_start = (c == 'r' || (c == 'b' && next == Some('r'))) && !prev_ident && {
                let mut k = i + if c == 'b' { 2 } else { 1 };
                while chars.get(k) == Some(&'#') {
                    k += 1;
                }
                chars.get(k) == Some(&'"')
            };
            if c == '"' || raw_start || (c == 'b' && next == Some('"') && !prev_ident) {
                let start_line = line;
                let mut k = i;
                let byte = c == 'b';
                if c == 'b' {
                    k += 1;
                }
                let mut hashes = 0;
                let raw = chars[k] == 'r';
                if raw {
                    k += 1;
                    while chars[k] == '#' {
                        hashes += 1;
                        k += 1;
                    }
                }
                k += 1; // 여는 따옴표
                let mut text = String::new();
                while k < chars.len() {
                    let ch = chars[k];
                    if ch == '\n' {
                        line += 1;
                    }
                    if !raw && ch == '\\' {
                        text.push(ch);
                        if let Some(&e) = chars.get(k + 1) {
                            if e == '\n' {
                                line += 1;
                            }
                            text.push(e);
                        }
                        k += 2;
                        continue;
                    }
                    if ch == '"' && (0..hashes).all(|h| chars.get(k + 1 + h) == Some(&'#')) {
                        k += 1 + hashes;
                        break;
                    }
                    text.push(ch);
                    k += 1;
                }
                let exempt = byte
                    || attr_depth.is_some()
                    || item_base.is_some()
                    || marker_line(start_line)
                    || ALLOWED_BEFORE.iter().any(|a| recent.ends_with(a));
                if !exempt && text.chars().any(is_hangul) {
                    out.push((start_line, text));
                }
                if pending_item && item_base.is_none() {
                    // 표시 줄 다음 항목이 리터럴로 시작하는 경우는 없지만 안전하게 처리
                    pending_item = false;
                }
                push_code(&mut recent, '"');
                i = k;
                continue;
            }
            // 문자 리터럴과 수명('a)
            if c == '\'' {
                if next == Some('\\') {
                    let mut k = i + 2;
                    while k < chars.len() && chars[k] != '\'' {
                        k += 1;
                    }
                    i = k + 1;
                    push_code(&mut recent, '\'');
                    continue;
                }
                if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                    push_code(&mut recent, '\'');
                    continue;
                }
            }
            // 일반 코드 글자
            if pending_item && item_base.is_none() {
                item_base = Some((depth, false));
                pending_item = false;
            }
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ => {}
            }
            if let Some(a) = attr_depth
                && c == ']'
                && depth == a
            {
                attr_depth = None;
            }
            if let Some((base, deeper)) = item_base {
                if depth > base {
                    item_base = Some((base, true));
                } else if (c == ';' && depth == base) || (c == '}' && deeper && depth == base) {
                    item_base = None;
                }
            }
            push_code(&mut recent, c);
            i += 1;
        }
        out
    }

    /// 검사할 소스 파일 (테스트 전용 모듈 qc.rs 제외).
    pub fn source_files() -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && p.file_name().is_some_and(|n| n != "qc.rs") {
                    out.push(p);
                }
            }
        }
        let mut v = vec![];
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut v);
        v.sort();
        v
    }

    /// 파일별 누락 목록 (검사할 파일 이름에 `only`가 들어간 것만, 빈 문자열이면 전체).
    pub fn report(only: &[&str]) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = vec![];
        for p in source_files() {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if !only.is_empty() && !only.iter().any(|o| rel.contains(o)) {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap_or_default();
            for (line, text) in untranslated(&src) {
                out.push(format!("{rel}:{line}: \"{text}\""));
            }
        }
        out
    }

    #[test]
    fn scanner_rules() {
        let src = r##"
fn a() {
    let x = tr!("고정", "Fixed");
    let y = trf!(
        "{n}개",
        "{n} items"
    );
    let z = "번역 안 됨";
    let w = "섞임"; // i18n-data
    let e = s.expect("개발자용");
    let c = '않';
    let r = r#"원시 "문자열""#;
    let b = b"bytes";
    const PAIR: Tr = Tr("짝", "Pair");
}
#[arg(help = "속성 안")]
// i18n-data
const VOCAB: &[&str] = &[
    "어휘",
    "목록",
];
const AFTER: &str = "표시 뒤 항목";
fn label<'a>(s: &'a str) -> &'a str { s }
#[cfg(test)]
mod tests {
    const T: &str = "테스트";
}
"##;
        let got: Vec<String> = untranslated(src).into_iter().map(|(_, t)| t).collect();
        assert_eq!(got, vec!["번역 안 됨", "원시 \"문자열\"", "표시 뒤 항목"]);
    }

    /// 전체 소스 검사: 사용자에게 보이는 한국어 문자열에 영어가 함께 있는지 본다(cargo test에 포함).
    /// `I18N_ONLY=app.rs,studio_canvas.rs`처럼 파일 이름 일부를 쉼표로 주면 그 파일만 본다.
    #[test]
    fn all_user_facing_korean_is_translated() {
        let only = std::env::var("I18N_ONLY").unwrap_or_default();
        let only: Vec<&str> = only.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        let missing = report(&only);
        let mut per_file: std::collections::BTreeMap<&str, usize> = Default::default();
        for m in &missing {
            *per_file.entry(m.split(':').next().unwrap_or("")).or_default() += 1;
        }
        let summary: Vec<String> = per_file.iter().map(|(f, n)| format!("  {n:4} {f}")).collect();
        assert!(
            missing.is_empty(),
            "번역 누락 {}건\n{}\n{}",
            missing.len(),
            summary.join("\n"),
            missing.iter().take(80).cloned().collect::<Vec<_>>().join("\n")
        );
    }
}
