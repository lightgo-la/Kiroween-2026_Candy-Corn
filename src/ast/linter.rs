//! Visitor 패턴 기반 린트 패스: 수식어 압축, 중복 제거, 모순 제약 탐지, 스키마 무결성 검증.
//!
//! 모순 판정은 휴리스틱이다. 문장을 단어·숫자·연산자·따옴표 값 토큰으로 나누고, 부정 표지는 단어 단위로 찾으며,
//! 한국어 단어는 조사·어미를 떼어 비교한다("탐지해"/"탐지하지" → "탐지", "주문금액"은 그대로).
//! 대상을 설명하는 부정은 금지가 아니다: 관계절("customers who never purchased"), 부정사("ways to avoid churn"),
//! 하이픈 복합어("never-paid invoices"), 관형형("구매하지 않은 고객"), 목적절("이탈하지 않도록 할 방안").
//! - 극성(긍정/부정)이 반대이고 부정 쪽 내용 단어가 모두 긍정 쪽에 있으면(부정이 긍정 지시를 덮으면) 모순이다.
//!   "Detect outliers" ↔ "Never detect outliers in the id column"처럼 부정이 더 좁으면 둘 다 둔다.
//! - 극성이 같고 지시형 반의어(제외/포함, 허용/불허, 오름차순/내림차순)만 반대이며 나머지 내용이 같아도 모순이다.
//!   증가/감소처럼 데이터를 설명할 때도 쓰는 반의어는 확신할 수 없으므로 경고만 한다.
//!
//! 확실한 모순이면 앞의 제약을 지운다(나중 지시 우선). 확신할 수 없는 쌍은 둘 다 두고 경고만 남긴다.

use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};

pub trait AstVisitor {
    fn visit_role(&mut self, _role: &mut RoleNode) {}
    fn visit_constraints(&mut self, _constraints: &mut Vec<ConstraintNode>) {}
    fn visit_output(&mut self, _output: &mut OutputSchemaAst) {}
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic>;
}

pub fn walk(ast: &mut RootPromptAst, v: &mut dyn AstVisitor) {
    v.visit_role(&mut ast.role);
    v.visit_constraints(&mut ast.constraints);
    v.visit_output(&mut ast.output_schema);
    ast.lint_warnings.extend(v.take_diagnostics());
}

/// 모든 패스를 순서대로 실행한다.
pub fn lint(ast: &mut RootPromptAst) {
    walk(ast, &mut FillerPass::default());
    walk(ast, &mut DedupPass::default());
    walk(ast, &mut ContradictionPass::default());
    walk(ast, &mut SchemaPass::default());
}

// ---------- 어휘 ----------

const FILLERS: [&str; 12] = [
    "very",
    "really",
    "extremely",
    "highly",
    "please",
    "kindly",
    "simply",
    "매우",
    "정말",
    "아주",
    "굉장히",
    "제발",
];

/// 따옴표 문자. 이런 문자가 붙은 단어는 수식어여도 지우지 않는다.
const QUOTE_CHARS: [char; 17] = [
    '\'', '"', '“', '”', '‘', '’', '`', '「', '」', '『', '』', '《', '》', '〈', '〉', '«', '»',
];

/// 여는 문자와 닫는 문자가 다른 따옴표·괄호 따옴표. 아포스트로피로 쓰이지 않으므로 짝만 맞으면 닫힌다.
const PAIRED_QUOTES: [(char, char); 6] = [
    ('“', '”'),
    ('「', '」'),
    ('『', '』'),
    ('《', '》'),
    ('〈', '〉'),
    ('«', '»'),
];

/// 영어 부정 표현. 단어 단위로 맞추므로 "can treat"(can t), "nevertheless"(never), "do note"(do not)는 부정이 아니다.
/// 아포스트로피는 단어를 나누므로 "don't"는 ["don", "t"]가 된다.
const NEG_PHRASES: [&[&str]; 17] = [
    &["do", "not"],
    &["don", "t"],
    &["dont"],
    &["does", "not"],
    &["doesn", "t"],
    &["never"],
    &["avoid"],
    &["must", "not"],
    &["mustn", "t"],
    &["should", "not"],
    &["shouldn", "t"],
    &["cannot"],
    &["can", "not"],
    &["can", "t"],
    &["won", "t"],
    &["will", "not"],
    &["shall", "not"],
];

/// "-지 마/말" 금지 명령(긴 것부터). 앞 단어가 "지"로 끝나거나(탐지하지 마) 붙여 쓰면(하지마라) 부정이다.
#[rustfmt::skip]
const KO_NEG_IMPERATIVES: &[&str] = &[
    "마십시오", "마세요", "마시오", "말아라", "말아요", "맙시다", "마라", "마요", "말고", "말라", "말아", "말것",
    "말자", "말기", "말길", "마", "말",
];

/// 내용 비교에서 빼는 기능어(영어 불용어, 한국어 보조 표현).
#[rustfmt::skip]
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "you", "your", "we", "our", "us", "i", "me", "my", "it", "its", "this", "that", "these", "those",
    "please", "kindly", "must", "should", "shall", "will", "would", "can", "could", "may", "might", "always",
    "do", "does", "did", "be", "is", "are", "was", "were", "been",
    "to", "of", "in", "on", "at", "by", "for", "from", "with", "into", "as", "and", "or", "any", "also", "just", "then",
    "하지", "해", "하고", "하라", "해라", "하세요", "합니다", "한다", "할", "하는", "주세요", "줘",
    "것", "수", "및", "또는", "반드시", "꼭", "절대", "절대로",
];

/// 한국어 어미·조사(긴 것부터). 어간이 남을 때만 떼어 "탐지해"·"탐지하지" → "탐지", "이상치는" → "이상치"로 맞춘다.
/// 명사 끝 글자로 흔한 가·도·만(할인가, 카드한도, 1천만)은 엄격 비교에서는 떼지 않는다(KO_LOOSE_SUFFIXES).
#[rustfmt::skip]
const KO_SUFFIXES: &[&str] = &[
    // 4글자
    "해주세요", "하십시오",
    // 3글자
    "하지는", "하지도", "하세요", "하시오", "합니다", "하도록", "되도록", "시켜라", "시키지", "시키고",
    // 2글자: 어미
    "하지", "해라", "해요", "해줘", "해서", "해야", "하고", "하라", "한다", "하는", "하여", "하면", "하되", "하기",
    "하게", "했다", "했음", "되지", "되는", "되어", "된다", "되게", "시켜", "시킨", "시킬",
    // 2글자: 조사
    "으로", "에서", "에게", "까지", "부터", "처럼", "보다", "마다", "이나", "이랑",
    // 1글자
    "해", "할", "한", "함", "된", "됨", "돼",
    "를", "을", "는", "은", "이", "의", "에", "로", "와", "과", "랑",
];

/// 경고용 느슨한 비교에서만 떼는 조사(이상치가/이상치도/이상치만).
const KO_LOOSE_SUFFIXES: &[&str] = &["가", "도", "만"];

/// 관계절 앞말. 바로 뒤의 부정은 금지가 아니라 대상을 설명한다("customers who never purchased").
const RELATIVES: &[&str] = &["who", "that", "which", "whose", "where"];

/// 부정사 "to avoid", "to never"를 금지로 읽게 하는 앞말("Try to avoid jargon", "Make sure to never round").
/// 문장 첫머리부터 "to"까지가 이런 말과 불용어뿐일 때만 금지이고, 그 밖의 부정사("Suggest ways to avoid churn",
/// "List risks to avoid", "To avoid churn, …")는 대상을 설명할 뿐이다.
const TO_FRAMES: &[&str] = &[
    "try",
    "make",
    "sure",
    "remember",
    "careful",
    "need",
    "have",
    "important",
];

/// 단어를 이어 붙이는 하이픈. "never-paid"처럼 하이픈으로 붙은 부정어는 복합어의 일부(설명)다.
const HYPHENS: [char; 3] = ['-', '\u{2010}', '\u{2011}'];

/// 관형형 부정("구매하지 않은 고객"): 금지가 아니라 대상을 설명한다.
const KO_DESCRIPTIVE_NEG: &[&str] = &["않은", "않는"];

/// "않" 뒤의 목적·결과 어미("-지 않도록", "-지 않게(끔)"). 뒤에 지시(KO_PURPOSE_DIRECTIVES)가 오거나 문장이 끝나면
/// 금지("포함되지 않도록 해", "누락되지 않도록")이고, 다른 말이 오면 목적절이다("이탈하지 않도록 할 방안을 제시해").
const KO_PURPOSE: &[&str] = &["도록", "게끔", "게"];

