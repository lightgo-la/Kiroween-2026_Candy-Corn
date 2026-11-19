//! Visitor-based lint passes: filler compression, duplicate removal, contradiction detection, schema integrity check.
//!
//! Contradiction detection is heuristic. A sentence is split into word, number, operator and quoted-value tokens,
//! negation markers are matched as whole words, and Korean words are compared after stripping particles and endings
//! ("tamji-hae"/"tamji-haji" → "tamji", detect; "jumun-geumaek", order amount, stays as is).
//! A negation that describes the target is not a prohibition: relative clauses ("customers who never purchased"),
//! infinitives ("ways to avoid churn"), hyphenated compounds ("never-paid invoices"), adnominal forms
//! ("gumaehaji aneun gogaek", customers who did not buy) and purpose clauses ("italhaji anhdorok hal bangan", a plan
//! so that they do not churn).
//! - Opposite polarity (positive/negative) where every content word of the negative side is also on the positive side
//!   (the negation covers the positive instruction) is a contradiction. When the negation is narrower, as in
//!   "Detect outliers" ↔ "Never detect outliers in the id column", both are kept.
//! - Same polarity where only a directive antonym (exclude/include, allow/forbid, ascending/descending) is opposite and
//!   the rest of the content is the same is also a contradiction. Antonyms that also describe data, such as
//!   increase/decrease, are uncertain and only warn.
//!
//! A certain contradiction removes the earlier constraint (the later instruction wins). Uncertain pairs are both kept
//! with a warning only.

use super::*;
use crate::lexicon;
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

/// Runs every pass in order.
pub fn lint(ast: &mut RootPromptAst) {
    walk(ast, &mut FillerPass::default());
    walk(ast, &mut DedupPass::default());
    walk(ast, &mut ContradictionPass::default());
    walk(ast, &mut SchemaPass::default());
}

// ---------- Vocabulary ----------
// The Korean word lists are data for judging input sentences, so they are used as is regardless of the UI language
// (they live in locales/lexicon.ko.core.json and come in through `crate::lexicon`).

/// English filler words (the Korean ones are `lexicon::LINTER_KO_FILLERS`).
const FILLERS: [&str; 7] = ["very", "really", "extremely", "highly", "please", "kindly", "simply"];

/// Quote characters. A word with one of these attached is not removed even if it is a filler.
const QUOTE_CHARS: [char; 17] = [
    '\'', '"', '“', '”', '‘', '’', '`', '「', '」', '『', '』', '《', '》', '〈', '〉', '«', '»',
];

/// Quotes and bracket quotes whose opening and closing characters differ. They are never used as apostrophes, so a
/// matching closer always closes them.
const PAIRED_QUOTES: [(char, char); 6] = [
    ('“', '”'),
    ('「', '」'),
    ('『', '』'),
    ('《', '》'),
    ('〈', '〉'),
    ('«', '»'),
];

/// English negation phrases. They are matched as whole words, so "can treat" (can t), "nevertheless" (never) and
/// "do note" (do not) are not negations. An apostrophe splits words, so "don't" becomes ["don", "t"].
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

/// "-ji ma/mal" prohibitive imperatives (longest first). Negative when the previous word ends in "-ji"
/// ("tamjihaji ma", do not detect) or when written glued to it ("hajimara").
const KO_NEG_IMPERATIVES: &[&str] = lexicon::LINTER_KO_NEG_IMPERATIVES;

/// Function words left out of content comparison (English stopwords; the Korean auxiliary expressions are
/// `lexicon::LINTER_KO_STOPWORDS`).
#[rustfmt::skip]
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "you", "your", "we", "our", "us", "i", "me", "my", "it", "its", "this", "that", "these", "those",
    "please", "kindly", "must", "should", "shall", "will", "would", "can", "could", "may", "might", "always",
    "do", "does", "did", "be", "is", "are", "was", "were", "been",
    "to", "of", "in", "on", "at", "by", "for", "from", "with", "into", "as", "and", "or", "any", "also", "just", "then",
];

/// Korean endings and particles (longest first). They are stripped only when a stem remains, so "tamji-hae" and
/// "tamji-haji" match "tamji" (detect) and "isangchi-neun" matches "isangchi" (outlier).
/// "ga", "do" and "man", common as the last syllable of nouns ("harin-ga" discount price, "kadeu-hando" card limit,
/// "1-cheon-man" ten million), are not stripped in the strict comparison (KO_LOOSE_SUFFIXES).
const KO_SUFFIXES: &[&str] = lexicon::LINTER_KO_SUFFIXES;

/// Particles stripped only in the loose comparison used for warnings ("isangchi-ga"/"-do"/"-man": outlier + subject
/// particle / "also" / "only").
const KO_LOOSE_SUFFIXES: &[&str] = lexicon::LINTER_KO_LOOSE_SUFFIXES;

/// Words that open a relative clause. A negation right after one describes the target instead of forbidding
/// ("customers who never purchased").
const RELATIVES: &[&str] = &["who", "that", "which", "whose", "where"];

/// Words that make the infinitives "to avoid" and "to never" read as prohibitions ("Try to avoid jargon", "Make sure
/// to never round"). It is a prohibition only when everything from the start of the sentence up to "to" is such
/// words and stopwords; other infinitives ("Suggest ways to avoid churn", "List risks to avoid", "To avoid churn, …")
/// only describe the target.
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

/// Hyphens that join words. A negation word joined by a hyphen, as in "never-paid", is part of a compound (a
/// description).
const HYPHENS: [char; 3] = ['-', '\u{2010}', '\u{2011}'];

/// Adnominal negation ("gumaehaji aneun gogaek", customers who did not buy): describes the target instead of
/// forbidding.
const KO_DESCRIPTIVE_NEG: &[&str] = lexicon::LINTER_KO_DESCRIPTIVE_NEG;

/// Purpose/result endings after "anh" ("-ji anhdorok", "-ji anhge(kkeum)": so that ... not). Followed by a directive
/// (KO_PURPOSE_DIRECTIVES) or at the end of the sentence they are a prohibition ("pohamdoeji anhdorok hae", make sure
/// it is not included; "nurakdoeji anhdorok", so that nothing is left out); followed by other words they are a
/// purpose clause ("italhaji anhdorok hal bangan-eul jesihae", suggest a plan so that they do not churn).
const KO_PURPOSE: &[&str] = lexicon::LINTER_KO_PURPOSE;

/// Directive forms of "hada" (do) that turn a preceding purpose clause into a prohibition. "hal" is a directive only
/// when "geot" follows ("hal geot", to be done).
const KO_PURPOSE_DIRECTIVES: &[&str] = lexicon::LINTER_KO_PURPOSE_DIRECTIVES;

/// Beginnings of words that are prohibitive directives even after a purpose clause ("juuihae", "yuuihaseyo",
/// "josimhal geot": be careful).
const KO_PURPOSE_CAUTIONS: &[&str] = lexicon::LINTER_KO_PURPOSE_CAUTIONS;

/// Korean markers that are negative at the end of a word (or before a ban ending). The part before them stays as
/// content ("sayong-geumji", use prohibited → "sayong").
const KO_BANS: &[&str] = lexicon::LINTER_KO_BANS;

