//! 타겟 모델별 Sugar-Coated 직렬화 (Claude=XML, GPT=Markdown, Gemini=구분자 섹션).

use super::*;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// 속성 값용: 따옴표까지 이스케이프한다.
fn xml_attr(s: &str) -> String {
    xml_escape(s).replace('"', "&quot;")
}

/// Markdown 표 셀용: 파이프와 개행이 표를 깨지 않게 한다.
pub fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn pct(r: f64) -> String {
    format!("{:.1}%", r * 100.0)
}

fn output_instruction(o: &OutputSchemaAst) -> String {
    match o.format {
        OutputFormat::StrictJson => format!("Respond with ONLY valid JSON matching this schema:\n{}", o.raw_schema),
        OutputFormat::MarkdownTable => {
            format!("Respond with ONLY a Markdown table with header:\n{}", o.raw_schema)
        }
    }
}

fn sample_block(d: &DataSubAst) -> String {
    let header = d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(",");
    let mut s = header;
    for r in &d.sample_rows {
        s.push('\n');
        s.push_str(r);
    }
    s
}

pub fn generate(ast: &RootPromptAst) -> String {
    match ast.target_model {
        TargetModel::Claude => xml(ast),
        TargetModel::Gpt => markdown(ast),
        TargetModel::Gemini => sections(ast),
    }
}

fn xml(a: &RootPromptAst) -> String {
    let mut s = String::new();
    s += &format!(
        "<role name=\"{}\">{}</role>\n",
        xml_attr(&a.role.name),
        xml_escape(&a.role.instruction)
    );
    if let Some(d) = &a.data_sub_ast {
        s += &format!(
            "<data_profile source=\"{}\" rows=\"{}\">\n",
            xml_attr(&d.source_name),
            d.total_rows
        );
        s += "  <schema>\n";
        for c in &d.columns {
            s += &format!(
                "    {}|{}|null:{}|{}\n",
                xml_escape(&c.name),
                c.dtype,
                pct(c.null_ratio),
                xml_escape(&c.summary_stats)
            );
        }
        s += "  </schema>\n";
        if !d.anomalies.is_empty() {
            s += "  <anomalies>\n";
            for x in &d.anomalies {
                s += &format!("    - {}\n", xml_escape(x));
            }
            s += "  </anomalies>\n";
        }
        if !d.sample_rows.is_empty() {
            s += &format!("  <sample>\n{}\n  </sample>\n", xml_escape(&sample_block(d)));
        }
        s += "</data_profile>\n";
    }
    if !a.constraints.is_empty() {
        s += "<constraints>\n";
        for c in &a.constraints {
            s += &format!(
                "  <rule id=\"{}\" type=\"{}\">{}</rule>\n",
                c.id,
                c.rule_type.tag(),
                xml_escape(&c.expression)
            );
        }
        s += "</constraints>\n";
    }
    s += &format!(
        "<output_format>\n{}\n</output_format>",
        xml_escape(&output_instruction(&a.output_schema))
    );
    s
}

fn markdown(a: &RootPromptAst) -> String {
    let mut s = format!("# Role\n{}: {}\n", a.role.name, a.role.instruction);
    if let Some(d) = &a.data_sub_ast {
        s += &format!(
            "\n## Data Profile\nsource: {} | rows: {}\n\n| column | type | null | stats |\n|---|---|---|---|\n",
            d.source_name, d.total_rows
        );
        for c in &d.columns {
            s += &format!(
                "| {} | {} | {} | {} |\n",
                md_cell(&c.name),
                c.dtype,
                pct(c.null_ratio),
                md_cell(&c.summary_stats)
            );
        }
        if !d.anomalies.is_empty() {
            s += "\n### Anomalies\n";
            for x in &d.anomalies {
                s += &format!("- {}\n", x);
            }
        }
        if !d.sample_rows.is_empty() {
            s += &format!("\n### Sample\n```csv\n{}\n```\n", sample_block(d));
        }
    }
    if !a.constraints.is_empty() {
        s += "\n## Constraints\n";
        for (i, c) in a.constraints.iter().enumerate() {
            s += &format!("{}. {}\n", i + 1, c.expression);
        }
    }
    s += &format!("\n## Output\n{}", output_instruction(&a.output_schema));
    s
}

fn sections(a: &RootPromptAst) -> String {
    let mut s = format!("=== ROLE ===\n{}: {}\n", a.role.name, a.role.instruction);
    if let Some(d) = &a.data_sub_ast {
        s += &format!("\n=== DATA ({}, {} rows) ===\n", d.source_name, d.total_rows);
        for c in &d.columns {
            s += &format!(
                "* {} [{}] null={} {}\n",
                c.name,
                c.dtype,
                pct(c.null_ratio),
                c.summary_stats
            );
        }
        for x in &d.anomalies {
            s += &format!("! {}\n", x);
        }
        if !d.sample_rows.is_empty() {
            s += &format!("--- sample ---\n{}\n", sample_block(d));
        }
    }
    if !a.constraints.is_empty() {
        s += "\n=== CONSTRAINTS ===\n";
        for c in &a.constraints {
            s += &format!("[{}] {}\n", c.id, c.expression);
        }
    }
    s += &format!("\n=== OUTPUT ===\n{}", output_instruction(&a.output_schema));
    s
}
