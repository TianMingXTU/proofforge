//! 报告渲染。
//!
//! 需求文档 FR-064 至 FR-068：证据报告、工程质量报告、质量门禁与导出格式
//! （JSON、HTML、SARIF）。
//!
//! 渲染器是纯函数：相同的 [`ReviewReport`] 必须产生完全相同的输出，这样
//! 报告才能被缓存、diff 与作为证据归档。

#![warn(missing_docs)]

use std::fmt::Write as _;

use pf_core::config::ReportFormat;
use pf_core::finding::{Finding, FindingCategory, Remediation};
use pf_core::review::{LayerSummary, QualityLayers, ReviewReport};
use pf_core::status::{EvidenceStatus, GateStatus, Severity};

/// 渲染错误。
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// 模板或输出格式不受支持。
    #[error("unsupported report format: {format:?}")]
    UnsupportedFormat {
        /// 格式标识。
        format: String,
    },

    /// 写入输出失败。
    #[error("failed to write report: {reason}")]
    WriteFailed {
        /// 底层原因。
        reason: String,
    },
}

/// 结果别名。
pub type Result<T> = std::result::Result<T, ReportError>;

/// 把报告渲染为字符串。
///
/// # Errors
/// 请求的格式不受支持或序列化失败时返回 [`ReportError`]。
pub fn render(report: &ReviewReport, format: ReportFormat) -> Result<String> {
    match format {
        ReportFormat::Terminal => Ok(render_terminal(report)),
        ReportFormat::Json => render_json(report),
        ReportFormat::Sarif => render_sarif(report),
        ReportFormat::Html => Ok(render_html(report)),
    }
}

/// 渲染为 JSON。
///
/// # Errors
/// 序列化失败时返回 [`ReportError::WriteFailed`]。
pub fn render_json(report: &ReviewReport) -> Result<String> {
    serde_json::to_string_pretty(&JsonReport::from(report)).map_err(|reason| {
        ReportError::WriteFailed {
            reason: reason.to_string(),
        }
    })
}

/// 渲染为 SARIF 2.1.0（GitHub 代码扫描可直接消费）。
///
/// # Errors
/// 序列化失败时返回 [`ReportError::WriteFailed`]。
pub fn render_sarif(report: &ReviewReport) -> Result<String> {
    let sarif = SarifReport::from(report);
    serde_json::to_string_pretty(&sarif).map_err(|reason| ReportError::WriteFailed {
        reason: reason.to_string(),
    })
}

/// 渲染为人类可读的终端报告。
#[must_use]
pub fn render_terminal(report: &ReviewReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "ProofForge 工程质量审查");
    let _ = writeln!(out, "────────────────────────");
    let _ = writeln!(out, "项目     : {}", report.project_root.display());
    let _ = writeln!(out, "版本     : {}", report.revision);
    let _ = writeln!(out, "运行     : {}", report.run_id);
    let _ = writeln!(
        out,
        "规模     : {} 个文件 / {} 个符号",
        report.file_count, report.symbol_count
    );
    let _ = writeln!(out, "规则集   : builtin@{}", report.ruleset_version);
    let _ = writeln!(out, "耗时     : {} ms", report.duration.as_millis());
    let _ = writeln!(out);

    let _ = writeln!(out, "质量分层");
    for (name, layer) in [
        ("正确性与安全", &report.layers.correctness),
        ("工程规范", &report.layers.engineering),
        ("可维护性与设计", &report.layers.maintainability),
        ("验证可信度", &report.layers.verification),
    ] {
        let _ = writeln!(
            out,
            "  {name:<14} 发现 {:<4} 已确认 {:<4} 已豁免 {:<4}",
            layer.count, layer.confirmed, layer.exempted
        );
    }
    let _ = writeln!(out);

    // 变更影响分析（FR-025）只在有基线时存在：没有基线就无所谓「相对谁变更」。
    // 被分析的文件只是「变了的那批」，依赖它们的文件全在别处——两者不同，
    // 因此分开呈现，否则使用者会以为影响范围已经被审查过了。
    if let Some(impact) = &report.impact {
        let _ = writeln!(
            out,
            "变更影响 : {} 个文件发生变化，可能波及 {} 个文件（未被本次分析覆盖）",
            impact.changed.len(),
            impact.impacted.len()
        );
        for path in impact.impacted.iter().take(20) {
            let _ = writeln!(out, "  · {path}");
        }
        if impact.impacted.len() > 20 {
            let _ = writeln!(out, "  …另有 {} 个", impact.impacted.len() - 20);
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "门禁结论 : {}", gate_line(report.gate));
    if !report.imported_standards.is_empty() {
        let _ = writeln!(out, "项目规范 ({}):", report.imported_standards.len());
        for standard in &report.imported_standards {
            let _ = writeln!(
                out,
                "  {}@{} — {} 条规则（{} 条可自动检查）",
                standard.name, standard.version, standard.rule_count, standard.automated_count
            );
            for note in &standard.notes {
                let _ = writeln!(out, "      note: {note}");
            }
        }
    }
    if report.findings.is_empty() {
        let _ = writeln!(out, "未发现工程质量问题。");
    } else {
        let _ = writeln!(out, "发现列表 ({}):", report.findings.len());
        for finding in &report.findings {
            let _ = writeln!(
                out,
                "  [{}] {} — {}{}",
                finding.severity,
                finding.status,
                finding.title,
                exemption_suffix(finding),
            );
            for location in &finding.locations {
                let _ = writeln!(out, "      at {}", location.display());
            }
            if !finding.facts.is_empty() {
                let facts: Vec<String> = finding
                    .facts
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect();
                let _ = writeln!(out, "      facts: {}", facts.join(", "));
            }
            if let Some(remediation) = &finding.remediation {
                let _ = writeln!(out, "      fix: {}", remediation.summary);
            }
        }
    }

    if !report.warnings.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "警告 ({}):", report.warnings.len());
        for warning in &report.warnings {
            let _ = writeln!(out, "  ! {warning}");
        }
    }

    out
}