/// Endings that keep "geumji" (prohibited) a ban when attached ("sayong-geumji", "geumji-handa"). With other letters
/// attached ("geumji-eo" banned word, "boheomgeum-jigeup" insurance payout, "geumji-doen" prohibited as a modifier)
/// it is not a ban.
const KO_BAN_ENDINGS: &[&str] = lexicon::LINTER_KO_BAN_ENDINGS;

/// Exception and contrast words. When only the positive constraint has one, the negative constraint may just repeat
/// that exception, so nothing is removed and a warning is given instead ("Use the median instead of the mean" +
/// "Do not use the mean", and the same pair in Korean with "daesin", instead of). The Korean words are
/// `lexicon::LINTER_KO_EXCEPTION_WORDS`.
#[rustfmt::skip]
const EXCEPTION_WORDS: &[&str] = &[
    "instead", "except", "excluding", "exclude", "excludes", "excluded", "without", "unless", "besides", "rather",
    "not",
];

/// Word pairs used with opposite meanings (English with inflected forms, Korean as stems), and whether the pair is a
/// certain contradiction when the rest of the content is the same.
/// Only directive pairs (exclude/include etc., true) are certain. Pairs often used to describe data, such as
/// increase/decrease (false), only warn wherever they appear ("analyze the cause of the revenue increase" + "... of
/// the revenue decrease", "inventory increase" + "inventory decrease"). The Korean sides come from the lexicon.
type WordSet = &'static [&'static str];
const ANTONYMS: [(WordSet, WordSet, bool); 8] = [
    (lexicon::LINTER_KO_EXCLUDE, lexicon::LINTER_KO_INCLUDE, true),
    (
        &["exclude", "excludes", "excluded", "excluding", "exclusion"],
        &["include", "includes", "included", "including", "inclusion"],
        true,
    ),
    (lexicon::LINTER_KO_ALLOW, lexicon::LINTER_KO_FORBID, true),
    (
        &["allow", "allows", "allowed", "allowing"],
        &["forbid", "forbids", "forbidden", "forbidding"],
        true,
    ),
    (lexicon::LINTER_KO_INCREASE, lexicon::LINTER_KO_DECREASE, false),
    (
        &["increase", "increases", "increased", "increasing"],
        &["decrease", "decreases", "decreased", "decreasing"],
        false,
    ),
    (lexicon::LINTER_KO_ASCENDING, lexicon::LINTER_KO_DESCENDING, true),
    (&["ascending"], &["descending"], true),
];

/// Comparison operator characters. A run of them is one operator token (>=, <>, ==).
const CMP_CHARS: [char; 6] = ['<', '>', '=', '≤', '≥', '≠'];
/// Symbols that matter when attached to a number (sign, arithmetic, range). Away from numbers, like the hyphen in
/// "z-score", they are separators.
const ARITH_CHARS: [char; 10] = ['+', '-', '−', '–', '*', '/', '^', '~', '×', '÷'];

// ---------- Quotes and tokens ----------

