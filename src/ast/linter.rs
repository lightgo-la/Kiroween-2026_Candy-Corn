//! Visitor 패턴 기반 린트 패스: 수식어 압축, 중복 제거, 모순 제약 탐지, 스키마 무결성 검증.

use super::*;
use std::collections::HashSet;

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

// ---------- 공통 헬퍼 ----------

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

const NEG_MARKERS: [&str; 17] = [
    "do not",
    "don t",
    "never",
    "avoid",
    "must not",
    "mustn t",
    "should not",
    "shouldn t",
    "cannot",
    "can t",
    "won t",
    "하지 마",
    "하지마",
    "하지 말",
    "하지말",
    "금지",
    "않",
];

/// 서로 반대 의미로 쓰이는 단어쌍. 나머지 단어가 같으면 모순으로 본다.
const ANTONYMS: [(&str, &str); 8] = [
    ("제외", "포함"),
    ("exclude", "include"),
    ("허용", "불허"),
    ("allow", "forbid"),
    ("증가", "감소"),
    ("increase", "decrease"),
    ("오름차순", "내림차순"),
    ("ascending", "descending"),
];

/// 정규화된 문자열에서 마지막 "말 것" 계열 부정 표현도 인식한다.
const NEG_SUFFIXES: [&str; 2] = ["말 것", "말것"];
fn normalize(s: &str) -> String {
    let mapped: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn is_negative(s: &str) -> bool {
    let n = normalize(s);
    NEG_MARKERS.iter().chain(NEG_SUFFIXES.iter()).any(|m| n.contains(m))
}

/// 한글 토큰은 앞 2글자로 줄여 어미 변화("탐지하지"/"탐지해")를 흡수한다(휴리스틱).
fn stem_set(s: &str) -> HashSet<String> {
    let mut n = normalize(s);
    for m in NEG_MARKERS.iter().chain(NEG_SUFFIXES.iter()) {
        n = n.replace(m, " ");
    }
    n.split_whitespace()
        .map(|t| {
            if t.is_ascii() {
                t.to_string()
            } else {
                t.chars().take(2).collect()
            }
        })
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

/// 반의어 단어를 포함하면 (쌍 번호, 방향)을 돌려준다. 한글 어미 변화를 고려해 접두 일치로 본다.
fn antonym_side(s: &str) -> Option<(usize, bool)> {
    let n = normalize(s);
    n.split_whitespace().find_map(|t| {
        ANTONYMS.iter().enumerate().find_map(|(i, (a, b))| {
            if t.starts_with(a) {
                Some((i, false))
            } else if t.starts_with(b) {
                Some((i, true))
            } else {
                None
            }
        })
    })
}

/// 반의어 단어를 뺀 나머지 단어 집합.
fn stems_without_antonyms(s: &str) -> HashSet<String> {
    let mut set = stem_set(s);
    for (a, b) in ANTONYMS {
        for w in [a, b] {
            let stem: String = if w.is_ascii() {
                w.to_string()
            } else {
                w.chars().take(2).collect()
            };
            set.remove(&stem);
        }
    }
    set
}

/// 수식어 토큰을 제거하고 제거 개수를 돌려준다.
fn strip_fillers(s: &str) -> (String, usize) {
    let mut removed = 0;
    let kept: Vec<&str> = s
        .split_whitespace()
        .filter(|t| {
            let is_filler = FILLERS.contains(&t.to_lowercase().trim_matches(|c: char| !c.is_alphanumeric()));
            if is_filler {
                removed += 1;
            }
            !is_filler
        })
        .collect();
    (kept.join(" "), removed)
}

// ---------- 패스 ----------

#[derive(Default)]
pub struct FillerPass {
    removed: usize,
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for FillerPass {
    fn visit_role(&mut self, role: &mut RoleNode) {
        let (s, n) = strip_fillers(&role.instruction);
        role.instruction = s;
        self.removed += n;
    }
    fn visit_constraints(&mut self, constraints: &mut Vec<ConstraintNode>) {
        for c in constraints.iter_mut() {
            let (s, n) = strip_fillers(&c.expression);
            if !s.is_empty() {
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

#[derive(Default)]
pub struct DedupPass {
    removed: usize,
}

impl AstVisitor for DedupPass {
    fn visit_constraints(&mut self, constraints: &mut Vec<ConstraintNode>) {
        let mut seen = HashSet::new();
        let before = constraints.len();
        constraints.retain(|c| seen.insert(normalize(&c.expression)));
        self.removed = before - constraints.len();
    }
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
        if self.removed > 0 {
            vec![LintDiagnostic::new(
                "WARN",
                format!("중복 제약 {}건 제거됨", self.removed),
            )]
        } else {
            vec![]
        }
    }
}

#[derive(Default)]
pub struct ContradictionPass {
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for ContradictionPass {
    fn visit_constraints(&mut self, constraints: &mut Vec<ConstraintNode>) {
        let infos: Vec<(bool, HashSet<String>)> = constraints
            .iter()
            .map(|c| (is_negative(&c.expression), stem_set(&c.expression)))
            .collect();
        let anto: Vec<Option<(usize, bool)>> = constraints.iter().map(|c| antonym_side(&c.expression)).collect();
        let mut drop = vec![false; constraints.len()];
        for j in 0..constraints.len() {
            for i in 0..j {
                if drop[i] || drop[j] {
                    continue;
                }
                let negation_clash = infos[i].0 != infos[j].0 && jaccard(&infos[i].1, &infos[j].1) >= 0.6;
                let antonym_clash = match (anto[i], anto[j]) {
                    (Some((pi, si)), Some((pj, sj))) => {
                        pi == pj
                            && si != sj
                            && infos[i].0 == infos[j].0
                            && jaccard(
                                &stems_without_antonyms(&constraints[i].expression),
                                &stems_without_antonyms(&constraints[j].expression),
                            ) >= 0.6
                    }
                    _ => false,
                };
                if negation_clash || antonym_clash {
                    // 나중에 쓴 지시가 우선한다.
                    drop[i] = true;
                    self.diags.push(LintDiagnostic::new(
                        "WARN",
                        format!(
                            "모순 지시 자동제거: \"{}\" ↔ \"{}\" (뒤의 지시 유지)",
                            constraints[i].expression, constraints[j].expression
                        ),
                    ));
                }
            }
        }
        let mut idx = 0;
        constraints.retain(|_| {
            let keep = !drop[idx];
            idx += 1;
            keep
        });
    }
    fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
        std::mem::take(&mut self.diags)
    }
}

#[derive(Default)]
pub struct SchemaPass {
    diags: Vec<LintDiagnostic>,
}

impl AstVisitor for SchemaPass {
    fn visit_output(&mut self, out: &mut OutputSchemaAst) {
        let before = out.required_fields.len();
        let mut seen = HashSet::new();
        out.required_fields
            .retain(|f| !f.trim().is_empty() && seen.insert(f.clone()));
        if out.required_fields.len() != before {
            self.diags
                .push(LintDiagnostic::new("WARN", "출력 스키마의 빈/중복 필드 제거됨"));
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
        ] {
            let mut t = ast(&[a, b], &["a"]);
            lint(&mut t);
            assert_eq!(t.constraints.len(), 1, "{a} / {b}");
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
}