/// 门禁结论的单行摘要（含退出码提示）。
fn gate_line(gate: GateStatus) -> String {
    match gate {
        GateStatus::Passed => "通过 (exit 0)".to_owned(),
        GateStatus::Failed => "未通过 (exit 1)".to_owned(),
        GateStatus::InsufficientEvidence => "证据不足 (exit 2)".to_owned(),
        GateStatus::Cancelled => "已取消 (exit 3)".to_owned(),
    }
}

/// 发现标题后面的豁免后缀。
///
/// 空串表示没有豁免。豁免必须连同理由与批准人一起显示：只说「已豁免」不说
/// 「谁为什么豁免」，等于把审计线索留在了代码里（FR-062 / FR-070）。
fn exemption_suffix(finding: &Finding) -> String {
    match &finding.exemption {
        None => String::new(),
        Some(exemption) => format!(
            "（已豁免：{}；批准人：{}；范围：{}）",
            exemption.reason,
            exemption.granted_by,
            exemption.scope.describe()
        ),
    }
}

/// 渲染为单文件 HTML（FR-068）。
///
/// # 为什么不用模板引擎
///
/// 报告渲染是纯函数且每轮只跑一次，引入模板 crate 只会多一层需要审计的依赖。
/// 样式内联在 `<style>` 里、不引用任何外部资源：HTML 报告要能随 CI 工件归档，
/// 离线打开，断网也能看。
///
/// 所有来自发现与路径的文本都经过 [`escape_html`]：仓库里的字符串可能含有
/// `<`、`&` 甚至 `</script>`，直接拼接会让报告变成注入点。
#[must_use]
pub fn render_html(report: &ReviewReport) -> String {
    let mut out = String::new();
    out.push_str(HTML_HEAD);
    out.push_str(HTML_STYLE);
    out.push_str("</head>\n<body>\n<main>\n");
    out.push_str("<h1>ProofForge 工程质量审查报告</h1>\n");
    push_meta(&mut out, report);
    push_gate(&mut out, report);
    push_imported_standards(&mut out, report);
    push_layers(&mut out, report);
    push_findings(&mut out, report);
    push_warnings(&mut out, report);
    let _ = writeln!(
        out,
        "<footer><p>run {} · schema proofforge/report/1 · 本报告由确定性静态分析生成，\
         不含模型猜测结论。</p></footer>",
        escape_html(&report.run_id.to_string())
    );
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

/// 文档头（不含样式，样式由 [`HTML_STYLE`] 提供）。
const HTML_HEAD: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ProofForge 工程质量审查报告</title>
"#;

/// 渲染报告的元信息段（项目、版本、规模、耗时）。
fn push_meta(out: &mut String, report: &ReviewReport) {
    out.push_str("<section class=\"meta\">\n<dl>\n");
    for (label, value) in [
        ("项目", report.project_root.display().to_string()),
        ("版本", report.revision.clone()),
        ("运行", report.run_id.to_string()),
        ("规则集", format!("builtin@{}", report.ruleset_version)),
        (
            "规模",
            format!(
                "{} 个文件 / {} 个符号",
                report.file_count, report.symbol_count
            ),
        ),
        ("耗时", format!("{} ms", report.duration.as_millis())),
    ] {
        let _ = writeln!(out, "<dt>{label}</dt><dd>{}</dd>", escape_html(&value));
    }
    out.push_str("</dl>\n</section>\n");
}

/// 渲染门禁结论段。
///
/// 放在最上面：读者第一眼要知道的就是这次审查算不算过。
fn push_gate(out: &mut String, report: &ReviewReport) {
    let _ = writeln!(
        out,
        "<section class=\"gate {}\"><h2>门禁结论：{}</h2><p>{}</p></section>",
        gate_class(report.gate),
        escape_html(&gate_line(report.gate)),
        escape_html(&gate_explanation(report))
    );
}

/// 渲染导入的项目规范段（FR-058）。
fn push_imported_standards(out: &mut String, report: &ReviewReport) {
    if report.imported_standards.is_empty() {
        return;
    }
    out.push_str(
        "<section><h2>项目规范</h2>\n<table>\n<thead><tr>\
         <th>来源</th><th>版本</th><th>规则</th><th>可自动检查</th>\
         </tr></thead>\n<tbody>\n",
    );
    for standard in &report.imported_standards {
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            escape_html(&standard.source),
            escape_html(&standard.version),
            standard.rule_count,
            standard.automated_count
        );
    }
    out.push_str("</tbody>\n</table>\n");
    for standard in &report.imported_standards {
        for note in &standard.notes {
            let _ = writeln!(out, "<p class=\"note\">{}</p>", escape_html(note));
        }
    }
    out.push_str("</section>\n");
}

