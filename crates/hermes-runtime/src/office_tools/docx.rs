use std::fmt::Write as _;
use std::path::Path;

use serde_json::{json, Value};

use crate::doc_extract::{collect_docx_paragraph, find_descendants, parse_xml, ZipArchive};
use crate::excel_tools::resolve_workspace_path;

use super::{
    parse_inline_runs, parse_markdown_to_doc_blocks, parse_structured_doc_blocks,
    read_bounded_office_file, write_zip_entries, xml_escape, DocBlock, InlineRun, TableData,
    DEFAULT_MAX_PARAGRAPHS, HARD_MAX_PARAGRAPHS,
};

// ── DOCX Reader & Writer ───────────────────────────────────────────────────────────────────────

/// Read a Word `.docx` document inside the workspace into structured paragraphs and tables.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.docx` path inside the workspace
/// - `max_paragraphs`: optional paragraph cap (default 5000, max 50000)
pub(crate) fn docx_read(workspace_root: &Path, profile: Option<&Path>, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "docx_read requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, profile, path_str, true) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let max_paragraphs = args["max_paragraphs"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(DEFAULT_MAX_PARAGRAPHS)
        .clamp(1, HARD_MAX_PARAGRAPHS);

    let data = match read_bounded_office_file(&resolved, "docx") {
        Ok(b) => b,
        Err(err) => return json!({"error": err}),
    };
    let zip = match ZipArchive::from_bytes(&data, "DOCX") {
        Ok(z) => z,
        Err(err) => return json!({"error": err}),
    };
    let doc_bytes = match zip.read_entry("word/document.xml") {
        Ok(b) => b,
        Err(err) => return json!({"error": err}),
    };
    let root = match parse_xml(&doc_bytes, "word/document.xml") {
        Ok(r) => r,
        Err(err) => return json!({"error": err}),
    };

    let mut bodies = Vec::new();
    find_descendants(&root, "body", &mut bodies);
    let container = bodies.first().copied().unwrap_or(&root);

    let mut paragraphs = Vec::new();
    let mut tables = Vec::new();
    let mut truncated = false;

    for child in container.children() {
        match child.local_name() {
            "p" => {
                let mut buf = String::new();
                collect_docx_paragraph(child, &mut buf);
                if !buf.trim().is_empty() {
                    if paragraphs.len() < max_paragraphs {
                        paragraphs.push(buf);
                    } else {
                        truncated = true;
                    }
                }
            }
            "tbl" => {
                let mut tbl_rows = Vec::new();
                for tr in child.children().iter().filter(|n| n.local_name() == "tr") {
                    let mut row_cells = Vec::new();
                    for tc in tr.children().iter().filter(|n| n.local_name() == "tc") {
                        let mut cell_parts = Vec::new();
                        for p in tc.children().iter().filter(|n| n.local_name() == "p") {
                            let mut buf = String::new();
                            collect_docx_paragraph(p, &mut buf);
                            if !buf.is_empty() {
                                cell_parts.push(buf);
                            }
                        }
                        row_cells.push(cell_parts.join("\n"));
                    }
                    if !row_cells.is_empty() {
                        tbl_rows.push(row_cells);
                    }
                }
                if !tbl_rows.is_empty() {
                    tables.push(tbl_rows);
                }
            }
            _ => {}
        }
    }

    json!({
        "path": path_str,
        "paragraphs": paragraphs,
        "tables": tables,
        "paragraph_count": paragraphs.len(),
        "table_count": tables.len(),
        "truncated": truncated,
    })
}