/// Byte ranges [start, end) of quoted spans, up to and including the closing quote:
/// '…' "…" “…” ‘…’ `…` (backtick runs of the same length) 「…」 『…』 《…》 〈…〉 «…».
/// Apostrophes of English contractions and possessives (don't, it's, users') do not open a quote. An unmatched quote
/// is an ordinary character. A quote also closes when a particle follows the closing quote, as in Korean
/// ('maeu manjok'-eul: "very satisfied" + object particle).
fn quote_spans(s: &str) -> Vec<(usize, usize)> {
    let cs: Vec<(usize, char)> = s.char_indices().collect();
    let n = cs.len();
    let ch = |k: usize| cs.get(k).map(|&(_, c)| c);
    // ' or ’ with an ASCII letter or digit before it and an ASCII letter after it is an apostrophe inside a word (don't).
    let apostrophe = |k: usize| {
        k > 0
            && ch(k - 1).is_some_and(|c| c.is_ascii_alphanumeric())
            && ch(k + 1).is_some_and(|c| c.is_ascii_alphabetic())
    };
    let positions = |pred: &dyn Fn(usize) -> bool| (0..n).filter(|&k| pred(k)).collect::<Vec<_>>();
    let double = positions(&|k| cs[k].1 == '"');
    // Paired quotes (“” 「」 『』 《》 〈〉 «»): positions of each closing character
    let paired: Vec<(char, Vec<usize>)> = PAIRED_QUOTES
        .iter()
        .map(|&(open, close)| (open, positions(&|k| cs[k].1 == close)))
        .collect();
    let single = positions(&|k| cs[k].1 == '\'' && !apostrophe(k));
    let curly_single = positions(&|k| cs[k].1 == '’' && !apostrophe(k));
    // Backtick runs: length → start positions
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
        // Position of the character after the closing quote
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

/// Token for meaning comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// Lowercase word or number (including decimal points and thousands commas between digits).
    /// `hyphened`: joined to the previous or next word by a hyphen ("never" and "paid" in "never-paid").
    Word { text: String, hyphened: bool },
    /// Comparison operator, sign or arithmetic symbol attached to a number, or %
    Op(String),
    /// Value inside quotes (whitespace collapsed to single spaces, case kept)
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

/// Splits text outside quotes into tokens. Other punctuation is a separator.
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
            // A compound only when there are letters on both sides of the hyphen ("never-paid"). "- never" (a list
            // marker) and "-1" (a sign) are not.
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

/// Duplicate key: ignores only case, punctuation and whitespace, and keeps numbers, signs, comparison operators and
/// quoted values. "Flag amount > 1000" and "Flag amount < 1000", or "-1" and "1", are different constraints.
fn dedup_key(s: &str) -> String {
    let toks = lex(s);
    if toks.is_empty() {
        // Sentences without words ("|", "&") are compared as written
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

/// Hangul syllable (U+AC00..=U+D7A3).
fn is_hangul(c: char) -> bool {
    ('\u{AC00}'..='\u{D7A3}').contains(&c)
}

/// Strips particles and endings from the end of a Hangul word. It strips only when the remaining stem ends in Hangul
/// and has at least 2 syllables (strict comparison) or 1 syllable (loose comparison), so "won-ga" (cost),
/// "jeung-ga" (increase) and "10-man" (100,000) stay as they are, and so does a word without a particle such as
/// "jumun-geumaek" (order amount). The loose comparison also strips "ga", "do" and "man", but keeps "man" after a
/// number unit ("1-cheon-man", "sip-man").
fn stem(w: &str, loose: bool) -> &str {
    let (min, extra) = if loose { (1, KO_LOOSE_SUFFIXES) } else { (2, &[][..]) };
    let mut cur = w;
    while let Some(base) = KO_SUFFIXES.iter().chain(extra).find_map(|suf| {
        cur.strip_suffix(suf).filter(|b| {
            let last = b.chars().next_back();
            b.chars().count() >= min
                && last.is_some_and(is_hangul)
                && !(*suf == lexicon::LINTER_KO_ONLY_PARTICLE
                    && last.is_some_and(|c| lexicon::LINTER_KO_NUMBER_UNITS.contains(c)))
        })
    }) {
        cur = base;
    }
    cur
}

/// Loose stem for warnings: folds English plurals and strips Hangul words down to one-syllable stems
/// ("gapseul"/"gapseun" → "gap", value).
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

/// For an antonym word, returns (pair index, side). `w` is a stem.
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
    STOPWORDS.contains(&w) || lexicon::LINTER_KO_STOPWORDS.contains(&w)
}

fn is_exception_word(w: &str) -> bool {
    EXCEPTION_WORDS.contains(&w) || lexicon::LINTER_KO_EXCEPTION_WORDS.contains(&w)
}

/// Constraint summary for contradiction checks.
#[derive(Debug, Default)]
struct RuleInfo {
    negative: bool,
    /// Content without negation markers and stopwords (stems, operators, quoted values) in sentence order. Used to see
    /// whether a negation covers a positive instruction.
    content: Vec<String>,
    /// Content with plurals and short stems folded too. Used only to warn about uncertain contradictions.
    loose: BTreeSet<String>,
    /// Antonyms as (pair index, side)
    antonyms: Vec<(usize, bool)>,
    /// Content without the antonym words
    rest: BTreeSet<String>,
    /// Exception and contrast words (instead, except, Korean "daesin"/"jeoe", the adnominal "aneun" that modifies the
    /// target, the purpose-clause "anhdorok" …)
    exceptions: BTreeSet<String>,
    /// The sentence lists several items ("Detect outliers and duplicates"). A later negation of one item then does
    /// not prove that the whole sentence is withdrawn.
    coordinated: bool,
}

/// Whether the sentence lists items: an "and"/"or" word (or a Korean coordinator) or a comma between words. A comma
/// inside a number (1,000) does not count.
fn is_coordinated(s: &str, toks: &[Tok]) -> bool {
    let word = |t: &Tok| {
        matches!(t, Tok::Word { text, .. }
            if text == "and" || text == "or" || lexicon::LINTER_KO_COORDINATORS.contains(&text.as_str()))
    };
    let digit_comma = |i: usize| {
        s[..i].ends_with(|d: char| d.is_ascii_digit()) && s[i + 1..].starts_with(|d: char| d.is_ascii_digit())
    };
    toks.iter().any(word) || s.char_indices().any(|(i, c)| c == ',' && !digit_comma(i))
}

/// Position of a Korean ban marker at the end of a word (or before a ban ending). "geumji-eo", "boheomgeum-jigeup"
/// and "geumji-doen" are not bans.
fn ban_at(w: &str) -> Option<usize> {
    KO_BANS.iter().find_map(|m| {
        let at = w.rfind(m)?;
        KO_BAN_ENDINGS.contains(&&w[at + m.len()..]).then_some(at)
    })
}

/// If the word `w` containing "anh" is a purpose clause ("-ji anhdorok", "-ji anhge"), returns whether it is a
/// prohibitive directive. `after` holds the tokens that follow it.
/// It is a prohibition when a directive ("hae", "haseyo", "hae juseyo", "hal geot", "juuihae" …) is attached or
/// follows, or when the sentence ends. Other words ("hal bangan", a plan to; "haneun bangbeop", a way to;
/// "geomtohae", review) only describe the purpose. None if it is not a purpose clause.
fn purpose_ban(w: &str, after: &[Tok]) -> Option<bool> {
    let at = w.find(lexicon::LINTER_KO_NEG_AUX)?;
    let ending = &w[at + lexicon::LINTER_KO_NEG_AUX.len()..];
    let attached = KO_PURPOSE.iter().find_map(|p| ending.strip_prefix(p))?;
    // Skip emphasis adverbs ("kkok", "bandeusi": be sure to) when looking ("nurakdoeji anhdorok kkok hae")
    let words: Vec<&str> = after
        .iter()
        .map(|t| match t {
            Tok::Word { text, .. } => text.as_str(),
            _ => "",
        })
        .filter(|w| !lexicon::LINTER_KO_EMPHASIS.contains(w))
        .collect();
    let directive = |d: &str, then: Option<&&str>| {
        KO_PURPOSE_DIRECTIVES.contains(&d)
            || (d == lexicon::LINTER_KO_DO_PROSPECTIVE && then == Some(&lexicon::LINTER_KO_NOMINALIZER))
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
    // framed[i]: everything before token i, from the start of the sentence, is prohibition frames ("Try",
    // "Make sure", "Be careful") and stopwords, with at least one frame
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
    // A negation inside a relative clause ("customers who never purchased") or an infinitive ("ways to avoid churn")
    // only describes the target; it is not a prohibition
    let explained = |k: usize| match k.checked_sub(1).and_then(word) {
        Some(w) if RELATIVES.contains(&w) => true,
        Some("to") => !framed[k - 1],
        _ => false,
    };
    // An adnominal negation ("gumaehaji aneun gogaek", customers who did not buy) only modifies the next word; it is
    // not a prohibition
    let adnominal = |k: usize, w: &str| k + 1 < toks.len() && KO_DESCRIPTIVE_NEG.iter().any(|d| w.ends_with(d));
    // Negation marker tokens, and what is left after removing a glued marker ("sayonggeumji" → "sayong",
    // "tamjihajima" → "tamjihaji")
    let mut marker = vec![false; toks.len()];
    let mut residual: Vec<Option<&str>> = vec![None; toks.len()];
    // Korean negations that describe the target or a purpose ("aneun gogaek", "anhdorok hal bangan"). Counted as
    // exception/contrast words.
    let mut explains = vec![false; toks.len()];
    // The Korean words checked in the loop below ("mal geot", "malgeot") are prohibitive-marker vocabulary.
    for k in 0..toks.len() {
        for p in NEG_PHRASES {
            // The negation word of a hyphenated compound ("never-paid", "do-not-call") is a description
            if p.iter().enumerate().all(|(o, w)| word(k + o) == Some(*w))
                && !explained(k)
                && !(k..k + p.len()).any(hyphened)
            {
                marker[k..k + p.len()].fill(true);
            }
        }
        let Some(w) = word(k) else { continue };
        let not_at = w.find(lexicon::LINTER_KO_NEG_AUX);
        if not_at.is_some() {
            explains[k] = !purpose_ban(w, &toks[k + 1..]).unwrap_or_else(|| !adnominal(k, w));
        }
        if let Some(at) = not_at.filter(|_| !explains[k]).or_else(|| ban_at(w)) {
            marker[k] = true;
            residual[k] = Some(&w[..at]);
        } else if w == lexicon::LINTER_KO_PROHIBITIVE && word(k + 1) == Some(lexicon::LINTER_KO_NOMINALIZER) {
            marker[k] = true;
            marker[k + 1] = true;
        } else if KO_NEG_IMPERATIVES.contains(&w)
            && k > 0
            && word(k - 1).is_some_and(|p| p.ends_with(lexicon::LINTER_KO_NEG_CONNECTIVE))
        {
            marker[k] = true;
        } else if let Some(base) = w
            .strip_suffix(lexicon::LINTER_KO_PROHIBITIVE_GLUED)
            .filter(|b| !b.is_empty())
            .or_else(|| {
                KO_NEG_IMPERATIVES.iter().find_map(|f| {
                    w.strip_suffix(f)
                        .filter(|b| b.ends_with(lexicon::LINTER_KO_NEG_CONNECTIVE) && b.chars().count() >= 2)
                })
            })
        {
            marker[k] = true;
            residual[k] = Some(base);
        }
    }
    let mut info = RuleInfo {
        negative: marker.contains(&true),
        coordinated: is_coordinated(s, &toks),
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
                if is_exception_word(w) || is_exception_word(strict) || explains[k] {
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
/// Whether the sentence is a negative instruction. Values inside quotes ("do not know") and negations that describe
/// the target or a purpose ("aneun gogaek", "ways to avoid churn", "never-paid invoices",
/// "italhaji anhdorok hal bangan") are not counted.
pub fn is_negative(s: &str) -> bool {
    analyze(s).negative
}

#[derive(Debug, PartialEq, Eq)]
enum Relation {
    Unrelated,
    /// Certain contradiction: the earlier constraint is removed
    Clash,
    /// Possible contradiction: both are kept with a warning only
    Suspect,
}

/// Whether the English words of `needle` appear in `hay` in the same order (other words may come between).
/// Korean word order is free because particles mark the roles, so the order of Korean words is not checked.
fn in_order(needle: &[String], hay: &[String]) -> bool {
    let mut rest = hay.iter().filter(|w| w.is_ascii());
    needle.iter().filter(|w| w.is_ascii()).all(|n| rest.any(|h| h == n))
}

fn relation(a: &RuleInfo, b: &RuleInfo) -> Relation {
    if a.negative != b.negative {
        let (neg, pos) = if a.negative { (a, b) } else { (b, a) };
        let pos_words: BTreeSet<&String> = pos.content.iter().collect();
        let covered = !neg.content.is_empty() && neg.content.iter().all(|w| pos_words.contains(w));
        // A certain contradiction only when the negation covers the positive instruction in the same word order and
        // the positive side has no exception/contrast word of its own ("instead", "daesin"). A narrower negation (an
        // exception clause) does not cover it, so it is not a contradiction.
        // "Detect outliers and duplicates" + "Do not detect outliers": the negation names one item of a list, so the
        // other item must survive (both are kept with a warning).
        let item_of_list = pos.coordinated && pos.content.iter().any(|w| !neg.content.contains(w));
        if covered && !item_of_list && in_order(&neg.content, &pos.content) && pos.exceptions.is_subset(&neg.exceptions)
        {
            return Relation::Clash;
        }
        if covered || (!neg.loose.is_empty() && neg.loose.is_subset(&pos.loose)) {
            return Relation::Suspect;
        }
        return Relation::Unrelated;
    }
    // Same polarity: antonyms that appear on only one side must be opposite (a sentence that has both sides is
    // skipped). Only opposite directive pairs (exclude/include etc.) with the same remaining content are a certain
    // contradiction. Descriptive pairs such as increase/decrease are uncertain even in predicate position
    // ("Explain revenue increase", "jaego jeungga", inventory increase), so they go no further than a possible
    // contradiction.
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

/// Diagnostic about constraints. Reads the original input positions from the pre-lint IDs (C1..Cn) in the order of
/// `involved` (removals: [removed, kept]; possible contradictions: [earlier, later]).
fn about(involved: &[&ConstraintNode], severity: &str, message: String) -> LintDiagnostic {
    let indices = involved.iter().filter_map(|c| c.input_index()).collect();
    LintDiagnostic::for_constraints(severity, message, indices)
}

// ---------- Fillers and whitespace ----------

fn is_newline(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Whether the word negates what follows ("not", "never", "don't", "cannot", "avoid").
fn is_negation_word(w: &str) -> bool {
    let w = w
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
        .replace('’', "'");
    matches!(
        w.as_str(),
        "not" | "never" | "no" | "cannot" | "avoid" | "dont" | "doesnt"
    ) || w.ends_with("n't")
}

/// If the word is a filler, returns (leading punctuation, trailing punctuation). A word with a quote attached is not
/// a filler.
fn filler_parts(w: &str) -> Option<(&str, &str)> {
    if w.contains(QUOTE_CHARS) {
        return None;
    }
    let start = w.find(char::is_alphanumeric)?;
    let (last, c) = w.char_indices().rev().find(|(_, c)| c.is_alphanumeric())?;
    let end = last + c.len_utf8();
    let word = w[start..end].to_lowercase();
    (FILLERS.contains(&word.as_str()) || lexicon::LINTER_KO_FILLERS.contains(&word.as_str()))
        .then_some((&w[..start], &w[end..]))
}

/// Removes fillers (`drop_fillers`) and tidies whitespace; returns (result, number removed).
/// Inside quotes ('…' "…" “…” ‘…’ `…`) no word is removed and the characters are kept as they are.
/// With `keep_lines`, line breaks are kept (at most one blank line) and only spaces within a line are collapsed (role
/// instruction). Otherwise the text becomes one line, and only line breaks and control characters inside quotes
/// become spaces (a constraint is a one-line slot).
/// Punctuation attached to a removed filler is kept: "(very important)" → "(important)", "do it very." → "do it."
fn compress(s: &str, keep_lines: bool, drop_fillers: bool) -> (String, usize) {
    let text = if keep_lines {
        s.replace("\r\n", "\n")
    } else {
        s.to_string()
    };
    // (line breaks in the whitespace before, word). A quoted span is part of the word even if it contains spaces.
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
    // A filler right after a negation stays: "Do not simply remove rows" must not become "Do not remove rows"
    let mut after_neg = false;
    // Line breaks seen since the last kept word (the larger of the gaps around removed words)
    let mut sep = 0;
    // Opening punctuation that was attached to a removed filler ("(very" → "("). Put in front of the next kept word.
    let mut open = String::new();
    for (lines, w) in words {
        sep = sep.max(lines);
        let filler = filler_parts(&w);
        if let Some((before, after)) = filler.filter(|_| drop_fillers && !after_neg) {
            removed += 1;
            match (before.is_empty(), after.is_empty()) {
                (false, true) => open.push_str(before),
                // Closing punctuation ("very," "very.") goes onto the previous word on the same line. Dropped if that
                // word already ends in punctuation.
                (true, false) if sep == 0 && out.chars().next_back().is_some_and(char::is_alphanumeric) => {
                    out.push_str(after)
                }
                _ => {}
            }
            continue;
        }
        after_neg = is_negation_word(&w) || (after_neg && filler.is_some());
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

// ---------- Passes ----------

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
                // A constraint made only of fillers is not emptied; only its whitespace is tidied
                c.expression = compress(&c.expression, false, false).0;
            } else {
                c.expression = s;
                self.removed += n;
            }
        }
    }
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
        if self.removed > 0 {
            let n = self.removed;
            self.diags.push(LintDiagnostic::new(
                "WARN",
                if n == 1 {
                    trf!("Compressed {n} filler word", n = n)
                } else {
                    trf!("Compressed {n} filler words", n = n)
                },
            ));
        }
        std::mem::take(&mut self.diags)
    }
}

/// Keeps only the last of identical constraints (differing only in case, punctuation and whitespace), so the later
/// instruction wins: "A", "not A", "A" leaves A. Every removed constraint gets a WARN with the removed text.
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
                trf!(
                    "Removed duplicate constraint: \"{}\" (kept the later copy)",
                    c.expression
                )
            } else {
                trf!(
                    "Removed duplicate constraint: \"{}\" (kept the later copy: \"{}\")",
                    c.expression,
                    survivor.expression
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

/// Removes the earlier of two contradicting constraints (the later instruction wins). Uncertain pairs are both kept
/// with a warning.
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
                    trf!(
                        "Removed contradicting instruction: \"{}\" ↔ \"{}\" (kept the later one)",
                        removed.expression,
                        kept.expression
                    ),
                ));
            }
        }
        // Remaining pairs that are uncertain because they differ only in inflection get a warning only. Both
        // constraints get the position (for the GUI badge).
        for j in (0..n).filter(|&j| !drop[j]) {
            for i in (0..j).filter(|&i| !drop[i]) {
                if relation(&infos[i], &infos[j]) == Relation::Suspect {
                    self.diags.push(about(
                        &[&constraints[i], &constraints[j]],
                        "WARN",
                        trf!("Possible contradiction (kept both): \"{}\" ↔ \"{}\". Check whether they give opposite instructions for the same thing", constraints[i].expression, constraints[j].expression),
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

/// Output schema check: removes blank fields and duplicate fields (the first one is kept) and names the removed fields
/// in a WARN.
#[derive(Default)]
pub struct SchemaPass {
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for SchemaPass {
    fn visit_output(&mut self, out: &mut OutputSchemaAst) {
        let mut seen = HashSet::new();
        // Removed duplicate fields (in order of first appearance, with the removal count) and the number of blank
        // fields
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
                parts.push(trf!("duplicate {}", names.join(", ")));
            }
            if blanks > 0 {
                parts.push(if blanks == 1 {
                    trf!("{blanks} blank field", blanks = blanks)
                } else {
                    trf!("{blanks} blank fields", blanks = blanks)
                });
            }
            self.diags.push(LintDiagnostic::new(
                "WARN",
                trf!("Removed blank/duplicate output schema fields: {}", parts.join(", ")),
            ));
            out.raw_schema = OutputSchemaAst::render_schema(out.format, &out.required_fields);
        }
        if out.required_fields.is_empty() {
            self.diags.push(LintDiagnostic::new(
                "ERROR",
                tr!("Output schema has no required fields"),
            ));
        } else {
            self.diags
                .push(LintDiagnostic::new("INFO", tr!("Schema integrity check passed")));
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

    /// Constraint sentences left after lint
    fn kept(rules: &[&str]) -> Vec<String> {
        let mut a = ast(rules, &["a"]);
        lint(&mut a);
        a.constraints.into_iter().map(|c| c.expression).collect()
    }

    /// WARNs other than the filler compression WARN (the role sentence of the test AST always produces one)
    fn warns(a: &RootPromptAst) -> Vec<&LintDiagnostic> {
        a.lint_warnings
            .iter()
            .filter(|d| d.severity == "WARN" && !d.message.contains("filler word"))
            .collect()
    }

    /// Consecutive items of a fixture list as (first, second) pairs.
    fn pairs(list: &'static [&'static str]) -> impl Iterator<Item = (&'static str, &'static str)> {
        let (pairs, rest) = list.as_chunks::<2>();
        assert!(rest.is_empty(), "odd pair list: {list:?}");
        pairs.iter().map(|&[a, b]| (a, b))
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
        assert!(a.lint_warnings.iter().any(|d| d.message.contains("filler word")));
    }

    #[test]
    fn drops_earlier_contradiction() {
        let (first, dont) = (
            ko_fixture!("core.detect_outliers_first"),
            ko_fixture!("core.dont_detect_outliers"),
        );
        let mut a = ast(&[first, dont], &["a"]);
        lint(&mut a);
        assert_eq!(a.constraints.len(), 1);
        assert_eq!(a.constraints[0].expression, dont);
        let w = warns(&a);
        assert_eq!(w.len(), 1);
        assert!(w[0].message.starts_with("Removed contradicting instruction") && w[0].message.contains(first));
        // Input positions of the removed (0) and the kept (1) constraint
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
        let english = [
            ("Detect outliers", "You must not detect outliers"),
            ("exclude missing rows", "include missing rows"),
            ("Never detect outliers", "Detect outliers"),
            ("Detect outliers in the amount column", "Never detect outliers"),
            ("Report totals including tax", "Report totals excluding tax"),
        ];
        for (a, b) in english.into_iter().chain(pairs(ko_fixture!("core.linter.clash_pairs"))) {
            assert_eq!(kept(&[a, b]), [b], "{a} / {b}");
        }
    }

    #[test]
    fn keeps_unrelated_rules() {
        let mut t = ast(
            &[
                ko_fixture!("core.exclude_missing_02pct"),
                ko_fixture!("core.detect_outliers_first"),
                "Do not use the id column",
            ],
            &["a"],
        );
        lint(&mut t);
        assert_eq!(t.constraints.len(), 3);
    }

    /// False positives reproduced in review: partial matches of negation markers, Korean words cut to 2 syllables, a
    /// one-word difference, a narrower negation.
    #[test]
    fn keeps_both_rules_that_do_not_contradict() {
        let english = [
            ("Use the amount column", "Never use the id column"),
            ("Do not sort rows by date", "Sort rows by amount"),
            (
                "Treat empty cells as zero values in totals",
                "You can treat empty cells as blanks",
            ),
            ("Detect outliers", "Never detect outliers in the id column"),
            ("Never detect outliers in the id column", "Detect outliers"),
            ("Nevertheless, report totals", "Report totals by region"),
            ("Do note the currency", "Note the currency per row"),
            ("Flag amount > 1000", "Do not flag amount < 1000"),
            ("Count 'very satisfied' answers", "Do not count 'satisfied' answers"),
            ("Do not answer \"do not know\"", "Answer \"yes\" or \"no\""),
        ];
        for (a, b) in english
            .into_iter()
            .chain(pairs(ko_fixture!("core.linter.unrelated_pairs")))
        {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            let got: Vec<&str> = t.constraints.iter().map(|c| c.expression.as_str()).collect();
            assert_eq!(got, [a, b], "{a} / {b}");
            assert!(
                !t.lint_warnings.iter().any(|d| d.message.contains("Removed")),
                "{a} / {b}: {:?}",
                t.lint_warnings
            );
        }
    }

    #[test]
    fn uncertain_pairs_warn_but_keep_both() {
        for (a, b) in [
            ("Detect outliers", "Never detect outlier"),
            (
                ko_fixture!("core.compute_value"),
                ko_fixture!("core.dont_compute_value"),
            ),
            (
                ko_fixture!("core.include_missing"),
                ko_fixture!("core.exclude_missing_02pct"),
            ),
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            let w = warns(&t);
            assert_eq!(w.len(), 1, "{a} / {b}: {w:?}");
            assert!(
                w[0].message.starts_with("Possible contradiction")
                    && w[0].message.contains(a)
                    && w[0].message.contains(b)
            );
            // Both stay, so the diagnostic carries the input positions of both constraints (earlier, later): the GUI
            // badge goes on both.
            assert_eq!(w[0].indices, [0, 1]);
        }
    }

    #[test]
    fn negation_markers_match_whole_words() {
        let english = [
            "You can treat empty cells as blanks",
            "Nevertheless, report totals",
            "Do note the units",
            "Avoidance rate per region",
            "Count rows labeled \"do not know\"",
        ];
        for s in english
            .into_iter()
            .chain(ko_fixture!("core.linter.not_negative").iter().copied())
        {
            assert!(!is_negative(s), "{s}");
        }
        let english = [
            "Don't guess",
            "don’t guess",
            "You can't skip rows",
            "Never round",
            "Avoid jargon",
        ];
        for s in english
            .into_iter()
            .chain(ko_fixture!("core.linter.negative").iter().copied())
        {
            assert!(is_negative(s), "{s}");
        }
    }

    #[test]
    fn korean_stems_drop_particles_and_endings_only() {
        for (w, want) in pairs(ko_fixture!("core.linter.stem_pairs")) {
            assert_eq!(stem(w, false), want, "{w}");
        }
        // Longer suffixes must come first so that "haneun" matches before "neun"
        let lens: Vec<usize> = KO_SUFFIXES.iter().map(|s| s.chars().count()).collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "{lens:?}");
    }

    #[test]
    fn dedup_keeps_numbers_operators_and_quoted_values() {
        for (a, b) in [
            ("Flag amount > 1000", "Flag amount < 1000"),
            ("Treat missing as -1", "Treat missing as 1"),
            (
                ko_fixture!("core.exclude_missing_02pct"),
                ko_fixture!("core.linter.exclude_missing_2pct"),
            ),
            ("Flag amount >= 5", "Flag amount > 5"),
            ("Round to 1.5", "Round to 15"),
            ("Label 'Yes' rows", "Label 'yes' rows"),
            ("Label 'a b' rows", "Label 'a' 'b' rows"),
            ("|", "&"),
        ] {
            assert_ne!(dedup_key(a), dedup_key(b), "{a} / {b}");
            assert_eq!(kept(&[a, b]), [a, b], "{a} / {b}");
        }
        let z = ko_fixture!("core.linter.zscore_basis_pair");
        for (a, b) in [
            ("Detect outliers first", "detect outliers first."),
            (z[0], z[1]),
            ("Flag amount>1000", "flag amount > 1000"),
        ] {
            assert_eq!(dedup_key(a), dedup_key(b), "{a} / {b}");
        }
    }

    #[test]
    fn removal_warnings_name_the_removed_text_and_index() {
        let (exclude, include) = (ko_fixture!("core.exclude_missing"), ko_fixture!("core.include_missing"));
        let mut a = ast(
            &["Detect outliers first", exclude, "detect outliers first.", include],
            &["a"],
        );
        lint(&mut a);
        // The later duplicate stays (the later instruction wins)
        let got: Vec<&str> = a.constraints.iter().map(|c| c.expression.as_str()).collect();
        assert_eq!(got, ["detect outliers first.", include]);
        let removals: Vec<(&str, &[usize])> = warns(&a)
            .iter()
            .filter(|d| d.message.contains("Removed"))
            .map(|d| (d.message.as_str(), d.indices.as_slice()))
            .collect();
        let clash = format!("Removed contradicting instruction: \"{exclude}\" ↔ \"{include}\" (kept the later one)");
        assert_eq!(
            removals,
            [
                (
                    "Removed duplicate constraint: \"Detect outliers first\" (kept the later copy: \"detect outliers first.\")",
                    [0, 2].as_slice()
                ),
                (clash.as_str(), [1, 3].as_slice()),
            ]
        );
        // When the same sentence repeats exactly, the text is written only once
        let mut b = ast(&["x", "x"], &["a"]);
        lint(&mut b);
        assert_eq!(
            warns(&b)[0].message,
            "Removed duplicate constraint: \"x\" (kept the later copy)"
        );
        assert_eq!(warns(&b)[0].indices, [0, 1]);
    }

    /// Review repro: with "detect", "do not detect", "detect" the last instruction wins. Before, the duplicate pass
    /// kept the first "detect", the contradiction pass removed it, and only "do not detect" was left.
    #[test]
    fn duplicates_keep_the_last_copy_so_the_later_instruction_wins() {
        let (detect, dont) = (
            ko_fixture!("core.detect_outliers"),
            ko_fixture!("core.dont_detect_outliers"),
        );
        let mut a = ast(&[detect, dont, detect], &["a"]);
        lint(&mut a);
        let got: Vec<&str> = a.constraints.iter().map(|c| c.expression.as_str()).collect();
        assert_eq!(got, [detect]);
        assert_eq!(
            a.constraints[0].input_index(),
            Some(2),
            "the survivor is the last input"
        );
        let w: Vec<(&str, &[usize])> = warns(&a)
            .iter()
            .map(|d| (d.message.as_str(), d.indices.as_slice()))
            .collect();
        let duplicate = format!("Removed duplicate constraint: \"{detect}\" (kept the later copy)");
        let clash = format!("Removed contradicting instruction: \"{dont}\" ↔ \"{detect}\" (kept the later one)");
        assert_eq!(
            w,
            [
                (duplicate.as_str(), [0, 2].as_slice()),
                (clash.as_str(), [1, 2].as_slice()),
            ]
        );
        // In the opposite order the last instruction stays too
        assert_eq!(kept(&[dont, detect, dont]), [dont]);
        // Three copies: the first two are removed, and every warning carries the positions of the removed and the
        // kept one
        let mut c = ast(&["a b", "A b.", "a  B"], &["a"]);
        lint(&mut c);
        assert_eq!(c.constraints.len(), 1);
        assert_eq!(c.constraints[0].expression, "a B");
        let idx: Vec<&[usize]> = warns(&c).iter().map(|d| d.indices.as_slice()).collect();
        assert_eq!(idx, [[0, 2].as_slice(), [1, 2].as_slice()]);
    }

    #[test]
    fn fillers_inside_quotes_are_kept() {
        let english = [
            ("Count \"very satisfied\" answers", "Count \"very satisfied\" answers"),
            (
                "Use `very` and “very  good” and ‘really’ labels",
                "Use `very` and “very  good” and ‘really’ labels",
            ),
            ("Don't be very vague, it's really simple", "Don't be vague, it's simple"),
            ("(very important) list causes", "(important) list causes"),
            ("Be careful, very.", "Be careful,"),
            ("Analyze it very.", "Analyze it."),
            ("Please, analyze", "analyze"),
            ("very  very", "very very"),
        ];
        for (input, want) in english
            .into_iter()
            .chain(pairs(ko_fixture!("core.linter.filler_quote_pairs")))
        {
            assert_eq!(kept(&[input]), [want], "{input:?}");
        }
    }

    #[test]
    fn role_keeps_line_breaks() {
        let mut a = ast(&[], &["a"]);
        a.role.instruction = ko_fixture!("core.linter.role_input").into();
        lint(&mut a);
        assert_eq!(a.role.instruction, ko_fixture!("core.linter.role_expected"));
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
        assert_eq!(
            spans(ko_fixture!("core.linter.quote_then_particle")),
            [ko_fixture!("core.linter.quoted_very_satisfied"), "‘don’t’", "“a”"]
        );
        assert_eq!(spans("``a ` b`` and `c` and ```"), ["``a ` b``", "`c`"]);
        assert_eq!(spans("5' and \"open"), Vec::<String>::new());
    }

    #[test]
    fn diagnostic_indices_default_when_missing() {
        // Diagnostics without positions (or old JSON with only an `index` field) still load
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

    /// False positives reproduced in the second review: exception/contrast clauses, stems that end in a noun's last
    /// syllable, descriptive increase/decrease and negations, word order, "geumji" (ban) inside a word.
    #[test]
    fn exception_clauses_and_descriptions_are_not_removed() {
        // Both kept, with only a possible-contradiction WARN
        let english = [
            ("Use the median instead of the mean", "Do not use the mean"),
            ("Round to 2 decimals except percentages", "Do not round percentages"),
            ("Treat empty cells as zero", "Do not treat zero as empty"),
        ];
        for (a, b) in english
            .into_iter()
            .chain(pairs(ko_fixture!("core.linter.exception_suspect_pairs")))
        {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            let w = warns(&t);
            assert!(
                w.len() == 1 && w[0].message.starts_with("Possible contradiction"),
                "{a} / {b}: {w:?}"
            );
        }
        // Both kept without any warning
        let english = [("Flag customers who never purchased", "Flag customers")];
        for (a, b) in english
            .into_iter()
            .chain(pairs(ko_fixture!("core.linter.description_pairs")))
        {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            assert!(warns(&t).is_empty(), "{a} / {b}: {:?}", warns(&t));
        }
        let english = ["Flag customers who never purchased"];
        for s in english
            .into_iter()
            .chain(ko_fixture!("core.linter.description_not_negative").iter().copied())
        {
            assert!(!is_negative(s), "{s}");
        }
        for s in ko_fixture!("core.linter.ban_negative") {
            assert!(is_negative(s), "{s}");
        }
        // Increase/decrease is uncertain even in predicate position (first or last): both stay with only a
        // possible-contradiction warning.
        // (The old expectation "only the later one stays" was exactly the false-positive rule the review found: it
        // also removed "Explain revenue increase/decrease".)
        let english = [("Increase the sample size", "Decrease the sample size")];
        for (a, b) in english
            .into_iter()
            .chain(pairs(ko_fixture!("core.linter.increase_decrease_pair")))
        {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 2, "{a} / {b}");
            let w = warns(&t);
            assert!(
                w.len() == 1 && w[0].message.starts_with("Possible contradiction"),
                "{a} / {b}: {w:?}"
            );
        }
    }

    /// Review repro: non-directive antonyms (increase/decrease), avoid/never in infinitives and hyphenated compounds,
    /// and the Korean purpose clause "-ji anhdorok" were judged certain contradictions and the earlier constraint was
    /// removed. Both must stay (a possible-contradiction warning is fine).
    #[test]
    fn descriptive_antonyms_and_negations_keep_both_rules() {
        let english = [
            ("Explain revenue increase", "Explain revenue decrease"),
            ("List products that increased", "List products that decreased"),
            ("Suggest ways to avoid churn", "Suggest ways to reduce churn"),
            ("List risks to avoid", "List risks by region"),
            ("Flag never-paid invoices", "Flag paid invoices"),
        ];
        for (a, b) in english
            .into_iter()
            .chain(pairs(ko_fixture!("core.linter.descriptive_pairs")))
        {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            let got: Vec<&str> = t.constraints.iter().map(|c| c.expression.as_str()).collect();
            assert_eq!(got, [a, b], "{a} / {b}");
            let w = warns(&t);
            assert!(
                w.iter().all(|d| d.message.starts_with("Possible contradiction")),
                "{a} / {b}: {w:?}"
            );
        }
        // Descriptive negations are not prohibitions
        let english = [
            "Suggest ways to avoid churn",
            "List risks to avoid",
            "Explain how to never miss a deadline",
            "To avoid churn, suggest discounts",
            "Flag never-paid invoices",
            "Call the do-not-call list owners",
        ];
        for s in english
            .into_iter()
            .chain(ko_fixture!("core.linter.purpose_not_negative").iter().copied())
        {
            assert!(!is_negative(s), "{s}");
        }
        // Prohibitive instructions stay negative: imperative avoid/never, an infinitive after a prohibition frame,
        // "-ji anhdorok" + a directive
        let english = [
            "Avoid jargon",
            "Never round",
            "Try to avoid jargon",
            "Make sure to never round values",
            "You must remember to avoid guessing",
            "It is important to avoid jargon",
        ];
        for s in english
            .into_iter()
            .chain(ko_fixture!("core.linter.purpose_negative").iter().copied())
        {
            assert!(is_negative(s), "{s}");
        }
        // An infinitive with a prohibition frame is still a factuality rule ("Try to avoid guessing")
        assert_eq!(
            ConstraintNode::classify("C1".into(), "Try to avoid guessing").rule_type,
            ConstraintType::FactualityRule
        );
        // Certain contradictions are still removed
        let purpose = ko_fixture!("core.linter.include_vs_purpose_ban_pair");
        for (a, b) in [
            (
                ko_fixture!("core.detect_outliers_first"),
                ko_fixture!("core.dont_detect_outliers"),
            ),
            (ko_fixture!("core.exclude_missing"), ko_fixture!("core.include_missing")),
            ("Detect outliers", "You must not detect outliers"),
            ("Use jargon", "Avoid jargon"),
            ("Round values", "Never round values"),
            (purpose[0], purpose[1]),
        ] {
            assert_eq!(kept(&[a, b]), [b], "{a} / {b}");
        }
    }

    /// Review repro: the warning for `--fields "a,b,a,b"` did not name the removed fields (requirement 4).
    #[test]
    fn schema_warning_names_the_removed_fields() {
        let schema_warn = |fields: &[&str]| {
            let mut a = ast(&[], fields);
            lint(&mut a);
            let w: Vec<String> = a
                .lint_warnings
                .iter()
                .filter(|d| d.message.starts_with("Removed blank/duplicate output schema fields"))
                .map(|d| d.message.clone())
                .collect();
            (w, a.output_schema.required_fields)
        };
        let (w, kept) = schema_warn(&["a", "b", "a", "b"]);
        assert_eq!(
            w,
            ["Removed blank/duplicate output schema fields: duplicate \"a\", \"b\""]
        );
        assert_eq!(kept, ["a", "b"]);
        let (w, kept) = schema_warn(&["x", " ", "x", "x", "", "y"]);
        assert_eq!(
            w,
            ["Removed blank/duplicate output schema fields: duplicate \"x\" ×2, 2 blank fields"]
        );
        assert_eq!(kept, ["x", "y"]);
        let (w, _) = schema_warn(&["a", "b"]);
        assert!(w.is_empty());
    }

    #[test]
    fn more_quote_styles_protect_fillers() {
        let english = ["《very good》 share", "«very good» share"];
        for s in english
            .into_iter()
            .chain(ko_fixture!("core.linter.bracket_quoted_fillers").iter().copied())
        {
            assert_eq!(kept(&[s]), [s], "{s}");
        }
    }

    /// Whether there is Hangul outside the user sentences quoted in double quotes
    fn hangul_outside_quotes(s: &str) -> bool {
        s.split('"').step_by(2).any(|part| part.chars().any(is_hangul))
    }

    /// English mode: only the diagnostic wording becomes English; quoted user sentences, numbers and field names go in
    /// as they are. The kept constraints, the schema, and the count, severity and positions of the diagnostics are the
    /// same as in Korean mode.
    #[test]
    fn english_messages_keep_quotes_numbers_and_indices() {
        use crate::i18n::{Lang, with_lang};
        let (exclude, include) = (ko_fixture!("core.exclude_missing"), ko_fixture!("core.include_missing"));
        let (compute, dont) = (
            ko_fixture!("core.compute_value"),
            ko_fixture!("core.dont_compute_value"),
        );
        let rules = [
            "Detect outliers first",
            exclude,
            "detect outliers first.",
            include,
            "x",
            "x",
            compute,
            dont,
        ];
        let fields = ["a", "a", " ", "b"];
        let run = |lang| {
            with_lang(lang, || {
                let mut a = ast(&rules, &fields);
                lint(&mut a);
                a
            })
        };
        let (ko, en) = (run(Lang::Ko), run(Lang::En));
        let messages = |a: &RootPromptAst| a.lint_warnings.iter().map(|d| d.message.clone()).collect::<Vec<_>>();
        let schema_parts = [
            format!(ko_text!("duplicate {}"), "\"a\""),
            format!(ko_text!("{blanks} blank field"), blanks = 1),
        ]
        .join(", ");
        assert_eq!(
            messages(&ko),
            [
                format!(ko_text!("Compressed {n} filler words"), n = 2),
                format!(
                    ko_text!("Removed duplicate constraint: \"{}\" (kept the later copy: \"{}\")"),
                    "Detect outliers first", "detect outliers first."
                ),
                format!(
                    ko_text!("Removed duplicate constraint: \"{}\" (kept the later copy)"),
                    "x"
                ),
                format!(
                    ko_text!("Removed contradicting instruction: \"{}\" ↔ \"{}\" (kept the later one)"),
                    exclude, include
                ),
                format!(
                    ko_text!(
                        "Possible contradiction (kept both): \"{}\" ↔ \"{}\". Check whether they give opposite instructions for the same thing"
                    ),
                    compute, dont
                ),
                format!(
                    ko_text!("Removed blank/duplicate output schema fields: {}"),
                    schema_parts
                ),
                ko_text!("Schema integrity check passed").to_string(),
            ]
        );
        assert_eq!(
            messages(&en),
            [
                "Compressed 2 filler words".to_string(),
                "Removed duplicate constraint: \"Detect outliers first\" (kept the later copy: \"detect outliers first.\")"
                    .to_string(),
                "Removed duplicate constraint: \"x\" (kept the later copy)".to_string(),
                format!("Removed contradicting instruction: \"{exclude}\" ↔ \"{include}\" (kept the later one)"),
                format!(
                    "Possible contradiction (kept both): \"{compute}\" ↔ \"{dont}\". Check whether they give opposite instructions for the same thing"
                ),
                "Removed blank/duplicate output schema fields: duplicate \"a\", 1 blank field".to_string(),
                "Schema integrity check passed".to_string(),
            ]
        );
        assert!(messages(&en).iter().all(|m| !hangul_outside_quotes(m)));
        let shape = |a: &RootPromptAst| {
            a.lint_warnings
                .iter()
                .map(|d| (d.severity.clone(), d.indices.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&ko), shape(&en));
        assert_eq!(shape(&en)[3], ("WARN".to_string(), vec![1, 3]));
        let exprs = |a: &RootPromptAst| a.constraints.iter().map(|c| c.expression.clone()).collect::<Vec<_>>();
        assert_eq!(exprs(&ko), exprs(&en));
        assert_eq!(ko.role.instruction, en.role.instruction);
        assert_eq!(ko.output_schema.raw_schema, en.output_schema.raw_schema);
    }

    #[test]
    fn english_counts_and_errors() {
        use crate::i18n::{Lang, with_lang};
        with_lang(Lang::En, || {
            // Singular and plural
            let mut a = ast(&["very short"], &["x", " ", "x", "x", "", "y"]);
            a.role.instruction = "analyze".into();
            lint(&mut a);
            let w: Vec<&str> = a.lint_warnings.iter().map(|d| d.message.as_str()).collect();
            assert_eq!(
                w,
                [
                    "Compressed 1 filler word",
                    "Removed blank/duplicate output schema fields: duplicate \"x\" ×2, 2 blank fields",
                    "Schema integrity check passed",
                ]
            );
            // No required fields: ERROR
            let mut e = ast(&[], &[]);
            lint(&mut e);
            let errors: Vec<&str> = e
                .lint_warnings
                .iter()
                .filter(|d| d.severity == "ERROR")
                .map(|d| d.message.as_str())
                .collect();
            assert_eq!(errors, ["Output schema has no required fields"]);
        });
    }

    #[test]
    fn a_negation_of_one_list_item_does_not_remove_the_whole_rule() {
        for (positive, negative) in [
            ("Detect outliers and duplicates", "Do not detect outliers"),
            ("Report revenue, cost and margin per region", "Do not report margin"),
            ("Flag refunds or chargebacks", "Do not flag refunds"),
        ] {
            let k = kept(&[positive, negative]);
            assert_eq!(k.len(), 2, "{positive} / {negative}: {k:?}");
        }
        // Without a list the same wording is still a certain contradiction, and a comma inside a number is no list
        assert_eq!(
            kept(&["Detect outliers", "Do not detect outliers"]),
            ["Do not detect outliers"]
        );
        assert_eq!(
            kept(&["Flag amounts above 1,000", "Do not flag amounts above 1,000"]),
            ["Do not flag amounts above 1,000"]
        );
    }

    #[test]
    fn a_filler_after_a_negation_is_not_dropped() {
        // "Do not simply remove rows" must not turn into "Do not remove rows" (and then contradict the first rule)
        let k = kept(&["Remove duplicate rows", "Do not simply remove duplicate rows"]);
        assert_eq!(k.len(), 2, "{k:?}");
        assert!(k[1].contains("simply"), "{k:?}");
        for (text, want) in [
            ("don't really change it", "don't really change it"),
            ("never very large", "never very large"),
            ("Avoid highly noisy columns", "Avoid highly noisy columns"),
            // Away from a negation the filler is still compressed
            ("it is very important", "it is important"),
        ] {
            assert_eq!(compress(text, false, true).0, want, "{text}");
        }
    }
}