/// 渲染质量分层表。
fn push_layers(out: &mut String, report: &ReviewReport) {
    out.push_str(
        "<section><h2>质量分层</h2>\n<table>\n<thead><tr>\
         <th>分层</th><th>发现</th><th>已确认</th><th>已豁免</th>\
         <th>达到阻断阈值</th></tr></thead>\n<tbody>\n",
    );
    for (name, layer) in [
        ("正确性与安全", &report.layers.correctness),
        ("工程规范", &report.layers.engineering),
        ("可维护性与设计", &report.layers.maintainability),
        ("验证可信度", &report.layers.verification),
    ] {
        let _ = writeln!(
            out,
            "<tr><td>{name}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            layer.count, layer.confirmed, layer.exempted, layer.blocking
        );
    }
    out.push_str("</tbody>\n</table>\n</section>\n");
}

/// 渲染发现列表。
///
/// 已豁免的发现仍然在这里：它多一行「已豁免」的说明，但不从报告里消失
/// （FR-062）。
fn push_findings(out: &mut String, report: &ReviewReport) {
    out.push_str("<section><h2>发现列表</h2>\n");
    if report.findings.is_empty() {
        out.push_str("<p class=\"empty\">未发现工程质量问题。</p>\n");
    }
    for finding in &report.findings {
        let _ = writeln!(
            out,
            "<article class=\"finding {}\">\n<h3><span class=\"sev\">{}</span> {}</h3>",
            severity_class(finding.severity),
            finding.severity.as_str(),
            escape_html(&finding.title)
        );
        let _ = writeln!(
            out,
            "<p class=\"detail\">{}</p>",
            escape_html(&finding.detail)
        );
        if let Some(exemption) = &finding.exemption {
            let _ = writeln!(
                out,
                "<p class=\"exempt\">已豁免：{}（批准人：{}；范围：{}）</p>",
                escape_html(&exemption.reason),
                escape_html(&exemption.granted_by),
                escape_html(&exemption.scope.describe())
            );
        }
        push_locations(out, finding);
        push_facts(out, finding);
        if let Some(remediation) = &finding.remediation {
            let _ = writeln!(
                out,
                "<p class=\"fix\">建议：{}</p>",
                escape_html(&remediation.summary)
            );
        }
        out.push_str("</article>\n");
    }
    out.push_str("</section>\n");
}

/// 渲染一条发现的代码位置列表。
fn push_locations(out: &mut String, finding: &Finding) {
    if finding.locations.is_empty() {
        return;
    }
    out.push_str("<ul class=\"locations\">\n");
    for location in &finding.locations {
        let _ = writeln!(
            out,
            "<li><code>{}</code></li>",
            escape_html(&location.display())
        );
    }
    out.push_str("</ul>\n");
}

/// 渲染一条发现的结构事实。
fn push_facts(out: &mut String, finding: &Finding) {
    if finding.facts.is_empty() {
        return;
    }
    out.push_str("<ul class=\"facts\">\n");
    for (key, value) in &finding.facts {
        let _ = writeln!(
            out,
            "<li><code>{}</code> = {}</li>",
            escape_html(key),
            escape_html(value)
        );
    }
    out.push_str("</ul>\n");
}

/// 渲染警告段。
fn push_warnings(out: &mut String, report: &ReviewReport) {
    if report.warnings.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "<section class=\"warnings\"><h2>警告（{}）</h2>\n<ul>",
        report.warnings.len()
    );
    for warning in &report.warnings {
        let _ = writeln!(out, "<li>{}</li>", escape_html(warning));
    }
    out.push_str("</ul>\n</section>\n");
}

/// 内联样式。
///
/// 刻意保持朴素：报告要在 CI 日志、本地文件与代码评审工具里都可读，因此只用
/// 系统字体与中性配色，不依赖任何外部样式表或字体服务。
const HTML_STYLE: &str = r#"<style>
:root { color-scheme: light dark; }
body { font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
       line-height: 1.55; margin: 0 auto; max-width: 60rem; padding: 2rem 1.25rem; }
h1 { font-size: 1.6rem; margin-bottom: .25rem; }
h2 { font-size: 1.15rem; margin-top: 2rem; }
h3 { font-size: 1rem; margin: 0 0 .35rem; }
.meta dl { display: grid; grid-template-columns: auto 1fr; gap: .15rem 1rem;
           font-size: .9rem; margin: 0; }