fn docx_push_run(out: &mut String, run: &InlineRun, force_bold: bool) {
    if run.text.is_empty() {
        return;
    }
    let is_bold = force_bold || run.bold;
    out.push_str("<w:r>");
    let has_props = is_bold
        || run.italic
        || run.code
        || run.underline
        || run.strike
        || run.color.is_some()
        || run.size_pt.is_some();
    if has_props {
        out.push_str("<w:rPr>");
        if run.code {
            out.push_str(
                r#"<w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas" w:eastAsia="Consolas"/>"#,
            );
        }
        if is_bold {
            out.push_str("<w:b/><w:bCs/>");
        }
        if run.italic {
            out.push_str("<w:i/><w:iCs/>");
        }
        if run.strike {
            out.push_str("<w:strike/>");
        }
        if let Some(color) = &run.color {
            let _ = write!(out, r#"<w:color w:val="{color}"/>"#);
        } else if run.code {
            out.push_str(r#"<w:color w:val="1E3A8A"/>"#);
        }
        if let Some(pt) = run.size_pt {
            let half_pts = (pt * 2.0).round().clamp(12.0, 144.0) as u32;
            let _ = write!(
                out,
                r#"<w:sz w:val="{half_pts}"/><w:szCs w:val="{half_pts}"/>"#
            );
        } else if run.code {
            out.push_str(r#"<w:sz w:val="20"/><w:szCs w:val="20"/>"#);
        }
        if run.underline {
            out.push_str(r#"<w:u w:val="single"/>"#);
        }
        if run.code {
            out.push_str(r#"<w:shd w:val="clear" w:color="auto" w:fill="EEF2F6"/>"#);
        }
        out.push_str("</w:rPr>");
    }

    let lines: Vec<&str> = run.text.split('\n').collect();
    for (idx, part) in lines.iter().enumerate() {
        if idx > 0 {
            out.push_str("<w:br/>");
        }
        if !part.is_empty() {
            let escaped = xml_escape(part);
            let _ = write!(out, r#"<w:t xml:space="preserve">{escaped}</w:t>"#);
        }
    }
    out.push_str("</w:r>");
}

fn docx_push_runs(out: &mut String, runs: &[InlineRun], force_bold: bool) {
    for run in runs {
        docx_push_run(out, run, force_bold);
    }
}

fn docx_jc_val(align: Option<&str>) -> Option<&'static str> {
    match align.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("center") => Some("center"),
        Some("right") => Some("right"),
        Some("justify" | "both") => Some("both"),
        Some("left") => Some("left"),
        _ => None,
    }
}

fn docx_render_table(out: &mut String, tbl: &TableData) {
    let col_count = tbl
        .headers
        .as_ref()
        .map(Vec::len)
        .unwrap_or_else(|| tbl.rows.first().map(Vec::len).unwrap_or(1))
        .max(1);
    let total_width = 9026usize; // A4 usable width (11906 - 2*1440 dxa)
    let col_width = (total_width / col_count).max(600);

    out.push_str("<w:tbl><w:tblPr>");
    let _ = write!(out, r#"<w:tblW w:w="{total_width}" w:type="dxa"/>"#);
    out.push_str(
        r#"<w:tblBorders><w:top w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:left w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:bottom w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:right w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:insideH w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="CBD5E1"/></w:tblBorders>"#,
    );
    out.push_str(
        r#"<w:tblCellMar><w:top w:w="80" w:type="dxa"/><w:left w:w="120" w:type="dxa"/><w:bottom w:w="80" w:type="dxa"/><w:right w:w="120" w:type="dxa"/></w:tblCellMar>"#,
    );
    out.push_str("</w:tblPr><w:tblGrid>");
    for _ in 0..col_count {
        let _ = write!(out, r#"<w:gridCol w:w="{col_width}"/>"#);
    }
    out.push_str("</w:tblGrid>");

    if let Some(headers) = &tbl.headers {
        out.push_str("<w:tr><w:trPr><w:cantSplit/><w:tblHeader/></w:trPr>");
        for cell in headers {
            out.push_str("<w:tc><w:tcPr>");
            let _ = write!(out, r#"<w:tcW w:w="{col_width}" w:type="dxa"/>"#);
            out.push_str(
                r#"<w:shd w:val="clear" w:color="auto" w:fill="E8EEF5"/><w:vAlign w:val="center"/></w:tcPr>"#,
            );
            out.push_str(r#"<w:p><w:pPr><w:spacing w:before="40" w:after="40"/></w:pPr>"#);
            let runs = parse_inline_runs(cell);
            docx_push_runs(out, &runs, true);
            out.push_str("</w:p></w:tc>");
        }
        out.push_str("</w:tr>");
    }

    for (r_idx, row) in tbl.rows.iter().enumerate() {
        out.push_str("<w:tr><w:trPr><w:cantSplit/></w:trPr>");
        let fill = if r_idx % 2 == 1 { Some("F8FAFC") } else { None };
        for cell in row {
            out.push_str("<w:tc><w:tcPr>");
            let _ = write!(out, r#"<w:tcW w:w="{col_width}" w:type="dxa"/>"#);
            if let Some(bg) = fill {
                let _ = write!(
                    out,
                    r#"<w:shd w:val="clear" w:color="auto" w:fill="{bg}"/>"#
                );
            }
            out.push_str(r#"<w:vAlign w:val="center"/></w:tcPr>"#);
            out.push_str(r#"<w:p><w:pPr><w:spacing w:before="40" w:after="40"/></w:pPr>"#);
            let runs = parse_inline_runs(cell);
            docx_push_runs(out, &runs, false);
            out.push_str("</w:p></w:tc>");
        }
        out.push_str("</w:tr>");
    }
    out.push_str("</w:tbl><w:p><w:pPr><w:spacing w:after=\"120\"/></w:pPr></w:p>");
}

fn docx_numbering_xml(max_num_list_id: usize) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="0">
    <w:multiLevelType w:val="hybridMultilevel"/>
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="◦"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="▪"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="2160" w:hanging="360"/></w:pPr></w:lvl>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="1">
    <w:multiLevelType w:val="hybridMultilevel"/>
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl>
    <w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="lowerRoman"/><w:lvlText w:val="%3."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="2160" w:hanging="360"/></w:pPr></w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
"#,
    );
    for id in 2..=max_num_list_id.max(2) {
        let _ = writeln!(
            xml,
            r#"  <w:num w:numId="{id}"><w:abstractNumId w:val="1"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride></w:num>"#
        );
    }
    xml.push_str("</w:numbering>\n");
    xml
}

fn docx_styles_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults>
    <w:rPrDefault>
      <w:rPr>
        <w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Calibri" w:eastAsia="Calibri"/>
        <w:color w:val="1F2933"/>
        <w:sz w:val="22"/>
        <w:szCs w:val="22"/>
      </w:rPr>
    </w:rPrDefault>
    <w:pPrDefault>
      <w:pPr>
        <w:spacing w:after="120" w:line="276" w:lineRule="auto"/>
      </w:pPr>
    </w:pPrDefault>
  </w:docDefaults>
  <w:style w:type="paragraph" w:default="1" w:styleId="Normal">
    <w:name w:val="Normal"/>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Title">
    <w:name w:val="Title"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="0" w:after="140"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="1F4E79"/>
      <w:sz w:val="48"/><w:szCs w:val="48"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Subtitle">
    <w:name w:val="Subtitle"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:spacing w:before="0" w:after="220"/>
    </w:pPr>
    <w:rPr>
      <w:color w:val="595959"/>
      <w:sz w:val="26"/><w:szCs w:val="26"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="280" w:after="120"/>
      <w:outlineLvl w:val="0"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="1F4E79"/>
      <w:sz w:val="34"/><w:szCs w:val="34"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading2">
    <w:name w:val="heading 2"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="220" w:after="100"/>
      <w:outlineLvl w:val="1"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="2E75B6"/>
      <w:sz w:val="28"/><w:szCs w:val="28"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading3">
    <w:name w:val="heading 3"/>
    <w:basedOn w:val="Normal"/>
    <w:next w:val="Normal"/>
    <w:pPr>
      <w:keepNext/>
      <w:spacing w:before="180" w:after="80"/>
      <w:outlineLvl w:val="2"/>
    </w:pPr>
    <w:rPr>
      <w:b/><w:bCs/>
      <w:color w:val="2E75B6"/>
      <w:sz w:val="24"/><w:szCs w:val="24"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Quote">
    <w:name w:val="Quote"/>
    <w:basedOn w:val="Normal"/>
    <w:pPr>
      <w:pBdr><w:left w:val="single" w:sz="18" w:space="12" w:color="2E75B6"/></w:pBdr>
      <w:shd w:val="clear" w:color="auto" w:fill="F8FAFC"/>
      <w:spacing w:before="100" w:after="120"/>
      <w:ind w:left="360" w:right="240"/>
    </w:pPr>
    <w:rPr>
      <w:i/><w:iCs/>
      <w:color w:val="4A5568"/>
    </w:rPr>
  </w:style>
  <w:style w:type="paragraph" w:styleId="CodeBlock">
    <w:name w:val="Code Block"/>
    <w:basedOn w:val="Normal"/>
    <w:pPr>
      <w:pBdr>
        <w:top w:val="single" w:sz="4" w:space="6" w:color="CBD5E1"/>
        <w:left w:val="single" w:sz="4" w:space="8" w:color="CBD5E1"/>
        <w:bottom w:val="single" w:sz="4" w:space="6" w:color="CBD5E1"/>
        <w:right w:val="single" w:sz="4" w:space="8" w:color="CBD5E1"/>
      </w:pBdr>
      <w:shd w:val="clear" w:color="auto" w:fill="F3F4F6"/>
      <w:spacing w:before="100" w:after="140" w:line="240" w:lineRule="auto"/>
      <w:ind w:left="180" w:right="180"/>
    </w:pPr>
    <w:rPr>
      <w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas" w:eastAsia="Consolas"/>
      <w:color w:val="111827"/>
      <w:sz w:val="19"/><w:szCs w:val="19"/>
    </w:rPr>
  </w:style>
</w:styles>
"#
    .to_owned()
}

pub(super) fn core_props_xml(title: &str, author: &str) -> String {
    let t = xml_escape(title);
    let a = xml_escape(author);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dc:title>{t}</dc:title>
  <dc:creator>{a}</dc:creator>
  <cp:lastModifiedBy>{a}</cp:lastModifiedBy>
</cp:coreProperties>
"#
    )
}

/// Create a formatted `.docx` Word document inside the workspace.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.docx` path inside the workspace
/// - `content`: optional Markdown string OR list of structured block objects
/// - `blocks`: optional list of structured block objects (`heading`, `paragraph`, `bullets`,
///   `numbered`, `table`, `quote`, `code`, `rule`, `page_break`)
/// - `title`: optional document title
/// - `subtitle`: optional document subtitle
/// - `author`: optional author metadata (defaults to `"Hermes"`)
pub(crate) fn docx_write(workspace_root: &Path, profile: Option<&Path>, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "docx_write requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, profile, path_str, false) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };

    let title = args["title"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let subtitle = args["subtitle"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let author = args["author"].as_str().unwrap_or("Hermes");

    let mut next_list_id = 2usize; // numId 1 is reserved for bullets
    let mut blocks = Vec::new();

    if let Some(blocks_arr) = args.get("blocks").and_then(Value::as_array) {
        match parse_structured_doc_blocks(blocks_arr, &mut next_list_id) {
            Ok(b) => blocks.extend(b),
            Err(err) => return json!({"error": err}),
        }
    }
    match args.get("content") {
        Some(Value::String(md)) if !md.trim().is_empty() => {
            blocks.extend(parse_markdown_to_doc_blocks(md, &mut next_list_id));
        }
        Some(Value::Array(arr)) => match parse_structured_doc_blocks(arr, &mut next_list_id) {
            Ok(b) => blocks.extend(b),
            Err(err) => return json!({"error": err}),
        },
        Some(Value::Null) | None => {}
        Some(Value::String(_)) => {}
        _ => {
            return json!({"error": "'content' must be a Markdown string or a list of block objects"});
        }
    }

    if title.is_none() && subtitle.is_none() && blocks.is_empty() {
        return json!({"error": "docx_write requires non-empty 'content', 'blocks', or 'title'"});
    }

    let mut body = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
"#,
    );

    if let Some(t) = title {
        body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr>"#);
        let runs = parse_inline_runs(t);
        docx_push_runs(&mut body, &runs, false);
        body.push_str("</w:p>");
    }
    if let Some(st) = subtitle {
        body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Subtitle"/></w:pPr>"#);
        let runs = parse_inline_runs(st);
        docx_push_runs(&mut body, &runs, false);
        body.push_str("</w:p>");
    }

    let mut tables_written = 0usize;
    for block in &blocks {
        match block {
            DocBlock::Heading { level, text } => {
                let style = match level {
                    1 => "Heading1",
                    2 => "Heading2",
                    _ => "Heading3",
                };
                let _ = write!(body, r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr>"#);
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Paragraph { runs, align } => {
                body.push_str("<w:p>");
                if let Some(jc) = docx_jc_val(align.as_deref()) {
                    let _ = write!(body, r#"<w:pPr><w:jc w:val="{jc}"/></w:pPr>"#);
                }
                docx_push_runs(&mut body, runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Bullet { level, text } => {
                let ilvl = (*level).min(2);
                let _ = write!(
                    body,
                    r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="1"/></w:numPr><w:spacing w:after="60"/></w:pPr>"#
                );
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Numbered {
                level,
                list_id,
                text,
            } => {
                let ilvl = (*level).min(2);
                let _ = write!(
                    body,
                    r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="{list_id}"/></w:numPr><w:spacing w:after="60"/></w:pPr>"#
                );
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Quote { text } => {
                body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Quote"/></w:pPr>"#);
                let runs = parse_inline_runs(text);
                docx_push_runs(&mut body, &runs, false);
                body.push_str("</w:p>");
            }
            DocBlock::Code { code } => {
                body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="CodeBlock"/></w:pPr>"#);
                let run = InlineRun {
                    text: code.clone(),
                    ..Default::default()
                };
                docx_push_run(&mut body, &run, false);
                body.push_str("</w:p>");
            }
            DocBlock::Table(tbl) => {
                tables_written += 1;
                docx_render_table(&mut body, tbl);
            }
            DocBlock::Rule => {
                body.push_str(
                    r#"<w:p><w:pPr><w:pBdr><w:bottom w:val="single" w:sz="6" w:space="1" w:color="CBD5E1"/></w:pBdr><w:spacing w:before="120" w:after="160"/></w:pPr></w:p>"#,
                );
            }
            DocBlock::PageBreak => {
                body.push_str(r#"<w:p><w:r><w:br w:type="page"/></w:r></w:p>"#);
            }
        }
    }

    body.push_str(
        r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>
</w:body>
</w:document>
"#,
    );

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
  <Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
  <Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
  <Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>
</Types>
"#
    .to_owned();

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>
"#
    .to_owned();

    let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>
"#
    .to_owned();

    let app_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">
  <Application>Hermes</Application>
</Properties>
"#
    .to_owned();

    let doc_title = title.unwrap_or("Document");
    let entries = vec![
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/_rels/document.xml.rels", doc_rels),
        ("word/styles.xml", docx_styles_xml()),
        ("word/numbering.xml", docx_numbering_xml(next_list_id - 1)),
        ("word/document.xml", body),
        ("docProps/core.xml", core_props_xml(doc_title, author)),
        ("docProps/app.xml", app_props),
    ];

    if let Err(err) = write_zip_entries(&resolved, &entries) {
        return json!({"error": err});
    }

    json!({
        "status": "ok",
        "path": path_str,
        "blocks_written": blocks.len() + usize::from(title.is_some()) + usize::from(subtitle.is_some()),
        "tables_written": tables_written,
    })
}