/// 목적절 뒤에 와서 금지 지시로 만드는 "하다"의 지시형. "할"은 뒤에 "것"이 올 때만("할 것") 지시다.
#[rustfmt::skip]
const KO_PURPOSE_DIRECTIVES: &[&str] = &[
    "해", "하라", "해라", "하세요", "해요", "해줘", "해주세요", "하십시오", "하시오", "합시다", "하자", "한다", "합니다",
    "해야", "할것",
];

/// 목적절 뒤에 와도 금지 지시인 말의 앞부분("주의해", "유의하세요", "조심할 것").
const KO_PURPOSE_CAUTIONS: &[&str] = &["주의", "유의", "조심"];

/// 단어 끝(또는 금지 어미 앞)에 있으면 부정인 한국어 표지. 앞부분은 내용으로 남긴다("사용금지" → "사용").
const KO_BANS: &[&str] = &["금지", "지양", "자제"];

/// "금지" 뒤에 붙어도 금지인 어미("사용금지", "금지한다"). 다른 글자가 붙으면("금지어", "보험금지급", "금지된") 금지가 아니다.
#[rustfmt::skip]
const KO_BAN_ENDINGS: &[&str] = &[
    "", "야", "다", "임", "이다", "입니다", "함", "하다", "한다", "합니다", "해", "하라", "해라", "하세요", "하고", "하며",
    "됨", "된다", "되어", "돼",
];

/// 예외·대조 표현. 긍정 제약에만 있으면 부정 제약이 그 예외를 되풀이한 것일 수 있으므로 지우지 않고 경고한다
/// ("Use the median instead of the mean" + "Do not use the mean", "평균 대신 중앙값을 사용해" + "평균은 사용하지 마").
#[rustfmt::skip]
const EXCEPTION_WORDS: &[&str] = &[
    "instead", "except", "excluding", "exclude", "excludes", "excluded", "without", "unless", "besides", "rather",
    "not",
    "대신", "제외", "말고", "빼고", "외", "외에", "외의", "아닌", "아니라", "아니고", "없이", "않은", "않는",
];

/// 서로 반대 의미로 쓰이는 단어쌍(영어는 활용형 포함, 한국어는 어간)과, 나머지 내용이 같을 때 확실한 모순인지 여부.
/// 지시형 쌍(제외/포함 등, true)만 확실하다. 증가/감소처럼 데이터를 설명할 때 흔히 쓰는 쌍(false)은 어느 자리에 있어도
/// 경고만 한다("매출 증가 원인을 분석해" + "매출 감소 원인을 분석해", "재고 증가" + "재고 감소").
type WordSet = &'static [&'static str];
const ANTONYMS: [(WordSet, WordSet, bool); 8] = [
    (&["제외", "배제"], &["포함"], true),
    (
        &["exclude", "excludes", "excluded", "excluding", "exclusion"],
        &["include", "includes", "included", "including", "inclusion"],
        true,
    ),
    (&["허용"], &["불허"], true),
    (
        &["allow", "allows", "allowed", "allowing"],
        &["forbid", "forbids", "forbidden", "forbidding"],
        true,
    ),
    (&["증가"], &["감소"], false),
    (
        &["increase", "increases", "increased", "increasing"],
        &["decrease", "decreases", "decreased", "decreasing"],
        false,
    ),
    (&["오름차순"], &["내림차순"], true),
    (&["ascending"], &["descending"], true),
];

/// 비교 연산자 문자. 연속된 것을 하나의 연산자 토큰으로 본다(>=, <>, ==).
const CMP_CHARS: [char; 6] = ['<', '>', '=', '≤', '≥', '≠'];
/// 숫자에 붙으면 의미가 있는 기호(부호, 산술, 범위). "z-score"의 하이픈처럼 숫자와 떨어져 있으면 구분자다.
const ARITH_CHARS: [char; 10] = ['+', '-', '−', '–', '*', '/', '^', '~', '×', '÷'];

// ---------- 따옴표·토큰 ----------

/// 따옴표 구간의 바이트 범위 [시작, 끝)을 닫는 따옴표까지 돌려준다:
/// '…' "…" “…” ‘…’ `…`(같은 길이 백틱) 「…」 『…』 《…》 〈…〉 «…».
/// 영어 축약형·소유격의 아포스트로피(don't, it's, users')는 따옴표로 열지 않는다. 짝이 없는 따옴표는 일반 문자다.
/// 한국어처럼 닫는 따옴표 뒤에 조사가 붙어도('매우 만족'을) 닫힌다.
fn quote_spans(s: &str) -> Vec<(usize, usize)> {
    let cs: Vec<(usize, char)> = s.char_indices().collect();
    let n = cs.len();
    let ch = |k: usize| cs.get(k).map(|&(_, c)| c);
    // 앞이 영숫자, 뒤가 영문자인 ' ’ 는 단어 안 아포스트로피다(don't).
    let apostrophe = |k: usize| {
        k > 0
            && ch(k - 1).is_some_and(|c| c.is_ascii_alphanumeric())
            && ch(k + 1).is_some_and(|c| c.is_ascii_alphabetic())
    };
    let positions = |pred: &dyn Fn(usize) -> bool| (0..n).filter(|&k| pred(k)).collect::<Vec<_>>();
    let double = positions(&|k| cs[k].1 == '"');
    // 짝 따옴표(“” 「」 『』 《》 〈〉 «»): 닫는 문자 → 위치들
    let paired: Vec<(char, Vec<usize>)> = PAIRED_QUOTES
        .iter()
        .map(|&(open, close)| (open, positions(&|k| cs[k].1 == close)))
        .collect();
    let single = positions(&|k| cs[k].1 == '\'' && !apostrophe(k));
    let curly_single = positions(&|k| cs[k].1 == '’' && !apostrophe(k));
    // 백틱 열: 길이 → 시작 위치들
    let mut ticks: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut k = 0;
    while k < n {
        let run = cs[k..].iter().take_while(|&&(_, c)| c == '`').count();
        if run > 0 {
            ticks.entry(run).or_default().push(k);
        }
        k += run.max(1);
    }
    let next = |list: &[usize], after: usize| list.get(list.partition_point(|&p| p <= after)).copied();
    let mut spans = vec![];
    let mut i = 0;
    while i < n {
        // 닫는 따옴표 다음 문자 위치
        let end = match cs[i].1 {
            '"' => next(&double, i).map(|k| k + 1),
            '\'' if !(i > 0 && ch(i - 1).is_some_and(|p| p.is_ascii_alphanumeric())) => next(&single, i).map(|k| k + 1),
            '‘' => next(&curly_single, i).map(|k| k + 1),
            '`' => {
                let run = cs[i..].iter().take_while(|&&(_, c)| c == '`').count();
                match ticks.get(&run).and_then(|v| next(v, i)) {
                    Some(k) => Some(k + run),
                    None => {
                        i += run;
                        continue;
                    }
                }
            }
            c => paired
                .iter()
                .find(|(open, _)| *open == c)
                .and_then(|(_, closers)| next(closers, i))
                .map(|k| k + 1),
        };
        match end {
            Some(e) => {
                spans.push((cs[i].0, cs.get(e).map_or(s.len(), |&(b, _)| b)));
                i = e;
            }
            None => i += 1,
        }
    }
    spans
}

/// 의미 비교용 토큰.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// 소문자 단어 또는 숫자(숫자 사이의 소수점·천 단위 쉼표 포함).
    /// `hyphened`: 하이픈으로 앞이나 뒤의 단어와 붙어 있다("never-paid"의 "never"와 "paid").
    Word { text: String, hyphened: bool },
    /// 비교 연산자, 숫자에 붙은 부호·산술 기호, %
    Op(String),
    /// 따옴표 안의 값(공백만 하나로 줄이고 대소문자는 유지)
    Quote(String),
}