.meta dt { color: #6b7280; }
.meta dd { margin: 0; }
.gate { border-left: 4px solid; padding: .5rem 1rem; border-radius: .25rem; }
.gate p { margin: .25rem 0 0; font-size: .9rem; }
.gate.passed { border-color: #15803d; background: #f0fdf4; }
.gate.failed { border-color: #b91c1c; background: #fef2f2; }
.gate.insufficient { border-color: #b45309; background: #fffbeb; }
.gate.cancelled { border-color: #4b5563; background: #f9fafb; }
table { border-collapse: collapse; font-size: .9rem; width: 100%; }
th, td { border-bottom: 1px solid #e5e7eb; padding: .35rem .5rem; text-align: left; }
th { color: #6b7280; font-weight: 600; }
.finding { border: 1px solid #e5e7eb; border-radius: .375rem;
           margin: .75rem 0; padding: .75rem 1rem; }
.sev { font-size: .75rem; text-transform: uppercase; letter-spacing: .04em;
       padding: .1rem .4rem; border-radius: .25rem; background: #e5e7eb; }
.finding.high .sev, .finding.blocker .sev { background: #fee2e2; color: #991b1b; }
.finding.medium .sev { background: #fef3c7; color: #92400e; }
.detail, .fix { font-size: .9rem; margin: .35rem 0 0; }
.exempt { background: #eff6ff; border-left: 3px solid #2563eb;
          font-size: .85rem; margin: .5rem 0 0; padding: .35rem .6rem; }
.note { color: #6b7280; font-size: .85rem; margin: .25rem 0 0; }
ul { margin: .35rem 0 0; padding-left: 1.25rem; }
code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85em; }
.empty { color: #6b7280; }
.warnings { font-size: .9rem; }
footer { color: #6b7280; font-size: .8rem; margin-top: 3rem; }
</style>
"#;

/// 门禁结论的 CSS 类名。
const fn gate_class(gate: GateStatus) -> &'static str {
    match gate {
        GateStatus::Passed => "passed",
        GateStatus::Failed => "failed",
        GateStatus::InsufficientEvidence => "insufficient",
        GateStatus::Cancelled => "cancelled",
    }
}

/// 严重程度的 CSS 类名。
const fn severity_class(severity: Severity) -> &'static str {
    match severity {
        Severity::Blocker => "blocker",
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low | Severity::Info => "low",
    }
}

/// 门禁结论的展开解释。
///
/// HTML 报告经常被单独转发给不看终端输出的人，因此必须把「为什么是这个结论」
/// 写进报告本身，而不是让人去翻日志。
fn gate_explanation(report: &ReviewReport) -> String {
    let exempted = report.exempted_count();
    let blocking = report.blocking_findings().count();
    match report.gate {
        GateStatus::Passed => format!(
            "全部检查完整执行，没有达到阻断阈值（{}）且未被豁免的发现。{}",
            report.layers.block_on.as_str(),
            if exempted == 0 {
                String::new()
            } else {
                format!("其中 {exempted} 个发现已被豁免压制，仍列在下方。")
            }
        ),
        GateStatus::Failed => format!(
            "有 {blocking} 个发现达到阻断阈值（{}）且未被豁免。",
            report.layers.block_on.as_str()
        ),
        GateStatus::InsufficientEvidence => {
            if report.file_count == 0 {
                "没有可分析的文件：空目录不是「没有问题的代码」，而是「没有看过代码」。".to_owned()
            } else {
                "检查未能完整执行（工具缺失、超时或崩溃），证据不足以判定。".to_owned()
            }
        }
        GateStatus::Cancelled => "本次审查被用户或系统取消。".to_owned(),
    }
}

/// 转义 HTML 特殊字符。
///
/// 覆盖 `<`、`>`、`&`、`"`、`'`：前三个破坏结构，后两个破坏属性值。发现文本
/// 与仓库路径都是不可信输入，任何一处漏转义都可能让报告变成注入点。
fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// JSON 报告结构。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonReport {
    /// 格式标识。
    pub schema: &'static str,
    /// 运行标识。
    pub run_id: String,
    /// 项目根目录。
    pub project_root: String,
    /// 代码版本。
    pub revision: String,
    /// 分析到的文件数。
    pub file_count: usize,
    /// 索引到的符号数。
    pub symbol_count: usize,
    /// 规则集版本。
    pub ruleset_version: String,
    /// 本次导入的项目规范（FR-058）。
    pub imported_standards: Vec<pf_core::review::ImportedStandard>,
    /// 门禁结论。
    pub gate: GateStatus,
    /// 质量分层。
    pub layers: JsonLayers,
    /// 发现列表。
    pub findings: Vec<JsonFinding>,
    /// 警告。
    pub warnings: Vec<String>,
    /// 证据摘要。
    pub evidence: Vec<JsonEvidence>,
    /// 变更影响分析（FR-025）；没有基线时为 `null`。
    ///
    /// 与终端报告分开呈现同一份数据：影响范围是「变了的那批文件会波及谁」，
    /// 而本轮只分析了前者。合成进 `findings` 会让读者以为这些文件也被审查过。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<pf_core::review::ImpactSummary>,
}

/// JSON 中的分层结构。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonLayers {
    /// 正确性与安全。
    pub correctness: JsonLayer,
    /// 工程规范。
    pub engineering: JsonLayer,
    /// 可维护性与设计。
    pub maintainability: JsonLayer,
    /// 验证可信度。
    pub verification: JsonLayer,
    /// 阻断阈值。
    pub block_on: Severity,
}

/// JSON 中的单层汇总。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonLayer {
    /// 发现数量（含已豁免）。
    pub count: usize,
    /// 已确认数量（不含已豁免）。
    pub confirmed: usize,
    /// 已豁免数量。
    pub exempted: usize,
    /// 达到阻断阈值且未被豁免的数量。
    pub blocking: usize,
}

/// JSON 中的单条发现。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonFinding {
    /// 发现 ID。
    pub id: String,
    /// 触发规则。
    pub rule_id: String,
    /// 问题类型标签。
    pub kind: &'static str,
    /// 所属大类。
    pub category: FindingCategory,
    /// 标题。
    pub title: String,
    /// 详细说明。
    pub detail: String,
    /// 位置。
    pub locations: Vec<pf_core::location::Location>,
    /// 严重程度。
    pub severity: Severity,
    /// 证据状态。
    pub status: EvidenceStatus,
    /// 结构事实。
    pub facts: Vec<(String, String)>,
    /// 修复建议。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<Remediation>,
    /// 关联需求。
    pub requirement_refs: Vec<String>,
    /// 是否已被豁免压制（FR-062）。
    pub exempted: bool,
    /// 豁免详情；未豁免时为 `null`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exemption: Option<JsonExemption>,
}

/// JSON 中的豁免详情。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonExemption {
    /// 被豁免的规则。
    pub rule_id: String,
    /// 适用范围的中文描述。
    pub scope: String,
    /// 为什么豁免。
    pub reason: String,
    /// 谁批准的。
    pub granted_by: String,
    /// 过期日期；`None` 表示长期有效。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<String>,
}

/// JSON 中的证据条目。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonEvidence {
    /// 证据类型。
    pub kind: String,
    /// 内容摘要（前 500 字符）。
    pub content_preview: String,
    /// SHA-256。
    pub sha256: String,
}

impl From<&LayerSummary> for JsonLayer {
    fn from(value: &LayerSummary) -> Self {
        Self {
            count: value.count,
            confirmed: value.confirmed,
            exempted: value.exempted,
            blocking: value.blocking,
        }
    }
}

impl From<&QualityLayers> for JsonLayers {
    fn from(value: &QualityLayers) -> Self {
        Self {
            correctness: (&value.correctness).into(),
            engineering: (&value.engineering).into(),
            maintainability: (&value.maintainability).into(),
            verification: (&value.verification).into(),
            block_on: value.block_on,
        }
    }
}

impl From<&Finding> for JsonFinding {
    fn from(value: &Finding) -> Self {
        Self {
            id: value.id.to_string(),
            rule_id: value.rule_id.to_string(),
            kind: value.kind.label(),
            category: value.kind.category(),
            title: value.title.clone(),
            detail: value.detail.clone(),
            locations: value.locations.clone(),
            severity: value.severity,
            status: value.status,
            facts: value.facts.clone(),
            remediation: value.remediation.clone(),
            requirement_refs: value.requirement_refs.clone(),
            exempted: value.is_exempted(),
            exemption: value.exemption.as_ref().map(|exemption| JsonExemption {
                rule_id: exemption.rule_id.to_string(),
                scope: exemption.scope.describe(),
                reason: exemption.reason.clone(),
                granted_by: exemption.granted_by.clone(),
                expires_on: exemption.expires_on.clone(),
            }),
        }
    }
}

impl From<&pf_core::evidence::EvidenceArtifact> for JsonEvidence {
    fn from(value: &pf_core::evidence::EvidenceArtifact) -> Self {
        Self {
            kind: value.kind.clone(),
            content_preview: value.content.chars().take(500).collect(),
            sha256: value.sha256.clone(),
        }
    }
}

impl From<&ReviewReport> for JsonReport {
    fn from(report: &ReviewReport) -> Self {
        Self {
            schema: "proofforge/report/1",
            run_id: report.run_id.to_string(),
            project_root: report.project_root.display().to_string(),
            revision: report.revision.clone(),
            file_count: report.file_count,
            symbol_count: report.symbol_count,
            ruleset_version: report.ruleset_version.clone(),
            imported_standards: report.imported_standards.clone(),
            gate: report.gate,
            layers: (&report.layers).into(),
            findings: report.findings.iter().map(JsonFinding::from).collect(),
            warnings: report.warnings.clone(),
            evidence: report.evidence.iter().map(JsonEvidence::from).collect(),
            impact: report.impact.clone(),
        }
    }
}

/// SARIF 2.1.0 报告（只包含 GitHub 代码扫描必需的字段）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifReport {
    /// SARIF 版本。
    pub version: &'static str,
    /// JSON Schema 地址。GitHub 代码扫描与多数校验器据此判定方言。
    #[serde(rename = "$schema")]
    pub schema: &'static str,
    /// 输出流。
    pub runs: Vec<SarifRun>,
}

/// 一次 SARIF run。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifRun {
    /// 工具描述。
    pub tool: SarifTool,
    /// 结果列表。
    pub results: Vec<SarifResult>,
}

/// SARIF 工具描述。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifTool {
    /// 驱动信息。
    pub driver: SarifDriver,
}

/// SARIF 驱动信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifDriver {
    /// 工具名。
    pub name: &'static str,
    /// 版本。
    pub version: String,
    /// 规则列表。
    pub rules: Vec<SarifRule>,
}

