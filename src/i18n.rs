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
            Lang::Ko => "한국어",
            Lang::En => "English",
        }
    }

    pub fn parse(s: &str) -> Option<Lang> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ko" | "kr" | "kor" | "korean" | "ko-kr" => Some(Lang::Ko),
            "en" | "eng" | "english" | "en-us" | "en-gb" => Some(Lang::En),
            _ if s.trim() == "한국어" => Some(Lang::Ko),
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
#[allow(dead_code)]
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