fn lex(s: &str) -> Vec<Tok> {
    let mut out = vec![];
    let mut pos = 0;
    for (a, b) in quote_spans(s).into_iter().chain([(s.len(), s.len())]) {
        lex_plain(&s[pos..a], &mut out);
        if a < b {
            let span = &s[a..b];
            let inner = if span.starts_with('`') {
                span.trim_matches('`')
            } else {
                let open = span.chars().next().map_or(0, char::len_utf8);
                let close = span.chars().next_back().map_or(0, char::len_utf8);
                &span[open..span.len() - close]
            };
            out.push(Tok::Quote(inner.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        pos = b;
    }
    out
}

/// 따옴표 밖 구간을 토큰으로 나눈다. 그 밖의 문장 부호는 구분자다.
fn lex_plain(s: &str, out: &mut Vec<Tok>) {
    let cs: Vec<char> = s.chars().collect();
    let digit = |k: usize| cs.get(k).is_some_and(|c| c.is_ascii_digit());
    let cmp = |k: usize| {
        cs.get(k).is_some_and(|c| CMP_CHARS.contains(c)) || (cs.get(k) == Some(&'!') && cs.get(k + 1) == Some(&'='))
    };
    let mut k = 0;
    while k < cs.len() {
        let c = cs[k];
        if c.is_alphanumeric() {
            let start = k;
            let mut w = String::new();
            while k < cs.len() {
                let c = cs[k];
                if c.is_alphanumeric() || (matches!(c, '.' | ',') && digit(k.wrapping_sub(1)) && digit(k + 1)) {
                    w.extend(c.to_lowercase());
                    k += 1;
                } else {
                    break;
                }
            }
            // 하이픈 양쪽이 모두 글자일 때만 복합어다("never-paid"). "- never"(목록 표지)나 "-1"(부호)은 아니다.
            let joined = |h: usize, other: usize| {
                cs.get(h).is_some_and(|c| HYPHENS.contains(c)) && cs.get(other).is_some_and(|c| c.is_alphanumeric())
            };
            let hyphened = (start >= 2 && joined(start - 1, start - 2)) || joined(k, k + 1);
            out.push(Tok::Word { text: w, hyphened });
        } else if cmp(k) {
            let start = k;
            while cmp(k) {
                k += 1;
            }
            out.push(Tok::Op(cs[start..k].iter().collect()));
        } else {
            if c == '%' || (ARITH_CHARS.contains(&c) && (digit(k.wrapping_sub(1)) || digit(k + 1))) {
                out.push(Tok::Op(c.to_string()));
            }
            k += 1;
        }
    }
}

/// 중복 판정 키: 대소문자·문장 부호·공백만 무시하고 숫자, 부호, 비교 연산자, 따옴표 값은 지킨다.
/// "Flag amount > 1000"과 "Flag amount < 1000", "-1"과 "1"은 다른 제약이다.
fn dedup_key(s: &str) -> String {
    let toks = lex(s);
    if toks.is_empty() {
        // 단어가 없는 문장("|", "&")은 원문으로 비교한다
        return format!("\u{0}{}", s.trim());
    }
    toks.iter()
        .map(|t| match t {
            Tok::Word { text: w, .. } | Tok::Op(w) => w.clone(),
            Tok::Quote(q) => format!("\"{}\"", q.replace('\\', "\\\\").replace('"', "\\\"")),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_hangul(c: char) -> bool {
    ('가'..='힣').contains(&c)
}

/// 한글 단어 끝의 조사·어미를 뗀다. 남는 어간이 한글로 끝나고 엄격 비교는 2글자, 느슨한 비교는 1글자 이상일 때만
/// 떼므로 "원가", "증가", "10만"은 그대로이고 "주문금액"처럼 조사가 없는 단어도 그대로다.
/// 느슨한 비교는 가·도·만까지 떼되 숫자 단위 뒤의 "만"("1천만", "십만")은 남긴다.
fn stem(w: &str, loose: bool) -> &str {
    let (min, extra) = if loose { (1, KO_LOOSE_SUFFIXES) } else { (2, &[][..]) };
    let mut cur = w;
    while let Some(base) = KO_SUFFIXES.iter().chain(extra).find_map(|suf| {
        cur.strip_suffix(suf).filter(|b| {
            let last = b.chars().next_back();
            b.chars().count() >= min
                && last.is_some_and(is_hangul)
                && !(*suf == "만" && last.is_some_and(|c| "십백천만억조".contains(c)))
        })
    }) {
        cur = base;
    }
    cur
}

/// 경고용 느슨한 어간: 영어 복수형을 접고 한글은 한 글자 어간까지 뗀다("값을"/"값은" → "값").
fn loose_stem(w: &str) -> String {
    if !w.is_ascii() {
        return stem(w, true).to_string();
    }
    if w.len() > 4 && w.ends_with("ies") {
        return format!("{}y", &w[..w.len() - 3]);
    }
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

/// 반의어 단어면 (쌍 번호, 쪽)을 돌려준다. `w`는 어간이다.
fn antonym_of(w: &str) -> Option<(usize, bool)> {
    ANTONYMS.iter().enumerate().find_map(|(i, (a, b, _))| {
        if a.contains(&w) {
            Some((i, false))
        } else if b.contains(&w) {
            Some((i, true))
        } else {
            None
        }
    })
}

fn is_stopword(w: &str) -> bool {
    STOPWORDS.contains(&w)
}

/// 모순 비교용 제약 요약.
#[derive(Debug, Default)]
struct RuleInfo {
    negative: bool,
    /// 부정 표지·불용어를 뺀 내용(어간, 연산자, 따옴표 값)을 문장 순서대로. 부정이 긍정을 덮는지 볼 때 쓴다.
    content: Vec<String>,
    /// 복수형·짧은 어간까지 접은 내용. 확신할 수 없는 모순을 경고할 때만 쓴다.
    loose: BTreeSet<String>,
    /// 반의어 (쌍 번호, 쪽)
    antonyms: Vec<(usize, bool)>,
    /// 반의어 단어를 뺀 내용
    rest: BTreeSet<String>,
    /// 예외·대조 표현(instead, except, 대신, 제외, 대상을 꾸미는 "않은", 목적절 "않도록" …)
    exceptions: BTreeSet<String>,
}

/// 단어 끝(또는 금지 어미 앞)에 있는 한국어 금지 표지의 위치. "금지어", "보험금지급", "금지된"은 금지가 아니다.
fn ban_at(w: &str) -> Option<usize> {
    KO_BANS.iter().find_map(|m| {
        let at = w.rfind(m)?;
        KO_BAN_ENDINGS.contains(&&w[at + m.len()..]).then_some(at)
    })
}

/// "않"이 든 단어 `w`가 목적절("-지 않도록", "-지 않게")이면 금지 지시인지 돌려준다. `after`는 그 뒤의 토큰들이다.
/// 지시("해", "하세요", "해 주세요", "할 것", "주의해" …)가 붙어 있거나 뒤따르면, 또는 문장이 끝나면 금지다.
/// 다른 말("할 방안", "하는 방법", "검토해")이 오면 목적을 설명할 뿐이다. 목적절이 아니면 None.
fn purpose_ban(w: &str, after: &[Tok]) -> Option<bool> {
    let at = w.find('않')?;
    let ending = &w[at + '않'.len_utf8()..];
    let attached = KO_PURPOSE.iter().find_map(|p| ending.strip_prefix(p))?;
    // 강조 부사("꼭", "반드시")는 건너뛰고 본다("누락되지 않도록 꼭 해")
    let words: Vec<&str> = after
        .iter()
        .map(|t| match t {
            Tok::Word { text, .. } => text.as_str(),
            _ => "",
        })
        .filter(|w| !["꼭", "반드시", "절대", "절대로"].contains(w))
        .collect();
    let directive = |d: &str, then: Option<&&str>| {
        KO_PURPOSE_DIRECTIVES.contains(&d)
            || (d == "할" && then == Some(&"것"))
            || KO_PURPOSE_CAUTIONS.iter().any(|c| d.starts_with(c))
    };
    Some(if attached.is_empty() {
        words.first().is_none_or(|d| directive(d, words.get(1)))
    } else {
        directive(attached, words.first())
    })
}

fn analyze(s: &str) -> RuleInfo {
    let toks = lex(s);
    let word = |k: usize| match toks.get(k) {
        Some(Tok::Word { text, .. }) => Some(text.as_str()),
        _ => None,
    };
    let hyphened = |k: usize| matches!(toks.get(k), Some(Tok::Word { hyphened: true, .. }));
    // framed[i]: i번째 토큰 앞이 문장 첫머리부터 금지 틀("Try", "Make sure", "Be careful")과 불용어뿐이고 틀이 있다
    let mut framed = Vec::with_capacity(toks.len());
    let (mut only_frames, mut any_frame) = (true, false);
    for t in &toks {
        framed.push(only_frames && any_frame);
        match t {
            Tok::Word { text, .. } if TO_FRAMES.contains(&text.as_str()) => any_frame = true,
            Tok::Word { text, .. } if is_stopword(text) => {}
            _ => only_frames = false,
        }
    }
    // 관계절("customers who never purchased")과 부정사("ways to avoid churn") 안의 부정은 대상을 설명할 뿐 금지가 아니다
    let explained = |k: usize| match k.checked_sub(1).and_then(word) {
        Some(w) if RELATIVES.contains(&w) => true,
        Some("to") => !framed[k - 1],
        _ => false,
    };
    // 관형형 부정("구매하지 않은 고객")은 뒤의 말을 꾸밀 뿐 금지가 아니다
    let adnominal = |k: usize, w: &str| k + 1 < toks.len() && KO_DESCRIPTIVE_NEG.iter().any(|d| w.ends_with(d));
    // 부정 표지 토큰과, 붙여 쓴 표지를 떼고 남는 앞부분("사용금지" → "사용", "탐지하지마" → "탐지하지")
    let mut marker = vec![false; toks.len()];
    let mut residual: Vec<Option<&str>> = vec![None; toks.len()];
    // 대상·목적을 설명하는 한국어 부정("않은 고객", "않도록 할 방안"). 예외·대조 표현으로 센다.
    let mut explains = vec![false; toks.len()];
    for k in 0..toks.len() {
        for p in NEG_PHRASES {
            // 하이픈 복합어("never-paid", "do-not-call")의 부정어는 설명이다
            if p.iter().enumerate().all(|(o, w)| word(k + o) == Some(*w))
                && !explained(k)
                && !(k..k + p.len()).any(hyphened)
            {
                marker[k..k + p.len()].fill(true);
            }
        }
        let Some(w) = word(k) else { continue };
        let not_at = w.find('않');
        if not_at.is_some() {
            explains[k] = !purpose_ban(w, &toks[k + 1..]).unwrap_or_else(|| !adnominal(k, w));
        }
        if let Some(at) = not_at.filter(|_| !explains[k]).or_else(|| ban_at(w)) {
            marker[k] = true;
            residual[k] = Some(&w[..at]);
        } else if w == "말" && word(k + 1) == Some("것") {
            marker[k] = true;
            marker[k + 1] = true;
        } else if KO_NEG_IMPERATIVES.contains(&w) && k > 0 && word(k - 1).is_some_and(|p| p.ends_with('지')) {
            marker[k] = true;
        } else if let Some(base) = w.strip_suffix("말것").filter(|b| !b.is_empty()).or_else(|| {
            KO_NEG_IMPERATIVES.iter().find_map(|f| {
                w.strip_suffix(f)
                    .filter(|b| b.ends_with('지') && b.chars().count() >= 2)
            })
        }) {
            marker[k] = true;
            residual[k] = Some(base);
        }
    }
    let mut info = RuleInfo {
        negative: marker.contains(&true),
        ..RuleInfo::default()
    };
    for (k, t) in toks.iter().enumerate() {
        let value = match t {
            Tok::Word { text, .. } => {
                let w = if marker[k] {
                    residual[k].unwrap_or_default()
                } else {
                    text.as_str()
                };
                let strict = stem(w, false);
                if w.is_empty() || is_stopword(w) || is_stopword(strict) {
                    continue;
                }
                if EXCEPTION_WORDS.contains(&w) || EXCEPTION_WORDS.contains(&strict) || explains[k] {
                    info.exceptions.insert(strict.to_string());
                }
                info.loose.insert(loose_stem(w));
                match antonym_of(strict) {
                    Some(a) => info.antonyms.push(a),
                    None => {
                        info.rest.insert(strict.to_string());
                    }
                }
                info.content.push(strict.to_string());
                continue;
            }
            Tok::Op(o) => o.clone(),
            Tok::Quote(q) => format!("\"{q}\""),
        };
        info.content.push(value.clone());
        info.loose.insert(value.clone());
        info.rest.insert(value);
    }
    info
}
/// 부정 지시인지 판단한다. 따옴표 안의 값("do not know")과 대상·목적을 설명하는 부정("않은 고객",
/// "ways to avoid churn", "never-paid invoices", "이탈하지 않도록 할 방안")은 보지 않는다.
pub fn is_negative(s: &str) -> bool {
    analyze(s).negative
}

#[derive(Debug, PartialEq, Eq)]
enum Relation {
    Unrelated,
    /// 확실한 모순: 앞의 제약을 지운다
    Clash,
    /// 모순일 수 있음: 둘 다 두고 경고만 한다
    Suspect,
}

/// `needle`의 영어 단어가 `hay` 안에 같은 순서로 들어 있는가(사이에 다른 말이 있어도 된다).
/// 한국어는 조사가 역할을 나타내 어순이 자유로우므로 순서를 보지 않는다.
fn in_order(needle: &[String], hay: &[String]) -> bool {
    let mut rest = hay.iter().filter(|w| w.is_ascii());
    needle.iter().filter(|w| w.is_ascii()).all(|n| rest.any(|h| h == n))
}

fn relation(a: &RuleInfo, b: &RuleInfo) -> Relation {
    if a.negative != b.negative {
        let (neg, pos) = if a.negative { (a, b) } else { (b, a) };
        let pos_words: BTreeSet<&String> = pos.content.iter().collect();
        let covered = !neg.content.is_empty() && neg.content.iter().all(|w| pos_words.contains(w));
        // 부정이 긍정 지시를 같은 어순으로 덮고, 긍정 쪽에만 있는 예외·대조 표현("instead", "대신")이 없을 때만
        // 확실한 모순이다. 더 좁은 부정(예외 조항)은 덮지 못하므로 모순이 아니다.
        if covered && in_order(&neg.content, &pos.content) && pos.exceptions.is_subset(&neg.exceptions) {
            return Relation::Clash;
        }
        if covered || (!neg.loose.is_empty() && neg.loose.is_subset(&pos.loose)) {
            return Relation::Suspect;
        }
        return Relation::Unrelated;
    }
    // 같은 극성: 한쪽에만 있는 반의어가 서로 반대여야 한다(두 쪽을 다 가진 문장은 뺀다).
    // 지시형 쌍(제외/포함 등)이 반대이고 나머지 내용이 같을 때만 확실한 모순이다. 증가/감소 같은 설명형 쌍은
    // 서술어 자리에 있어도("Explain revenue increase", "재고 증가") 확신할 수 없으므로 모순 의심까지만 간다.
    let has = |r: &RuleInfo, p: usize, side: bool| r.antonyms.contains(&(p, side));
    let opposed: Vec<bool> = a
        .antonyms
        .iter()
        .filter(|&&(p, side)| has(b, p, !side) && !has(a, p, !side) && !has(b, p, side))
        .map(|&(p, _)| ANTONYMS[p].2)
        .collect();
    if opposed.is_empty() || a.rest.is_empty() || b.rest.is_empty() {
        Relation::Unrelated
    } else if a.rest == b.rest && opposed.contains(&true) {
        Relation::Clash
    } else if a.rest.is_subset(&b.rest) || b.rest.is_subset(&a.rest) {
        Relation::Suspect
    } else {
        Relation::Unrelated
    }
}

/// 제약들에 대한 진단. 린트 전 ID(C1..Cn)에서 원래 입력 위치를 읽어 `involved` 순서대로 담는다
/// (제거는 [지운 제약, 남긴 제약], 모순 의심은 [앞 제약, 뒤 제약]).
fn about(involved: &[&ConstraintNode], severity: &str, message: String) -> LintDiagnostic {
    let indices = involved.iter().filter_map(|c| c.input_index()).collect();
    LintDiagnostic::for_constraints(severity, message, indices)
}

// ---------- 수식어·공백 ----------

fn is_newline(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// 단어가 수식어면 (앞 문장 부호, 뒤 문장 부호)를 돌려준다. 따옴표가 붙은 단어는 수식어로 보지 않는다.
fn filler_parts(w: &str) -> Option<(&str, &str)> {
    if w.contains(QUOTE_CHARS) {
        return None;
    }
    let start = w.find(char::is_alphanumeric)?;
    let (last, c) = w.char_indices().rev().find(|(_, c)| c.is_alphanumeric())?;
    let end = last + c.len_utf8();
    FILLERS
        .contains(&w[start..end].to_lowercase().as_str())
        .then_some((&w[..start], &w[end..]))
}

/// 수식어를 지우고(`drop_fillers`) 공백을 정리해 (결과, 지운 개수)를 돌려준다.
/// 따옴표 안('…' "…" “…” ‘…’ `…`)은 단어를 지우지 않고 글자도 그대로 둔다.
/// `keep_lines`면 줄바꿈을 살리고(빈 줄은 하나까지) 줄 안의 공백만 하나로 줄인다(역할 지시문).
/// 아니면 한 줄로 만들고, 따옴표 안의 줄바꿈·제어 문자만 공백으로 바꾼다(제약은 한 줄 자리다).
/// 지운 수식어에 붙어 있던 문장 부호는 살린다: "(very important)" → "(important)", "do it very." → "do it."
fn compress(s: &str, keep_lines: bool, drop_fillers: bool) -> (String, usize) {
    let text = if keep_lines {
        s.replace("\r\n", "\n")
    } else {
        s.to_string()
    };
    // (앞 공백의 줄바꿈 수, 단어). 따옴표 구간은 공백이 있어도 단어의 일부다.
    let mut words: Vec<(usize, String)> = vec![];
    let mut cur: Option<(usize, String)> = None;
    let mut gap = 0;
    let mut spans = quote_spans(&text).into_iter().peekable();
    let mut chars = text.char_indices();
    while let Some((b, c)) = chars.next() {
        if let Some(&(a, e)) = spans.peek().filter(|&&(a, _)| a == b) {
            spans.next();
            let w = &mut cur.get_or_insert_with(|| (std::mem::take(&mut gap), String::new())).1;
            if keep_lines {
                w.push_str(&text[a..e]);
            } else {
                w.extend(
                    text[a..e]
                        .chars()
                        .map(|c| if c.is_control() || is_newline(c) { ' ' } else { c }),
                );
            }
            while text.len() - chars.as_str().len() < e {
                chars.next();
            }
        } else if c.is_whitespace() || c.is_control() {
            words.extend(cur.take());
            gap += usize::from(is_newline(c));
        } else {
            cur.get_or_insert_with(|| (std::mem::take(&mut gap), String::new()))
                .1
                .push(c);
        }
    }
    words.extend(cur.take());
    let mut out = String::new();
    let mut removed = 0;
    // 마지막으로 남긴 단어 뒤로 본 줄바꿈 수(지운 단어 양쪽 공백 중 큰 쪽)
    let mut sep = 0;
    // 지운 수식어 앞에 붙어 있던 여는 부호("(very" → "("). 다음에 남기는 단어 앞에 붙인다.
    let mut open = String::new();
    for (lines, w) in words {
        sep = sep.max(lines);
        if let Some((before, after)) = filler_parts(&w).filter(|_| drop_fillers) {
            removed += 1;
            match (before.is_empty(), after.is_empty()) {
                (false, true) => open.push_str(before),
                // 닫는 부호("very," "very.")는 같은 줄 앞 단어에 붙인다. 앞 단어가 이미 부호로 끝나면 버린다.
                (true, false) if sep == 0 && out.chars().next_back().is_some_and(char::is_alphanumeric) => {
                    out.push_str(after)
                }
                _ => {}
            }
            continue;
        }
        if !out.is_empty() {
            out.push_str(match (keep_lines, sep) {
                (true, 1) => "\n",
                (true, 2..) => "\n\n",
                _ => " ",
            });
        }
        out.push_str(&std::mem::take(&mut open));
        out.push_str(&w);
        sep = 0;
    }
    (out, removed)
}

// ---------- 패스 ----------

#[derive(Default)]
pub struct FillerPass {
    removed: usize,
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for FillerPass {
    fn visit_role(&mut self, role: &mut RoleNode) {
        let (s, n) = compress(&role.instruction, true, true);
        role.instruction = s;
        self.removed += n;
    }
    fn visit_constraints(&mut self, constraints: &mut Vec<ConstraintNode>) {
        for c in constraints.iter_mut() {
            let (s, n) = compress(&c.expression, false, true);
            if s.is_empty() {
                // 수식어만 있는 제약은 지우지 않고 공백만 정리한다
                c.expression = compress(&c.expression, false, false).0;
            } else {
                c.expression = s;
                self.removed += n;
            }
        }
    }
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
        if self.removed > 0 {
            self.diags.push(LintDiagnostic::new(
                "WARN",
                format!("중복 수식어 {}건 압축됨", self.removed),
            ));
        }
        std::mem::take(&mut self.diags)
    }
}

/// 같은 제약(대소문자·문장 부호·공백만 다름)은 마지막 것만 남긴다(나중 지시 우선: "A", "not A", "A"는 A가 남는다).
/// 지운 제약마다 지운 본문을 담은 WARN을 남긴다.
#[derive(Default)]
pub struct DedupPass {
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for DedupPass {
    fn visit_constraints(&mut self, constraints: &mut Vec<ConstraintNode>) {
        let keys: Vec<String> = constraints.iter().map(|c| dedup_key(&c.expression)).collect();
        let last: HashMap<&str, usize> = keys.iter().enumerate().map(|(k, key)| (key.as_str(), k)).collect();
        let mut keep = vec![true; constraints.len()];
        for (k, c) in constraints.iter().enumerate() {
            let kept = last[keys[k].as_str()];
            if kept == k {
                continue;
            }
            keep[k] = false;
            let survivor = &constraints[kept];
            let message = if survivor.expression == c.expression {
                format!("중복 제약 제거: \"{}\" (뒤의 같은 제약 유지)", c.expression)
            } else {
                format!(
                    "중복 제약 제거: \"{}\" (뒤의 같은 제약 유지: \"{}\")",
                    c.expression, survivor.expression
                )
            };
            self.diags.push(about(&[c, survivor], "WARN", message));
        }
        let mut keep = keep.into_iter();
        constraints.retain(|_| keep.next().unwrap_or(true));
    }
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
        std::mem::take(&mut self.diags)
    }
}

/// 모순 제약 중 앞의 것을 지운다(나중 지시 우선). 확신할 수 없는 쌍은 둘 다 두고 경고한다.
#[derive(Default)]
pub struct ContradictionPass {
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for ContradictionPass {
    fn visit_constraints(&mut self, constraints: &mut Vec<ConstraintNode>) {
        let infos: Vec<RuleInfo> = constraints.iter().map(|c| analyze(&c.expression)).collect();
        let n = constraints.len();
        let mut drop = vec![false; n];
        for j in 0..n {
            for i in 0..j {
                if drop[i] || relation(&infos[i], &infos[j]) != Relation::Clash {
                    continue;
                }
                drop[i] = true;
                let (removed, kept) = (&constraints[i], &constraints[j]);
                self.diags.push(about(
                    &[removed, kept],
                    "WARN",
                    format!(
                        "모순 지시 자동제거: \"{}\" ↔ \"{}\" (뒤의 지시 유지)",
                        removed.expression, kept.expression
                    ),
                ));
            }
        }
        // 남은 제약 중 활용형만 달라 확신할 수 없는 쌍은 경고만 한다. 위치는 두 제약 모두에 단다(GUI 배지용).
        for j in (0..n).filter(|&j| !drop[j]) {
            for i in (0..j).filter(|&i| !drop[i]) {
                if relation(&infos[i], &infos[j]) == Relation::Suspect {
                    self.diags.push(about(
                        &[&constraints[i], &constraints[j]],
                        "WARN",
                        format!(
                            "모순 의심(둘 다 유지): \"{}\" ↔ \"{}\". 같은 대상을 반대로 지시하는지 확인하세요",
                            constraints[i].expression, constraints[j].expression
                        ),
                    ));
                }
            }
        }
        let mut drop = drop.into_iter();
        constraints.retain(|_| !drop.next().unwrap_or(false));
    }
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
        std::mem::take(&mut self.diags)
    }
}

/// 출력 스키마 검사: 빈 필드와 중복 필드(처음 것을 남긴다)를 지우고, 지운 필드 이름을 WARN에 적는다.
#[derive(Default)]
pub struct SchemaPass {
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for SchemaPass {
    fn visit_output(&mut self, out: &mut OutputSchemaAst) {
        let mut seen = HashSet::new();
        // 지운 중복 필드(처음 나온 순서, 지운 횟수)와 빈 필드 수
        let mut dups: Vec<(String, usize)> = vec![];
        let mut blanks = 0;
        out.required_fields.retain(|f| {
            if f.trim().is_empty() {
                blanks += 1;
                false
            } else if seen.insert(f.clone()) {
                true
            } else {
                match dups.iter_mut().find(|(d, _)| d == f) {
                    Some((_, n)) => *n += 1,
                    None => dups.push((f.clone(), 1)),
                }
                false
            }
        });
        if !dups.is_empty() || blanks > 0 {
            let mut parts = vec![];
            if !dups.is_empty() {
                let names: Vec<String> = dups
                    .iter()
                    .map(|(f, n)| {
                        if *n > 1 {
                            format!("{f:?} ×{n}")
                        } else {
                            format!("{f:?}")
                        }
                    })
                    .collect();
                parts.push(format!("중복 {}", names.join(", ")));
            }
            if blanks > 0 {
                parts.push(format!("빈 필드 {blanks}개"));
            }
            self.diags.push(LintDiagnostic::new(
                "WARN",
                format!("출력 스키마의 빈/중복 필드 제거됨: {}", parts.join(", ")),
            ));
            out.raw_schema = OutputSchemaAst::render_schema(out.format, &out.required_fields);
        }
        if out.required_fields.is_empty() {
            self.diags
                .push(LintDiagnostic::new("ERROR", "출력 스키마에 필수 필드가 없음"));
        } else {
            self.diags.push(LintDiagnostic::new("INFO", "스키마 무결성 검증 통과"));
        }
    }
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
        std::mem::take(&mut self.diags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ast(rules: &[&str], fields: &[&str]) -> RootPromptAst {
        RootPromptAst {
            target_model: TargetModel::Claude,
            role: RoleNode {
                name: "Analyst".into(),
                instruction: "Please analyze very carefully".into(),
            },
            data_sub_ast: None,
            constraints: rules
                .iter()
                .enumerate()
                .map(|(i, r)| ConstraintNode::classify(format!("C{}", i + 1), r))
                .collect(),
            output_schema: OutputSchemaAst::new(
                OutputFormat::StrictJson,
                fields.iter().map(|s| s.to_string()).collect(),
            ),
            lint_warnings: vec![],
        }
    }

    /// 린트 후 남은 제약 문장
    fn kept(rules: &[&str]) -> Vec<String> {
        let mut a = ast(rules, &["a"]);
        lint(&mut a);
        a.constraints.into_iter().map(|c| c.expression).collect()
    }

    /// 수식어 압축 WARN(테스트 AST의 역할 문장에서 항상 나온다)을 뺀 WARN
    fn warns(a: &RootPromptAst) -> Vec<&LintDiagnostic> {
        a.lint_warnings
            .iter()
            .filter(|d| d.severity == "WARN" && !d.message.contains("수식어"))
            .collect()
    }

    #[test]
    fn removes_duplicates_and_fillers() {
        let mut a = ast(
            &[
                "Detect outliers first",
                "detect outliers first.",
                "very important: list causes",
            ],
            &["a"],
        );
        lint(&mut a);
        assert_eq!(a.constraints.len(), 2);
        assert_eq!(a.role.instruction, "analyze carefully");
        assert!(a.lint_warnings.iter().any(|d| d.message.contains("수식어")));
    }

    #[test]
    fn drops_earlier_contradiction() {
        let mut a = ast(&["이상치 우선 탐지", "이상치 탐지하지 마"], &["a"]);
        lint(&mut a);
        assert_eq!(a.constraints.len(), 1);
        assert_eq!(a.constraints[0].expression, "이상치 탐지하지 마");
        let w = warns(&a);
        assert_eq!(w.len(), 1);
        assert!(w[0].message.starts_with("모순 지시 자동제거") && w[0].message.contains("이상치 우선 탐지"));
        // 지운 제약(0)과 남긴 제약(1)의 입력 위치
        assert_eq!(w[0].indices, [0, 1]);
    }

    #[test]
    fn empty_schema_is_error() {
        let mut a = ast(&[], &[]);
        lint(&mut a);
        assert!(a.lint_warnings.iter().any(|d| d.severity == "ERROR"));
    }

    #[test]
    fn detects_more_negations_and_antonyms() {
        for (a, b) in [
            ("Detect outliers", "You must not detect outliers"),
            ("이상치 탐지해", "이상치는 탐지 말 것"),
            ("결측치 제외", "결측치 포함"),
            ("exclude missing rows", "include missing rows"),
            ("Never detect outliers", "Detect outliers"),
            ("Detect outliers in the amount column", "Never detect outliers"),
            ("결측치를 평균으로 대체해", "평균으로 결측치를 대체하지 마"),
            ("결측치를 포함해", "결측치는 포함하지 않는다"),
            ("id 커럼 사용", "id 커럼 사용금지"),
            ("날짜순 오름차순 정렬", "날짜순 내림차순 정렬"),
            ("Report totals including tax", "Report totals excluding tax"),
        ] {
            assert_eq!(kept(&[a, b]), [b], "{a} / {b}");
        }
    }

    #[test]
    fn keeps_unrelated_rules() {
        let mut t = ast(
            &["결측치 0.2% 제외", "이상치 우선 탐지", "Do not use the id column"],
            &["a"],
        );
        lint(&mut t);
        assert_eq!(t.constraints.len(), 3);
    }

    /// 리뷰에서 재현한 오탐: 부정 표지 부분 일치, 한국어 2글자 절단, 한 단어 차이, 더 좁은 부정.
    #[test]
    fn keeps_both_rules_that_do_not_contradict() {
        for (a, b) in [
            ("Use the amount column", "Never use the id column"),
            ("Do not sort rows by date", "Sort rows by amount"),
            (
                "Treat empty cells as zero values in totals",
                "You can treat empty cells as blanks",
            ),
            ("주문금액 평균을 계산해", "주문수량 평균은 계산하지 마"),
            ("매출액 합계를 보고해", "매출원가 합계는 보고하지 마"),
            ("Detect outliers", "Never detect outliers in the id column"),
            ("Never detect outliers in the id column", "Detect outliers"),
            ("Nevertheless, report totals", "Report totals by region"),
            ("Do note the currency", "Note the currency per row"),
            ("Flag amount > 1000", "Do not flag amount < 1000"),
            ("결측치 제외", "결측치 포함하지 마"),
            ("결측치 0.2% 제외", "결측치 포함"),
            ("Count 'very satisfied' answers", "Do not count 'satisfied' answers"),
            ("Do not answer \"do not know\"", "Answer \"yes\" or \"no\""),
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            let got: Vec<&str> = t.constraints.iter().map(|c| c.expression.as_str()).collect();
            assert_eq!(got, [a, b], "{a} / {b}");
            assert!(
                !t.lint_warnings.iter().any(|d| d.message.contains("제거")),
                "{a} / {b}: {:?}",
                t.lint_warnings
            );
        }
    }

    #[test]
    fn uncertain_pairs_warn_but_keep_both() {
        for (a, b) in [
            ("Detect outliers", "Never detect outlier"),
            ("값을 계산해", "값은 계산하지 마"),
            ("결측치 포함", "결측치 0.2% 제외"),
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            let w = warns(&t);
            assert_eq!(w.len(), 1, "{a} / {b}: {w:?}");
            assert!(w[0].message.starts_with("모순 의심") && w[0].message.contains(a) && w[0].message.contains(b));
            // 둘 다 남으므로 두 제약의 입력 위치(앞, 뒤)를 모두 단다(GUI 배지가 둘 다에 달린다).
            assert_eq!(w[0].indices, [0, 1]);
        }
    }

    #[test]
    fn negation_markers_match_whole_words() {
        for s in [
            "You can treat empty cells as blanks",
            "Nevertheless, report totals",
            "Do note the units",
            "Avoidance rate per region",
            "Count rows labeled \"do not know\"",
            "하지만 합계는 보고해",
            "끝까지 마지막 행 포함",
        ] {
            assert!(!is_negative(s), "{s}");
        }
        for s in [
            "Don't guess",
            "don’t guess",
            "You can't skip rows",
            "Never round",
            "Avoid jargon",
            "이상치 탐지하지 마",
            "이상치 탐지하지마라",
            "이상치는 탐지 말 것",
            "커럼 이름을 바꾸지 마세요",
            "결측치는 포함하지 않는다",
            "외부 데이터 사용금지",
        ] {
            assert!(is_negative(s), "{s}");
        }
    }

    #[test]
    fn korean_stems_drop_particles_and_endings_only() {
        for (w, want) in [
            ("탐지해", "탐지"),
            ("탐지하지", "탐지"),
            ("탐지하지는", "탐지"),
            ("이상치는", "이상치"),
            ("데이터에서는", "데이터"),
            ("평균으로", "평균"),
            ("주문금액", "주문금액"),
            ("매출액", "매출액"),
            ("포함", "포함"),
            ("원가", "원가"),
            ("증가를", "증가"),
            ("미만", "미만"),
            ("10만", "10만"),
        ] {
            assert_eq!(stem(w, false), want, "{w}");
        }
        // 긴 접미부터 떼어야 "하는"이 "는"보다 먼저 맞는다
        let lens: Vec<usize> = KO_SUFFIXES.iter().map(|s| s.chars().count()).collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "{lens:?}");
    }

    #[test]
    fn dedup_keeps_numbers_operators_and_quoted_values() {
        for (a, b) in [
            ("Flag amount > 1000", "Flag amount < 1000"),
            ("Treat missing as -1", "Treat missing as 1"),
            ("결측치 0.2% 제외", "결측치 2% 제외"),
            ("Flag amount >= 5", "Flag amount > 5"),
            ("Round to 1.5", "Round to 15"),
            ("Label 'Yes' rows", "Label 'yes' rows"),
            ("Label 'a b' rows", "Label 'a' 'b' rows"),
            ("|", "&"),
        ] {
            assert_ne!(dedup_key(a), dedup_key(b), "{a} / {b}");
            assert_eq!(kept(&[a, b]), [a, b], "{a} / {b}");
        }
        for (a, b) in [
            ("Detect outliers first", "detect outliers first."),
            ("z-score 3 기준", "z score 3 기준"),
            ("Flag amount>1000", "flag amount > 1000"),
        ] {
            assert_eq!(dedup_key(a), dedup_key(b), "{a} / {b}");
        }
    }

    #[test]
    fn removal_warnings_name_the_removed_text_and_index() {
        let mut a = ast(
            &[
                "Detect outliers first",
                "결측치 제외",
                "detect outliers first.",
                "결측치 포함",
            ],
            &["a"],
        );
        lint(&mut a);
        // 중복은 뒤의 것이 남는다(나중 지시 우선)
        let got: Vec<&str> = a.constraints.iter().map(|c| c.expression.as_str()).collect();
        assert_eq!(got, ["detect outliers first.", "결측치 포함"]);
        let removals: Vec<(&str, &[usize])> = warns(&a)
            .iter()
            .filter(|d| d.message.contains("제거"))
            .map(|d| (d.message.as_str(), d.indices.as_slice()))
            .collect();
        assert_eq!(
            removals,
            [
                (
                    "중복 제약 제거: \"Detect outliers first\" (뒤의 같은 제약 유지: \"detect outliers first.\")",
                    [0, 2].as_slice()
                ),
                (
                    "모순 지시 자동제거: \"결측치 제외\" ↔ \"결측치 포함\" (뒤의 지시 유지)",
                    [1, 3].as_slice()
                ),
            ]
        );
        // 같은 문장이 그대로 반복되면 본문을 한 번만 적는다
        let mut b = ast(&["x", "x"], &["a"]);
        lint(&mut b);
        assert_eq!(warns(&b)[0].message, "중복 제약 제거: \"x\" (뒤의 같은 제약 유지)");
        assert_eq!(warns(&b)[0].indices, [0, 1]);
    }

    /// 리뷰 재현: "탐지해", "탐지하지 마", "탐지해"는 마지막 지시가 이긴다. 예전에는 중복 제거가 첫 "탐지해"를 남겨
    /// 모순 제거가 그것을 지웠고 "탐지하지 마"만 남았다.
    #[test]
    fn duplicates_keep_the_last_copy_so_the_later_instruction_wins() {
        let mut a = ast(&["이상치 탐지해", "이상치 탐지하지 마", "이상치 탐지해"], &["a"]);
        lint(&mut a);
        let got: Vec<&str> = a.constraints.iter().map(|c| c.expression.as_str()).collect();
        assert_eq!(got, ["이상치 탐지해"]);
        assert_eq!(a.constraints[0].input_index(), Some(2), "남은 것은 마지막 입력이다");
        let w: Vec<(&str, &[usize])> = warns(&a)
            .iter()
            .map(|d| (d.message.as_str(), d.indices.as_slice()))
            .collect();
        assert_eq!(
            w,
            [
                (
                    "중복 제약 제거: \"이상치 탐지해\" (뒤의 같은 제약 유지)",
                    [0, 2].as_slice()
                ),
                (
                    "모순 지시 자동제거: \"이상치 탐지하지 마\" ↔ \"이상치 탐지해\" (뒤의 지시 유지)",
                    [1, 2].as_slice()
                ),
            ]
        );
        // 반대 순서도 마지막 지시가 남는다
        assert_eq!(
            kept(&["이상치 탐지하지 마", "이상치 탐지해", "이상치 탐지하지 마"]),
            ["이상치 탐지하지 마"]
        );
        // 세 번 반복되면 앞의 두 개를 지우고, 경고마다 지운 것과 남긴 것의 위치를 단다
        let mut c = ast(&["a b", "A b.", "a  B"], &["a"]);
        lint(&mut c);
        assert_eq!(c.constraints.len(), 1);
        assert_eq!(c.constraints[0].expression, "a B");
        let idx: Vec<&[usize]> = warns(&c).iter().map(|d| d.indices.as_slice()).collect();
        assert_eq!(idx, [[0, 2].as_slice(), [1, 2].as_slice()]);
    }

    #[test]
    fn fillers_inside_quotes_are_kept() {
        for (input, want) in [
            ("'매우 만족' 응답 비율을 계산해", "'매우 만족' 응답 비율을 계산해"),
            ("Count \"very satisfied\" answers", "Count \"very satisfied\" answers"),
            (
                "매우 중요: '매우 만족'을 선택한 비율",
                "중요: '매우 만족'을 선택한 비율",
            ),
            (
                "Use `very` and “very  good” and ‘really’ labels",
                "Use `very` and “very  good” and ‘really’ labels",
            ),
            ("Don't be very vague, it's really simple", "Don't be vague, it's simple"),
            ("(very important) list causes", "(important) list causes"),
            ("Be careful, very.", "Be careful,"),
            ("Analyze it very.", "Analyze it."),
            ("Please, analyze", "analyze"),
            (
                "이상치는   z-score 3 기준으로\n    판단",
                "이상치는 z-score 3 기준으로 판단",
            ),
            ("라벨 \"a\nb\" 제외", "라벨 \"a b\" 제외"),
            ("very  very", "very very"),
        ] {
            assert_eq!(kept(&[input]), [want], "{input:?}");
        }
    }

    #[test]
    fn role_keeps_line_breaks() {
        let mut a = ast(&[], &["a"]);
        a.role.instruction =
            "  Please analyze\r\n  very   carefully\n\n\n\n- 매우 짧게 \t 답해\n'very  quoted'\n".into();
        lint(&mut a);
        assert_eq!(a.role.instruction, "analyze\ncarefully\n\n- 짧게 답해\n'very  quoted'");
        let once = a.role.instruction.clone();
        lint(&mut a);
        assert_eq!(a.role.instruction, once);
    }

    #[test]
    fn quote_spans_skip_apostrophes() {
        let spans = |s: &str| {
            quote_spans(s)
                .into_iter()
                .map(|(a, b)| s[a..b].to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(spans("don't say 'it's fine' or users' 'x'"), ["'it's fine'", "'x'"]);
        assert_eq!(spans("'매우 만족'을 ‘don’t’ “a”"), ["'매우 만족'", "‘don’t’", "“a”"]);
        assert_eq!(spans("``a ` b`` and `c` and ```"), ["``a ` b``", "`c`"]);
        assert_eq!(spans("5' and \"open"), Vec::<String>::new());
    }

    #[test]
    fn diagnostic_indices_default_when_missing() {
        // 위치가 없던 진단(또는 예전 `index` 필드만 있는 JSON)도 읽힌다
        for json in [
            r#"{"severity":"WARN","message":"m"}"#,
            r#"{"severity":"WARN","message":"m","index":2}"#,
        ] {
            let d: LintDiagnostic = serde_json::from_str(json).unwrap();
            assert!(d.indices.is_empty(), "{json}");
        }
        let d = LintDiagnostic::for_constraints("WARN", "m", vec![2, 0]);
        assert_eq!(d.indices, [2, 0]);
        assert!(LintDiagnostic::new("INFO", "m").indices.is_empty());
        assert_eq!(ConstraintNode::classify("C3".into(), "x").input_index(), Some(2));
        assert_eq!(ConstraintNode::classify(String::new(), "x").input_index(), None);
        assert_eq!(ConstraintNode::classify("C0".into(), "x").input_index(), None);
    }

    /// 2차 리뷰에서 재현한 오탐: 예외·대조 조항, 명사 끝 글자 어간, 설명형 증가/감소·부정, 어순, 단어 속 "금지".
    #[test]
    fn exception_clauses_and_descriptions_are_not_removed() {
        // 둘 다 남기고 모순 의심 WARN만
        for (a, b) in [
            ("Use the median instead of the mean", "Do not use the mean"),
            ("Round to 2 decimals except percentages", "Do not round percentages"),
            ("평균 대신 중앙값을 사용해", "평균은 사용하지 마"),
            ("취소 건은 제외하고 주문 수를 집계해", "취소 건은 집계하지 마"),
            ("매출 증가 원인을 분석해", "매출 감소 원인을 분석해"),
            ("Treat empty cells as zero", "Do not treat zero as empty"),
            ("할인가는 표시하지 마", "할인 금액을 표시해"),
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            let w = warns(&t);
            assert!(
                w.len() == 1 && w[0].message.starts_with("모순 의심"),
                "{a} / {b}: {w:?}"
            );
        }
        // 경고도 없이 둘 다 남김
        for (a, b) in [
            ("1천만 원 이상 거래는 표시하지 마", "1천 원 이상 거래를 표시해"),
            ("구매하지 않은 고객을 표시해", "구매한 고객을 표시해"),
            ("Flag customers who never purchased", "Flag customers"),
            ("보험금지급 내역을 요약해", "보험 내역을 요약해"),
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            assert!(warns(&t).is_empty(), "{a} / {b}: {:?}", warns(&t));
        }
        for s in [
            "구매하지 않은 고객을 표시해",
            "Flag customers who never purchased",
            "보험금지급 내역을 요약해",
            "금지어 목록을 출력해",
        ] {
            assert!(!is_negative(s), "{s}");
        }
        for s in [
            "추측은 지양",
            "외부 링크 사용 자제",
            "id 컬럼 사용금지",
            "결측치는 포함하지 않는다",
        ] {
            assert!(is_negative(s), "{s}");
        }
        // 증가/감소는 서술어 자리(처음·끝)에 있어도 확신할 수 없다: 둘 다 남기고 모순 의심만 남긴다.
        // (예전 기대값 "뒤의 것만 남음"은 리뷰가 찾은 오탐 규칙 그대로였다: "Explain revenue increase/decrease"도 지웠다.)
        for (a, b) in [
            ("Increase the sample size", "Decrease the sample size"),
            ("매출을 증가시켜라", "매출을 감소시켜라"),
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            let w = warns(&t);
            assert!(
                w.len() == 1 && w[0].message.starts_with("모순 의심"),
                "{a} / {b}: {w:?}"
            );
        }
    }

    /// 리뷰 재현: 지시가 아닌 반의어(증가/감소), 부정사·하이픈 복합어의 avoid/never, 목적절 "-지 않도록"이
    /// 확실한 모순으로 판정되어 앞 제약이 지워졌다. 둘 다 남아야 한다(모순 의심 경고는 괜찮다).
    #[test]
    fn descriptive_antonyms_and_negations_keep_both_rules() {
        for (a, b) in [
            ("Explain revenue increase", "Explain revenue decrease"),
            ("List products that increased", "List products that decreased"),
            ("재고 증가", "재고 감소"),
            ("증가 추세 설명", "감소 추세 설명"),
            ("Suggest ways to avoid churn", "Suggest ways to reduce churn"),
            ("List risks to avoid", "List risks by region"),
            ("Flag never-paid invoices", "Flag paid invoices"),
            ("고객이 이탈하지 않도록 할 방안을 제시해", "고객 이탈 방안을 제시해"),
            ("고객이 이탈하지 않게 하는 방법을 제시해", "고객 이탈 방법을 제시해"),
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            let got: Vec<&str> = t.constraints.iter().map(|c| c.expression.as_str()).collect();
            assert_eq!(got, [a, b], "{a} / {b}");
            let w = warns(&t);
            assert!(w.iter().all(|d| d.message.starts_with("모순 의심")), "{a} / {b}: {w:?}");
        }
        // 설명형 부정은 금지가 아니다
        for s in [
            "Suggest ways to avoid churn",
            "List risks to avoid",
            "Explain how to never miss a deadline",
            "To avoid churn, suggest discounts",
            "Flag never-paid invoices",
            "Call the do-not-call list owners",
            "고객이 이탈하지 않도록 할 방안을 제시해",
            "누락되지 않도록 하는 방법을 설명해",
            "결측치가 섞이지 않게 할 방안",
            "이상치가 누락되지 않도록 검토해",
        ] {
            assert!(!is_negative(s), "{s}");
        }
        // 금지 지시는 그대로 부정이다: 명령형 avoid/never, 금지 틀 뒤의 부정사, "-지 않도록" + 지시
        for s in [
            "Avoid jargon",
            "Never round",
            "Try to avoid jargon",
            "Make sure to never round values",
            "You must remember to avoid guessing",
            "It is important to avoid jargon",
            "결측치가 포함되지 않도록 해",
            "결측치가 포함되지 않도록 하라",
            "결측치가 포함되지 않도록 하세요",
            "결측치가 포함되지 않도록 해줘",
            "결측치가 포함되지 않도록 해 주세요",
            "결측치가 포함되지 않도록 할 것",
            "결측치가 포함되지 않도록 주의해",
            "결측치가 포함되지 않도록 꼭 해",
            "결측치가 포함되지 않도록",
            "결측치가 포함되지않도록해",
            "중복되지 않게 해",
            "중복되지 않게끔 하세요",
        ] {
            assert!(is_negative(s), "{s}");
        }
        // 금지 틀이 있는 부정사는 사실성 규칙도 그대로다("Try to avoid guessing")
        assert_eq!(
            ConstraintNode::classify("C1".into(), "Try to avoid guessing").rule_type,
            ConstraintType::FactualityRule
        );
        // 확실한 모순은 계속 지운다
        for (a, b) in [
            ("이상치 우선 탐지", "이상치 탐지하지 마"),
            ("결측치 제외", "결측치 포함"),
            ("Detect outliers", "You must not detect outliers"),
            ("Use jargon", "Avoid jargon"),
            ("Round values", "Never round values"),
            ("결측치를 포함해", "결측치를 포함하지 않도록 해"),
        ] {
            assert_eq!(kept(&[a, b]), [b], "{a} / {b}");
        }
    }

    /// 리뷰 재현: `--fields "a,b,a,b"`의 경고가 지운 필드 이름을 적지 않았다(요구사항 4).
    #[test]
    fn schema_warning_names_the_removed_fields() {
        let schema_warn = |fields: &[&str]| {
            let mut a = ast(&[], fields);
            lint(&mut a);
            let w: Vec<String> = a
                .lint_warnings
                .iter()
                .filter(|d| d.message.starts_with("출력 스키마의 빈/중복 필드 제거됨"))
                .map(|d| d.message.clone())
                .collect();
            (w, a.output_schema.required_fields)
        };
        let (w, kept) = schema_warn(&["a", "b", "a", "b"]);
        assert_eq!(w, ["출력 스키마의 빈/중복 필드 제거됨: 중복 \"a\", \"b\""]);
        assert_eq!(kept, ["a", "b"]);
        let (w, kept) = schema_warn(&["x", " ", "x", "x", "", "y"]);
        assert_eq!(w, ["출력 스키마의 빈/중복 필드 제거됨: 중복 \"x\" ×2, 빈 필드 2개"]);
        assert_eq!(kept, ["x", "y"]);
        let (w, _) = schema_warn(&["a", "b"]);
        assert!(w.is_empty());
    }

    #[test]
    fn more_quote_styles_protect_fillers() {
        for s in [
            "「매우 만족」 응답 비율",
            "『아주 좋음』 비율",
            "《very good》 share",
            "«very good» share",
            "〈매우〉 표시",
        ] {
            assert_eq!(kept(&[s]), [s], "{s}");
        }
    }
}