/// SARIF 规则描述。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifRule {
    /// 规则 ID。
    pub id: String,
    /// 规则说明。
    pub short_description: SarifText,
}

/// SARIF 多语言文本。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifText {
    /// 文本内容。
    pub text: String,
}

/// SARIF 单条结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifResult {
    /// 规则 ID。
    #[serde(rename = "ruleId")]
    pub rule_id: String,
    /// 级别映射。
    pub level: &'static str,
    /// 消息。
    pub message: SarifText,
    /// 位置列表。
    pub locations: Vec<SarifLocation>,
    /// 稳定指纹（FR-068 / FR-061）。
    ///
    /// GitHub 用 `partialFingerprints` 判断「这是不是同一个问题」：没有它，
    /// 每次审查都会把全部发现当成新问题重新开一遍。指纹取 [`FindingId`]——
    /// 它由规则、类型、标题与位置派生，与运行标识无关，因此同一问题在两次
    /// 审查中得到同一个指纹。
    #[serde(
        rename = "partialFingerprints",
        skip_serializing_if = "Option::is_none"
    )]
    pub partial_fingerprints: Option<SarifFingerprints>,
    /// 抑制信息（FR-062）。
    ///
    /// 已豁免的发现仍然出现在 SARIF 里，只是带 `suppressions`：代码扫描平台
    /// 因此不会把它算进未解决问题，而审计仍能看到「这里压掉了一个问题」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressions: Option<Vec<SarifSuppression>>,
}

/// SARIF 稳定指纹集合。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifFingerprints {
    /// `proofforge/v1` 指纹命名空间。
    #[serde(rename = "proofforge/v1")]
    pub proofforge_v1: String,
}

/// SARIF 抑制信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifSuppression {
    /// 抑制类型：`external` 表示由外部配置（豁免文件）压制。
    pub kind: &'static str,
    /// 压制理由，直接来自豁免的 `reason`。
    pub justification: String,
}

/// SARIF 位置。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifLocation {
    /// 物理位置。
    #[serde(rename = "physicalLocation")]
    pub physical_location: SarifPhysicalLocation,
}

/// SARIF 物理位置。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifPhysicalLocation {
    /// 文件位置。
    #[serde(rename = "artifactLocation")]
    pub artifact_location: SarifArtifactLocation,
    /// 区域。
    pub region: SarifRegion,
}

/// SARIF 文件位置。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifArtifactLocation {
    /// 仓库相对路径。
    pub uri: String,
}

/// SARIF 区域。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SarifRegion {
    /// 起始行。
    #[serde(rename = "startLine")]
    pub start_line: u32,
    /// 起始列。
    #[serde(rename = "startColumn")]
    pub start_column: u32,
}

impl From<&ReviewReport> for SarifReport {
    fn from(report: &ReviewReport) -> Self {
        let mut rules: Vec<SarifRule> = Vec::new();
        let mut results: Vec<SarifResult> = Vec::new();

        for finding in &report.findings {
            let rule_id = finding.rule_id.to_string();
            if !rules.iter().any(|rule| rule.id == rule_id) {
                rules.push(SarifRule {
                    id: rule_id.clone(),
                    short_description: SarifText {
                        text: finding.title.clone(),
                    },
                });
            }
            let locations = finding
                .locations
                .iter()
                .map(|location| SarifLocation {
                    physical_location: SarifPhysicalLocation {
                        artifact_location: SarifArtifactLocation {
                            uri: location.path.clone(),
                        },
                        region: SarifRegion {
                            start_line: location.start_line.unwrap_or(1),
                            start_column: location.start_column.unwrap_or(1),
                        },
                    },
                })
                .collect();
            results.push(SarifResult {
                rule_id,
                level: sarif_level(finding.severity),
                message: SarifText {
                    text: finding.detail.clone(),
                },
                locations,
                partial_fingerprints: Some(SarifFingerprints {
                    proofforge_v1: finding.id.to_string(),
                }),
                suppressions: finding.exemption.as_ref().map(|exemption| {
                    vec![SarifSuppression {
                        kind: "external",
                        justification: format!(
                            "{}（批准人：{}；范围：{}）",
                            exemption.reason,
                            exemption.granted_by,
                            exemption.scope.describe()
                        ),
                    }]
                }),
            });
        }

        Self {
            version: "2.1.0",
            schema: "https://json.schemastore.org/sarif-2.1.0.json",
            runs: vec![SarifRun {
                tool: SarifTool {
                    driver: SarifDriver {
                        name: "ProofForge",
                        version: report.ruleset_version.clone(),
                        rules,
                    },
                },
                results,
            }],
        }
    }
}

/// 把内部严重程度映射为 SARIF 级别。
fn sarif_level(severity: Severity) -> &'static str {
    match severity {
        Severity::Blocker | Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low | Severity::Info => "note",
    }
}

/// 计算报告的可读摘要行（用于 CLI 的最后一行输出）。
#[must_use]
pub fn summary_line(report: &ReviewReport) -> String {
    format!(
        "{}: {} findings across {} files ({} confirmed)",
        gate_line(report.gate),
        report.findings.len(),
        report.file_count,
        report
            .findings
            .iter()
            .filter(|finding| finding.status.is_evidence_backed())
            .count()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::finding::FindingKind;
    use pf_core::ids::{FindingId, ProjectId, RuleId, RunId};
    use pf_core::location::{Location, Span};
    use std::time::Duration;

    fn sample_report() -> ReviewReport {
        // 与真实审查一致：检查器会用 `Rule::effective_severity()` 覆盖种类的
        // 默认严重程度，并把有静态证据的发现标记为 StaticConfirmed。
        let finding = Finding::new(
            RuleId::builtin("public-api-missing-docs"),
            FindingKind::MissingDocumentation,
            "公共函数 `f` 缺少文档",
            "详细说明",
            vec![Location::new("src/lib.rs".to_owned(), Span::new(0, 10)).with_lines(4, 1, 4, 11)],
        )
        .with_severity(Severity::Medium)
        .with_status(EvidenceStatus::StaticConfirmed);
        ReviewReport {
            impact: None,
            run_id: RunId::new(),
            project_id: ProjectId::new(),
            project_root: std::path::PathBuf::from("/tmp/demo"),
            revision: "workdir".to_owned(),
            file_count: 3,
            symbol_count: 12,
            findings: vec![finding],
            layers: QualityLayers {
                correctness: LayerSummary {
                    count: 0,
                    confirmed: 0,
                    exempted: 0,
                    blocking: 0,
                },
                engineering: LayerSummary {
                    count: 1,
                    confirmed: 1,
                    exempted: 0,
                    blocking: 0,
                },
                maintainability: LayerSummary {
                    count: 0,
                    confirmed: 0,
                    exempted: 0,
                    blocking: 0,
                },
                verification: LayerSummary {
                    count: 0,
                    confirmed: 0,
                    exempted: 0,
                    blocking: 0,
                },
                block_on: Severity::High,
            },
            gate: GateStatus::Passed,
            ruleset_version: "0.1.0".to_owned(),
            imported_standards: Vec::new(),
            duration: Duration::from_millis(42),
            evidence: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn terminal_report_contains_key_sections() {
        let text = render_terminal(&sample_report());
        assert!(text.contains("ProofForge 工程质量审查"));
        assert!(text.contains("质量分层"));
        assert!(text.contains("门禁结论"));
        assert!(text.contains("src/lib.rs:4:1"));
    }

    #[test]
    fn json_report_is_valid_and_has_schema() {
        let json = render_json(&sample_report()).expect("json");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["schema"], "proofforge/report/1");
        assert_eq!(value["gate"], "passed");
        assert_eq!(value["findings"][0]["kind"], "missing_documentation");
        assert_eq!(value["findings"][0]["locations"][0]["start_line"], 4);
    }

    #[test]
    fn json_report_carries_the_impact_analysis_when_there_is_one() {
        // FR-025：影响范围不能只出现在终端输出里。JSON 消费者（CI、看板）看不到
        // 它，「可能波及哪些文件」这个结论就对机器不存在。
        let mut report = sample_report();
        report.impact = Some(pf_core::review::ImpactSummary::new(
            vec!["src/changed.rs".to_owned()],
            vec!["src/dependent.rs".to_owned()],
        ));
        let value: serde_json::Value =
            serde_json::from_str(&render_json(&report).expect("json")).expect("parse");
        assert_eq!(value["impact"]["changed"][0], "src/changed.rs", "{value}");
        assert_eq!(
            value["impact"]["impacted"][0], "src/dependent.rs",
            "{value}"
        );
    }

    #[test]
    fn json_report_omits_impact_when_there_is_no_baseline() {
        // 没有基线时「影响范围」这个词不成立，因此字段必须缺席而不是给个空数组——
        // 空数组会被读成「没有文件受影响」，那是另一个结论。
        let value: serde_json::Value =
            serde_json::from_str(&render_json(&sample_report()).expect("json")).expect("parse");
        assert!(value.get("impact").is_none(), "{value}");
    }

    #[test]
    fn sarif_report_is_valid_2_1_0() {
        let sarif = render_sarif(&sample_report()).expect("sarif");
        let value: serde_json::Value = serde_json::from_str(&sarif).expect("parse");
        assert_eq!(value["version"], "2.1.0");
        // GitHub 代码扫描与多数校验器按 $schema 判定方言；缺了它有些平台会
        // 把报告当成通用 JSON 直接拒收。
        assert!(
            value["$schema"]
                .as_str()
                .is_some_and(|schema| schema.ends_with("sarif-2.1.0.json")),
            "{value}"
        );
        assert_eq!(value["runs"][0]["tool"]["driver"]["name"], "ProofForge");
        assert_eq!(value["runs"][0]["results"][0]["level"], "warning");
        assert_eq!(
            value["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]
                ["startLine"],
            4
        );
    }

    #[test]
    fn sarif_results_carry_a_stable_fingerprint() {
        // 没有 partialFingerprints，代码扫描平台会把每次审查的全部发现都当成
        // 新问题重开一遍（FR-068）。
        let report = sample_report();
        let fingerprint = report.findings[0].id.to_string();
        let sarif = render_sarif(&report).expect("sarif");
        let value: serde_json::Value = serde_json::from_str(&sarif).expect("parse");
        assert_eq!(
            value["runs"][0]["results"][0]["partialFingerprints"]["proofforge/v1"],
            fingerprint
        );
        // 指纹与运行标识无关：同一问题在两次审查里必须得到同一个值。
        assert_ne!(fingerprint, "", "指纹不能为空");
        assert!(
            value["runs"][0]["results"][0]["suppressions"].is_null(),
            "未豁免的发现不应带 suppressions"
        );
    }

    #[test]
    fn an_exempted_finding_is_suppressed_in_sarif_but_not_removed() {
        // FR-062：SARIF 里用 suppressions 表达「已知且有理由」，而不是删掉结果。
        let mut report = sample_report();
        report.findings[0].exemption = Some(pf_core::exempt::Exemption {
            rule_id: RuleId::builtin("public-api-missing-docs"),
            scope: pf_core::exempt::ExemptionScope::File {
                path: "src/lib.rs".to_owned(),
            },
            reason: "对外契约，改名排期在 #42".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        });
        let sarif = render_sarif(&report).expect("sarif");
        let value: serde_json::Value = serde_json::from_str(&sarif).expect("parse");
        let suppression = &value["runs"][0]["results"][0]["suppressions"][0];
        assert_eq!(suppression["kind"], "external");
        assert!(
            suppression["justification"]
                .as_str()
                .is_some_and(|text| text.contains("#42") && text.contains("alice")),
            "{suppression}"
        );
    }

    #[test]
    fn json_report_marks_exemptions_without_hiding_them() {
        let mut report = sample_report();
        report.findings[0].exemption = Some(pf_core::exempt::Exemption {
            rule_id: RuleId::builtin("public-api-missing-docs"),
            scope: pf_core::exempt::ExemptionScope::Rule,
            reason: "排期在 #42".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        });
        // 分层汇总由发现推导，因此改完发现必须重算，否则测到的是夹具里的旧值。
        report.layers = pf_core::review::summarize(&report.findings, report.layers.block_on);
        let json = render_json(&report).expect("json");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["findings"][0]["exempted"], true);
        assert_eq!(value["findings"][0]["exemption"]["granted_by"], "alice");
        assert_eq!(value["layers"]["engineering"]["exempted"], 1);
        assert_eq!(value["layers"]["engineering"]["blocking"], 0);
    }

    #[test]
    fn html_report_is_self_contained_and_escapes_untrusted_text() {
        // 仓库里的字符串是不可信输入：发现文本里的 `<` 必须被转义，否则报告本身
        // 变成注入点。同时 HTML 不得引用外部资源——它要能随 CI 工件离线归档。
        let mut report = sample_report();
        report.findings[0].title = "危险 <script>alert(1)</script> & \"引号\"".to_owned();
        let html = render_html(&report);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("</html>"));
        assert!(!html.contains("<script>alert(1)</script>"), "未转义的脚本");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&amp;"));
        assert!(!html.contains("<link"), "不得引用外部样式表");
        assert!(!html.contains("http://") && !html.contains("https://"));
        assert!(html.contains("门禁结论"));
        assert!(html.contains("src/lib.rs:4:1"));
    }

    #[test]
    fn html_report_shows_the_exemption_reason_and_grantor() {
        let mut report = sample_report();
        report.findings[0].exemption = Some(pf_core::exempt::Exemption {
            rule_id: RuleId::builtin("public-api-missing-docs"),
            scope: pf_core::exempt::ExemptionScope::Rule,
            reason: "排期在 #42".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        });
        let html = render_html(&report);
        assert!(html.contains("已豁免：排期在 #42"));
        assert!(html.contains("批准人：alice"));
        assert!(html.contains("class=\"exempt\""));
    }

    #[test]
    fn terminal_report_shows_the_exemption_reason_and_grantor() {
        let mut report = sample_report();
        report.findings[0].exemption = Some(pf_core::exempt::Exemption {
            rule_id: RuleId::builtin("public-api-missing-docs"),
            scope: pf_core::exempt::ExemptionScope::Rule,
            reason: "排期在 #42".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        });
        report.layers = pf_core::review::summarize(&report.findings, report.layers.block_on);
        let text = render_terminal(&report);
        assert!(text.contains("已豁免：排期在 #42"));
        assert!(text.contains("批准人：alice"));
        assert!(text.contains("已豁免 1"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let report = sample_report();
        assert_eq!(
            render_json(&report).expect("a"),
            render_json(&report).expect("b")
        );
        assert_eq!(render_terminal(&report), render_terminal(&report));
    }

    #[test]
    fn summary_line_reports_counts() {
        assert_eq!(
            summary_line(&sample_report()),
            "通过 (exit 0): 1 findings across 3 files (1 confirmed)"
        );
    }

    #[test]
    fn gate_line_covers_all_states() {
        assert!(gate_line(GateStatus::Failed).contains("exit 1"));
        assert!(gate_line(GateStatus::InsufficientEvidence).contains("exit 2"));
        assert!(gate_line(GateStatus::Cancelled).contains("exit 3"));
    }

    #[test]
    fn report_helpers_use_ids() {
        // 保证 FindingId / RunId 在报告模块中被真实使用（避免死代码警告）。
        let report = sample_report();
        assert!(report.finding(&FindingId::new()).is_none());
        assert_ne!(report.run_id.to_string(), "");
    }
}
