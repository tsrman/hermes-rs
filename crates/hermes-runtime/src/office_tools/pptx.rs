use std::fmt::Write as _;
use std::path::Path;

use serde_json::{json, Value};

use crate::doc_extract::{find_descendants, parse_xml, slide_number, XmlNode, ZipArchive};
use crate::excel_tools::resolve_workspace_path;

use super::docx::core_props_xml;
use super::{
    parse_inline_runs, parse_md_heading, parse_table_value, read_bounded_office_file,
    value_to_string, write_zip_entries, xml_escape, TableData, DEFAULT_MAX_SLIDES, HARD_MAX_SLIDES,
};

// ── PPTX Reader & Writer ───────────────────────────────────────────────────────────────────────

fn extract_a_paragraph_text(p: &XmlNode, skip_slidenum_fields: bool) -> String {
    let mut out = String::new();
    for child in p.children() {
        match child.local_name() {
            "r" => {
                for sub in child.children() {
                    if sub.local_name() == "t" {
                        out.push_str(&sub.collect_text());
                    }
                }
            }
            "fld" => {
                if skip_slidenum_fields && child.attr("type") == Some("slidenum") {
                    continue;
                }
                for sub in child.children() {
                    if sub.local_name() == "t" {
                        out.push_str(&sub.collect_text());
                    }
                }
            }
            "br" => out.push('\n'),
            _ => {}
        }
    }
    out
}

fn resolve_notes_path(zip: &ZipArchive<'_>, slide_num: usize) -> Option<String> {
    let rels_path = format!("ppt/slides/_rels/slide{slide_num}.xml.rels");
    if let Ok(rels_bytes) = zip.read_entry(&rels_path) {
        if let Ok(root) = parse_xml(&rels_bytes, &rels_path) {
            let mut rels = Vec::new();
            find_descendants(&root, "Relationship", &mut rels);
            for rel in rels {
                let is_notes = rel.attr("Type").is_some_and(|t| t.ends_with("/notesSlide"));
                if !is_notes {
                    continue;
                }
                if let Some(target) = rel.attr("Target") {
                    let norm = if let Some(rest) = target.strip_prefix("../") {
                        format!("ppt/{rest}")
                    } else if let Some(rest) = target.strip_prefix('/') {
                        rest.to_owned()
                    } else {
                        format!("ppt/slides/{target}")
                    };
                    if zip.contains(&norm) {
                        return Some(norm);
                    }
                }
            }
        }
    }
    let fallback = format!("ppt/notesSlides/notesSlide{slide_num}.xml");
    zip.contains(&fallback).then_some(fallback)
}

/// Read a PowerPoint `.pptx` presentation inside the workspace into structured slides, tables,
/// and speaker notes.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.pptx` path inside the workspace
/// - `max_slides`: optional slide cap (default 200, max 1000)
pub(crate) fn pptx_read(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "pptx_read requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, true) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let max_slides = args["max_slides"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(DEFAULT_MAX_SLIDES)
        .clamp(1, HARD_MAX_SLIDES);

    let data = match read_bounded_office_file(&resolved, "pptx") {
        Ok(b) => b,
        Err(err) => return json!({"error": err}),
    };
    let zip = match ZipArchive::from_bytes(&data, "PPTX") {
        Ok(z) => z,
        Err(err) => return json!({"error": err}),
    };

    let mut slide_entries: Vec<(usize, String)> = zip
        .entry_names()
        .iter()
        .filter_map(|name| slide_number(name).map(|n| (n, name.clone())))
        .collect();
    slide_entries.sort_by_key(|(n, _)| *n);

    let total_slides = slide_entries.len();
    let truncated = total_slides > max_slides;
    let mut slides_out = Vec::new();

    for (num, entry_name) in slide_entries.into_iter().take(max_slides) {
        let xml_bytes = match zip.read_entry(&entry_name) {
            Ok(b) => b,
            Err(err) => return json!({"error": err}),
        };
        let root = match parse_xml(&xml_bytes, &entry_name) {
            Ok(r) => r,
            Err(err) => return json!({"error": err}),
        };

        let mut title: Option<String> = None;
        let mut texts = Vec::new();
        let mut shapes = Vec::new();
        find_descendants(&root, "sp", &mut shapes);
        for sp in shapes {
            let mut phs = Vec::new();
            find_descendants(sp, "ph", &mut phs);
            let is_title_ph = phs
                .iter()
                .any(|p| matches!(p.attr("type"), Some("title" | "ctrTitle")));
            let is_slide_num_shape = sp
                .children()
                .iter()
                .find(|c| c.local_name() == "nvSpPr")
                .and_then(|nv| nv.children().iter().find(|c| c.local_name() == "cNvPr"))
                .and_then(|c| c.attr("name"))
                .is_some_and(|n| n == "Slide Number");

            let mut paras = Vec::new();
            find_descendants(sp, "p", &mut paras);
            let mut shape_lines = Vec::new();
            for p in paras {
                let text = extract_a_paragraph_text(p, true);
                if !text.trim().is_empty() {
                    shape_lines.push(text);
                }
            }
            if is_slide_num_shape {
                continue;
            }
            if is_title_ph && title.is_none() && !shape_lines.is_empty() {
                title = Some(shape_lines.join(" "));
            }
            texts.extend(shape_lines);
        }
        if title.is_none() {
            title = texts.first().cloned();
        }

        let mut tables = Vec::new();
        let mut tbl_nodes = Vec::new();
        find_descendants(&root, "tbl", &mut tbl_nodes);
        for tbl in tbl_nodes {
            let mut tbl_rows = Vec::new();
            for tr in tbl.children().iter().filter(|n| n.local_name() == "tr") {
                let mut row_cells = Vec::new();
                for tc in tr.children().iter().filter(|n| n.local_name() == "tc") {
                    let mut paras = Vec::new();
                    find_descendants(tc, "p", &mut paras);
                    let cell_text: Vec<String> = paras
                        .into_iter()
                        .map(|p| extract_a_paragraph_text(p, false))
                        .filter(|s| !s.is_empty())
                        .collect();
                    row_cells.push(cell_text.join("\n"));
                }
                if !row_cells.is_empty() {
                    tbl_rows.push(row_cells);
                }
            }
            if !tbl_rows.is_empty() {
                tables.push(tbl_rows);
            }
        }

        let notes = resolve_notes_path(&zip, num).and_then(|notes_path| {
            let bytes = zip.read_entry(&notes_path).ok()?;
            let notes_root = parse_xml(&bytes, &notes_path).ok()?;
            let mut paras = Vec::new();
            find_descendants(&notes_root, "p", &mut paras);
            let lines: Vec<String> = paras
                .into_iter()
                .map(|p| extract_a_paragraph_text(p, true))
                .filter(|s| !s.trim().is_empty())
                .collect();
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        });

        slides_out.push(json!({
            "number": num,
            "title": title,
            "texts": texts,
            "tables": tables,
            "notes": notes,
        }));
    }

    json!({
        "path": path_str,
        "slide_count": total_slides,
        "slides": slides_out,
        "truncated": truncated,
    })
}

#[derive(Clone, Copy)]
struct SlideTheme {
    bg: &'static str,
    cover_bg: &'static str,
    accent: &'static str,
    accent_soft: &'static str,
    title_fg: &'static str,
    cover_title_fg: &'static str,
    cover_sub_fg: &'static str,
    body_fg: &'static str,
    muted_fg: &'static str,
    table_hdr_bg: &'static str,
    table_hdr_fg: &'static str,
    table_alt_bg: &'static str,
    table_border: &'static str,
}

fn resolve_slide_theme(name: Option<&str>) -> SlideTheme {
    match name.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("dark") => SlideTheme {
            bg: "0F172A",
            cover_bg: "0B1120",
            accent: "38BDF8",
            accent_soft: "1E293B",
            title_fg: "F8FAFC",
            cover_title_fg: "FFFFFF",
            cover_sub_fg: "CBD5E1",
            body_fg: "E2E8F0",
            muted_fg: "94A3B8",
            table_hdr_bg: "1E3A5F",
            table_hdr_fg: "FFFFFF",
            table_alt_bg: "1E293B",
            table_border: "334155",
        },
        Some("minimal") => SlideTheme {
            bg: "FFFFFF",
            cover_bg: "111827",
            accent: "059669",
            accent_soft: "ECFDF5",
            title_fg: "111827",
            cover_title_fg: "FFFFFF",
            cover_sub_fg: "D1FAE5",
            body_fg: "374151",
            muted_fg: "6B7280",
            table_hdr_bg: "111827",
            table_hdr_fg: "FFFFFF",
            table_alt_bg: "F9FAFB",
            table_border: "D1D5DB",
        },
        _ => SlideTheme {
            bg: "FFFFFF",
            cover_bg: "1F4E79",
            accent: "1F4E79",
            accent_soft: "E8EEF5",
            title_fg: "1F4E79",
            cover_title_fg: "FFFFFF",
            cover_sub_fg: "DCE6F2",
            body_fg: "1F2933",
            muted_fg: "64748B",
            table_hdr_bg: "1F4E79",
            table_hdr_fg: "FFFFFF",
            table_alt_bg: "F1F5F9",
            table_border: "CBD5E1",
        },
    }
}

#[derive(Clone, Debug, Default)]
struct ColumnSpec {
    title: Option<String>,
    bullets: Vec<String>,
    paragraphs: Vec<String>,
}

#[derive(Clone, Debug)]
struct SlideSpec {
    layout: String,
    title: String,
    subtitle: Option<String>,
    bullets: Vec<String>,
    paragraphs: Vec<String>,
    table: Option<TableData>,
    left: Option<ColumnSpec>,
    right: Option<ColumnSpec>,
    notes: Option<String>,
}

fn parse_column_spec(val: &Value) -> Result<ColumnSpec, String> {
    if let Some(s) = val.as_str() {
        return Ok(ColumnSpec {
            title: None,
            bullets: Vec::new(),
            paragraphs: vec![s.to_owned()],
        });
    }
    if let Some(arr) = val.as_array() {
        return Ok(ColumnSpec {
            title: None,
            bullets: arr.iter().map(value_to_string).collect(),
            paragraphs: Vec::new(),
        });
    }
    let obj = val
        .as_object()
        .ok_or_else(|| "column spec must be an object, list, or string".to_owned())?;
    let title = obj.get("title").and_then(Value::as_str).map(str::to_owned);
    let bullets = obj
        .get("bullets")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(value_to_string).collect())
        .unwrap_or_default();
    let mut paragraphs = Vec::new();
    if let Some(body) = obj.get("body").and_then(Value::as_str) {
        if !body.trim().is_empty() {
            paragraphs.push(body.to_owned());
        }
    }
    if let Some(arr) = obj.get("paragraphs").and_then(Value::as_array) {
        paragraphs.extend(arr.iter().map(value_to_string));
    }
    Ok(ColumnSpec {
        title,
        bullets,
        paragraphs,
    })
}

fn parse_slide_spec(idx: usize, val: &Value) -> Result<SlideSpec, String> {
    if let Some(raw) = val.as_str() {
        let mut title = String::new();
        let mut bullets = Vec::new();
        let mut paragraphs = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if title.is_empty() {
                if let Some((_, h)) = parse_md_heading(trimmed) {
                    title = h.to_owned();
                    continue;
                }
            }
            if let Some(b) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| trimmed.strip_prefix("• "))
            {
                bullets.push(b.to_owned());
            } else if title.is_empty() {
                title = trimmed.to_owned();
            } else {
                paragraphs.push(trimmed.to_owned());
            }
        }
        let layout = if idx == 0 && bullets.is_empty() && paragraphs.len() <= 1 {
            "title".to_owned()
        } else {
            "content".to_owned()
        };
        let subtitle = if layout == "title" && paragraphs.len() == 1 {
            paragraphs.pop()
        } else {
            None
        };
        return Ok(SlideSpec {
            layout,
            title,
            subtitle,
            bullets,
            paragraphs,
            table: None,
            left: None,
            right: None,
            notes: None,
        });
    }

    let obj = val
        .as_object()
        .ok_or_else(|| format!("slides[{idx}] must be an object or string"))?;
    let title = obj.get("title").map(value_to_string).unwrap_or_default();
    let subtitle = obj
        .get("subtitle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let bullets: Vec<String> = obj
        .get("bullets")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(value_to_string).collect())
        .unwrap_or_default();
    let mut paragraphs = Vec::new();
    if let Some(body) = obj.get("body").and_then(Value::as_str) {
        for part in body.split("\n\n") {
            let t = part.trim();
            if !t.is_empty() {
                paragraphs.push(t.to_owned());
            }
        }
    }
    if let Some(arr) = obj.get("paragraphs").and_then(Value::as_array) {
        paragraphs.extend(arr.iter().map(value_to_string));
    }
    let table = match obj.get("table") {
        Some(Value::Null) | None => None,
        Some(t) => Some(parse_table_value(t)?),
    };
    let left = match obj.get("left") {
        Some(Value::Null) | None => None,
        Some(v) => Some(parse_column_spec(v)?),
    };
    let right = match obj.get("right") {
        Some(Value::Null) | None => None,
        Some(v) => Some(parse_column_spec(v)?),
    };
    let notes = obj
        .get("notes")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let has_body = !bullets.is_empty()
        || !paragraphs.is_empty()
        || table.is_some()
        || left.is_some()
        || right.is_some();
    let layout = obj
        .get("layout")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| {
            if idx == 0 && !has_body {
                "title".to_owned()
            } else {
                "content".to_owned()
            }
        });

    Ok(SlideSpec {
        layout,
        title,
        subtitle,
        bullets,
        paragraphs,
        table,
        left,
        right,
        notes,
    })
}

fn pptx_push_inline_runs(
    out: &mut String,
    text: &str,
    size_hundredths: u32,
    default_color: &str,
    default_bold: bool,
) {
    let runs = parse_inline_runs(text);
    if runs.is_empty() {
        let _ = write!(
            out,
            r#"<a:endParaRPr lang="ru-RU" sz="{size_hundredths}" dirty="0"/>"#
        );
        return;
    }
    for run in runs {
        let lines: Vec<&str> = run.text.split('\n').collect();
        for (l_idx, part) in lines.iter().enumerate() {
            if l_idx > 0 {
                let _ = write!(
                    out,
                    r#"<a:br><a:rPr lang="ru-RU" sz="{size_hundredths}" dirty="0"/></a:br>"#
                );
            }
            if part.is_empty() {
                continue;
            }
            let b_attr = if default_bold || run.bold {
                r#" b="1""#
            } else {
                ""
            };
            let i_attr = if run.italic { r#" i="1""# } else { "" };
            let u_attr = if run.underline { r#" u="sng""# } else { "" };
            let s_attr = if run.strike {
                r#" strike="sngStrike""#
            } else {
                ""
            };
            let color = run.color.as_deref().unwrap_or(default_color);
            let font = if run.code { "Consolas" } else { "Calibri" };
            let sz = run
                .size_pt
                .map(|p| (p * 100.0).round().clamp(800.0, 9600.0) as u32)
                .unwrap_or(size_hundredths);
            let escaped = xml_escape(part);
            let _ = write!(
                out,
                r#"<a:r><a:rPr lang="ru-RU" sz="{sz}"{b_attr}{i_attr}{u_attr}{s_attr} dirty="0"><a:solidFill><a:srgbClr val="{color}"/></a:solidFill><a:latin typeface="{font}"/><a:ea typeface="{font}"/><a:cs typeface="{font}"/></a:rPr><a:t>{escaped}</a:t></a:r>"#
            );
        }
    }
    let _ = write!(
        out,
        r#"<a:endParaRPr lang="ru-RU" sz="{size_hundredths}" dirty="0"/>"#
    );
}

#[derive(Clone, Copy)]
struct EmuRect {
    x: i64,
    y: i64,
    cx: i64,
    cy: i64,
}

impl EmuRect {
    const fn new(x: i64, y: i64, cx: i64, cy: i64) -> Self {
        Self { x, y, cx, cy }
    }
}

fn pptx_push_rect(out: &mut String, shape_id: &mut u32, name: &str, rect: EmuRect, fill_hex: &str) {
    let EmuRect { x, y, cx, cy } = rect;
    let id = *shape_id;
    *shape_id += 1;
    let esc_name = xml_escape(name);
    let _ = write!(
        out,
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{esc_name}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="{fill_hex}"/></a:solidFill><a:ln><a:noFill/></a:ln></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="ru-RU"/></a:p></p:txBody></p:sp>"#
    );
}

fn pptx_push_textbox(
    out: &mut String,
    shape_id: &mut u32,
    name: &str,
    rect: EmuRect,
    anchor: &str,
    paragraphs_xml: &str,
) {
    let EmuRect { x, y, cx, cy } = rect;
    let id = *shape_id;
    *shape_id += 1;
    let esc_name = xml_escape(name);
    let body = if paragraphs_xml.is_empty() {
        r#"<a:p><a:endParaRPr lang="ru-RU"/></a:p>"#
    } else {
        paragraphs_xml
    };
    let _ = write!(
        out,
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{esc_name}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody><a:bodyPr wrap="square" lIns="91440" tIns="45720" rIns="91440" bIns="45720" anchor="{anchor}"/><a:lstStyle/>{body}</p:txBody></p:sp>"#
    );
}

fn pptx_render_content_paragraphs(
    heading: Option<&str>,
    paragraphs: &[String],
    bullets: &[String],
    theme: SlideTheme,
    font_sz: u32,
) -> String {
    let mut xml = String::new();
    if let Some(h) = heading {
        if !h.trim().is_empty() {
            xml.push_str(r#"<a:p><a:pPr algn="l"><a:spcAft><a:spcPts val="1000"/></a:spcAft><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(&mut xml, h, font_sz + 300, theme.accent, true);
            xml.push_str("</a:p>");
        }
    }
    for p in paragraphs {
        xml.push_str(
            r#"<a:p><a:pPr algn="l"><a:lnSpc><a:spcPct val="120000"/></a:lnSpc><a:spcAft><a:spcPts val="1000"/></a:spcAft><a:buNone/></a:pPr>"#,
        );
        pptx_push_inline_runs(&mut xml, p, font_sz, theme.body_fg, false);
        xml.push_str("</a:p>");
    }
    for b in bullets {
        let _ = write!(
            xml,
            r#"<a:p><a:pPr marL="342900" indent="-228600" algn="l"><a:lnSpc><a:spcPct val="120000"/></a:lnSpc><a:spcAft><a:spcPts val="800"/></a:spcAft><a:buClr><a:srgbClr val="{}"/></a:buClr><a:buFont typeface="Arial"/><a:buChar char="•"/></a:pPr>"#,
            theme.accent
        );
        pptx_push_inline_runs(&mut xml, b, font_sz, theme.body_fg, false);
        xml.push_str("</a:p>");
    }
    xml
}

fn pptx_push_table(
    out: &mut String,
    shape_id: &mut u32,
    tbl: &TableData,
    rect: EmuRect,
    theme: SlideTheme,
) {
    let EmuRect { x, y, cx, cy } = rect;
    let id = *shape_id;
    *shape_id += 1;
    let col_count = tbl
        .headers
        .as_ref()
        .map(Vec::len)
        .unwrap_or_else(|| tbl.rows.first().map(Vec::len).unwrap_or(1))
        .max(1);
    let total_rows = tbl.rows.len() + usize::from(tbl.headers.is_some());
    let col_w = (cx / (col_count as i64)).max(100_000);
    let row_h = (cy / (total_rows.max(1) as i64)).clamp(280_000, 580_000);
    let frame_h = row_h.saturating_mul(total_rows.max(1) as i64);
    let cell_font_sz = if col_count >= 5 || total_rows >= 8 {
        1200
    } else {
        1400
    };

    let _ = write!(
        out,
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="{id}" name="Table {id}"/><p:cNvGraphicFramePr><a:graphicFrameLocks noGrp="1"/></p:cNvGraphicFramePr><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{frame_h}"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1" bandRow="1"><a:tableStyleId>{{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}}</a:tableStyleId></a:tblPr><a:tblGrid>"#
    );
    for _ in 0..col_count {
        let _ = write!(out, r#"<a:gridCol w="{col_w}"/>"#);
    }
    out.push_str("</a:tblGrid>");

    let border_xml = format!(
        r#"<a:lnL w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnL><a:lnR w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnR><a:lnT w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnT><a:lnB w="12700"><a:solidFill><a:srgbClr val="{b}"/></a:solidFill></a:lnB>"#,
        b = theme.table_border
    );

    if let Some(headers) = &tbl.headers {
        let _ = write!(out, r#"<a:tr h="{row_h}">"#);
        for cell in headers {
            out.push_str(r#"<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(out, cell, cell_font_sz, theme.table_hdr_fg, true);
            let _ = write!(
                out,
                r#"</a:p></a:txBody><a:tcPr marL="91440" marR="91440" marT="45720" marB="45720" anchor="ctr">{border_xml}<a:solidFill><a:srgbClr val="{}"/></a:solidFill></a:tcPr></a:tc>"#,
                theme.table_hdr_bg
            );
        }
        out.push_str("</a:tr>");
    }

    for (r_idx, row) in tbl.rows.iter().enumerate() {
        let _ = write!(out, r#"<a:tr h="{row_h}">"#);
        let fill = if r_idx % 2 == 1 {
            theme.table_alt_bg
        } else {
            theme.bg
        };
        for cell in row {
            out.push_str(r#"<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(out, cell, cell_font_sz, theme.body_fg, false);
            let _ = write!(
                out,
                r#"</a:p></a:txBody><a:tcPr marL="91440" marR="91440" marT="45720" marB="45720" anchor="ctr">{border_xml}<a:solidFill><a:srgbClr val="{fill}"/></a:solidFill></a:tcPr></a:tc>"#
            );
        }
        out.push_str("</a:tr>");
    }

    out.push_str("</a:tbl></a:graphicData></a:graphic></p:graphicFrame>");
}

fn pptx_render_slide_xml(
    slide: &SlideSpec,
    slide_idx: usize,
    total_slides: usize,
    theme: SlideTheme,
) -> String {
    let is_cover = slide.layout == "title" || slide.layout == "section";
    let bg_hex = if is_cover { theme.cover_bg } else { theme.bg };
    let mut xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld>
<p:bg><p:bgPr><a:solidFill><a:srgbClr val="{bg_hex}"/></a:solidFill><a:effectLst/></p:bgPr></p:bg>
<p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
"#
    );

    let mut shape_id = 2u32;

    if is_cover {
        // Left vertical accent stripe
        pptx_push_rect(
            &mut xml,
            &mut shape_id,
            "Accent Stripe",
            EmuRect::new(0, 0, 240_000, 6_858_000),
            theme.accent,
        );
        // Subtle horizontal divider above subtitle
        pptx_push_rect(
            &mut xml,
            &mut shape_id,
            "Cover Divider",
            EmuRect::new(914_400, 3_650_000, 2_200_000, 55_000),
            theme.accent_soft,
        );

        let mut title_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
        pptx_push_inline_runs(&mut title_p, &slide.title, 4000, theme.cover_title_fg, true);
        title_p.push_str("</a:p>");
        pptx_push_textbox(
            &mut xml,
            &mut shape_id,
            "Title",
            EmuRect::new(914_400, 1_750_000, 10_363_200, 1_750_000),
            "b",
            &title_p,
        );

        if let Some(sub) = &slide.subtitle {
            let mut sub_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(&mut sub_p, sub, 2000, theme.cover_sub_fg, false);
            sub_p.push_str("</a:p>");
            pptx_push_textbox(
                &mut xml,
                &mut shape_id,
                "Subtitle",
                EmuRect::new(914_400, 3_900_000, 10_363_200, 1_300_000),
                "t",
                &sub_p,
            );
        }
    } else {
        // Top accent ribbon
        pptx_push_rect(
            &mut xml,
            &mut shape_id,
            "Top Bar",
            EmuRect::new(0, 0, 12_192_000, 110_000),
            theme.accent,
        );

        // Slide title
        let mut title_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
        pptx_push_inline_runs(&mut title_p, &slide.title, 2800, theme.title_fg, true);
        title_p.push_str("</a:p>");
        pptx_push_textbox(
            &mut xml,
            &mut shape_id,
            "Title",
            EmuRect::new(685_800, 320_000, 10_820_400, 680_000),
            "ctr",
            &title_p,
        );

        let mut content_y = 1_120_000i64;
        if let Some(sub) = &slide.subtitle {
            let mut sub_p = String::from(r#"<a:p><a:pPr algn="l"><a:buNone/></a:pPr>"#);
            pptx_push_inline_runs(&mut sub_p, sub, 1500, theme.muted_fg, false);
            sub_p.push_str("</a:p>");
            pptx_push_textbox(
                &mut xml,
                &mut shape_id,
                "Subtitle",
                EmuRect::new(685_800, 1_020_000, 10_820_400, 420_000),
                "t",
                &sub_p,
            );
            content_y = 1_520_000;
        }
        let content_h = 6_250_000 - content_y;

        if slide.left.is_some() || slide.right.is_some() {
            // Explicit two-column slide
            if let Some(left) = &slide.left {
                let left_xml = pptx_render_content_paragraphs(
                    left.title.as_deref(),
                    &left.paragraphs,
                    &left.bullets,
                    theme,
                    1600,
                );
                pptx_push_textbox(
                    &mut xml,
                    &mut shape_id,
                    "Left Column",
                    EmuRect::new(685_800, content_y, 5_150_000, content_h),
                    "t",
                    &left_xml,
                );
            }
            if let Some(right) = &slide.right {
                let right_xml = pptx_render_content_paragraphs(
                    right.title.as_deref(),
                    &right.paragraphs,
                    &right.bullets,
                    theme,
                    1600,
                );
                pptx_push_textbox(
                    &mut xml,
                    &mut shape_id,
                    "Right Column",
                    EmuRect::new(6_356_200, content_y, 5_150_000, content_h),
                    "t",
                    &right_xml,
                );
            }
        } else {
            let has_text = !slide.bullets.is_empty() || !slide.paragraphs.is_empty();
            match (&slide.table, has_text) {
                (Some(tbl), true) => {
                    // Side-by-side: text on left, table on right
                    let text_xml = pptx_render_content_paragraphs(
                        None,
                        &slide.paragraphs,
                        &slide.bullets,
                        theme,
                        1600,
                    );
                    pptx_push_textbox(
                        &mut xml,
                        &mut shape_id,
                        "Content",
                        EmuRect::new(685_800, content_y, 4_700_000, content_h),
                        "t",
                        &text_xml,
                    );
                    pptx_push_table(
                        &mut xml,
                        &mut shape_id,
                        tbl,
                        EmuRect::new(5_650_000, content_y, 5_856_200, content_h.min(4_500_000)),
                        theme,
                    );
                }
                (Some(tbl), false) => {
                    pptx_push_table(
                        &mut xml,
                        &mut shape_id,
                        tbl,
                        EmuRect::new(685_800, content_y, 10_820_400, content_h.min(4_700_000)),
                        theme,
                    );
                }
                (None, true) => {
                    let font_sz = if slide.bullets.len() + slide.paragraphs.len() > 7 {
                        1600
                    } else {
                        1800
                    };
                    let text_xml = pptx_render_content_paragraphs(
                        None,
                        &slide.paragraphs,
                        &slide.bullets,
                        theme,
                        font_sz,
                    );
                    pptx_push_textbox(
                        &mut xml,
                        &mut shape_id,
                        "Content",
                        EmuRect::new(685_800, content_y, 10_820_400, content_h),
                        "t",
                        &text_xml,
                    );
                }
                (None, false) => {}
            }
        }

        // Subtle slide number footer
        let mut num_p = String::from(r#"<a:p><a:pPr algn="r"><a:buNone/></a:pPr>"#);
        let label = format!("{slide_idx} / {total_slides}");
        pptx_push_inline_runs(&mut num_p, &label, 1100, theme.muted_fg, false);
        num_p.push_str("</a:p>");
        pptx_push_textbox(
            &mut xml,
            &mut shape_id,
            "Slide Number",
            EmuRect::new(10_200_000, 6_380_000, 1_306_200, 320_000),
            "ctr",
            &num_p,
        );
    }

    xml.push_str("</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>\n");
    xml
}

fn pptx_notes_slide_xml(notes: &str) -> String {
    let mut paras = String::new();
    for line in notes.lines() {
        if line.is_empty() {
            paras.push_str(r#"<a:p><a:endParaRPr lang="ru-RU" dirty="0"/></a:p>"#);
        } else {
            let escaped = xml_escape(line);
            let _ = write!(
                paras,
                r#"<a:p><a:r><a:rPr lang="ru-RU" dirty="0"/><a:t>{escaped}</a:t></a:r><a:endParaRPr lang="ru-RU" dirty="0"/></a:p>"#
            );
        }
    }
    if paras.is_empty() {
        paras.push_str(r#"<a:p><a:endParaRPr lang="ru-RU" dirty="0"/></a:p>"#);
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:notes xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Slide Image Placeholder 1"/><p:cNvSpPr><a:spLocks noGrp="1" noRot="1" noChangeAspect="1"/></p:cNvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Notes Placeholder 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr/>
        <p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
  <p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:notes>
"#
    )
}

fn pptx_theme_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Hermes">
  <a:themeElements>
    <a:clrScheme name="Hermes">
      <a:dk1><a:srgbClr val="1F2933"/></a:dk1>
      <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
      <a:dk2><a:srgbClr val="1F4E79"/></a:dk2>
      <a:lt2><a:srgbClr val="E8EEF5"/></a:lt2>
      <a:accent1><a:srgbClr val="1F4E79"/></a:accent1>
      <a:accent2><a:srgbClr val="2E75B6"/></a:accent2>
      <a:accent3><a:srgbClr val="059669"/></a:accent3>
      <a:accent4><a:srgbClr val="D97706"/></a:accent4>
      <a:accent5><a:srgbClr val="7C3AED"/></a:accent5>
      <a:accent6><a:srgbClr val="DC2626"/></a:accent6>
      <a:hlink><a:srgbClr val="1D4ED8"/></a:hlink>
      <a:folHlink><a:srgbClr val="6D28D9"/></a:folHlink>
    </a:clrScheme>
    <a:fontScheme name="Hermes">
      <a:majorFont><a:latin typeface="Calibri"/><a:ea typeface="Calibri"/><a:cs typeface="Calibri"/></a:majorFont>
      <a:minorFont><a:latin typeface="Calibri"/><a:ea typeface="Calibri"/><a:cs typeface="Calibri"/></a:minorFont>
    </a:fontScheme>
    <a:fmtScheme name="Hermes">
      <a:fillStyleLst>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      </a:fillStyleLst>
      <a:lnStyleLst>
        <a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
        <a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
        <a:ln w="28575"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
      </a:lnStyleLst>
      <a:effectStyleLst>
        <a:effectStyle><a:effectLst/></a:effectStyle>
        <a:effectStyle><a:effectLst/></a:effectStyle>
        <a:effectStyle><a:effectLst/></a:effectStyle>
      </a:effectStyleLst>
      <a:bgFillStyleLst>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      </a:bgFillStyleLst>
    </a:fmtScheme>
  </a:themeElements>
  <a:objectDefaults/>
  <a:extraClrSchemeLst/>
</a:theme>
"#
    .to_owned()
}

/// Create a formatted 16:9 `.pptx` PowerPoint presentation inside the workspace.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.pptx` path inside the workspace
/// - `slides`: list of slide objects (`title`, `subtitle`, `layout`, `bullets`, `body`,
///   `paragraphs`, `table`, `left`, `right`, `notes`) or Markdown strings
/// - `title`: optional presentation metadata title
/// - `author`: optional author metadata (defaults to `"Hermes"`)
/// - `theme`: optional `"corporate"` (default), `"dark"`, or `"minimal"`
pub(crate) fn pptx_write(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "pptx_write requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, false) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let Some(slides_arr) = args.get("slides").and_then(Value::as_array) else {
        return json!({"error": "pptx_write requires a 'slides' list"});
    };
    if slides_arr.is_empty() {
        return json!({"error": "'slides' list must contain at least one slide"});
    }

    let mut slides = Vec::with_capacity(slides_arr.len());
    for (idx, item) in slides_arr.iter().enumerate() {
        match parse_slide_spec(idx, item) {
            Ok(s) => slides.push(s),
            Err(err) => return json!({"error": err}),
        }
    }

    let theme = resolve_slide_theme(args["theme"].as_str());
    let author = args["author"].as_str().unwrap_or("Hermes");
    let pres_title = args["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            slides
                .first()
                .map(|s| s.title.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("Presentation");
    let has_any_notes = slides.iter().any(|s| s.notes.is_some());

    let mut content_types = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
  <Override PartName="/ppt/presProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"/>
  <Override PartName="/ppt/viewProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"/>
  <Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/>
  <Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
  <Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>
"#,
    );
    if has_any_notes {
        content_types.push_str(
            r#"  <Override PartName="/ppt/notesMasters/notesMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"/>
"#,
        );
    }

    let mut pres_rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps" Target="presProps.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps" Target="viewProps.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles" Target="tableStyles.xml"/>
"#,
    );
    let mut next_pres_rid = 5usize;
    let notes_master_rid = if has_any_notes {
        let rid = next_pres_rid;
        next_pres_rid += 1;
        let _ = writeln!(
            pres_rels,
            r#"  <Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="notesMasters/notesMaster1.xml"/>"#
        );
        Some(rid)
    } else {
        None
    };

    let mut sld_id_lst = String::new();
    let mut owned_entries: Vec<(String, String)> = Vec::new();
    let mut notes_written = 0usize;

    for (idx, slide) in slides.iter().enumerate() {
        let k = idx + 1;
        let rid = next_pres_rid;
        next_pres_rid += 1;
        let slide_id = 256 + idx;

        let _ = writeln!(
            content_types,
            r#"  <Override PartName="/ppt/slides/slide{k}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        );
        let _ = writeln!(
            pres_rels,
            r#"  <Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{k}.xml"/>"#
        );
        let _ = writeln!(
            sld_id_lst,
            r#"    <p:sldId id="{slide_id}" r:id="rId{rid}"/>"#
        );

        let slide_xml = pptx_render_slide_xml(slide, k, slides.len(), theme);
        owned_entries.push((format!("ppt/slides/slide{k}.xml"), slide_xml));

        let mut slide_rels = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
"#,
        );
        if let Some(notes) = &slide.notes {
            notes_written += 1;
            let _ = writeln!(
                content_types,
                r#"  <Override PartName="/ppt/notesSlides/notesSlide{k}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/>"#
            );
            let _ = writeln!(
                slide_rels,
                r#"  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide{k}.xml"/>"#
            );
            owned_entries.push((
                format!("ppt/notesSlides/notesSlide{k}.xml"),
                pptx_notes_slide_xml(notes),
            ));
            let notes_rels = format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="../notesMasters/notesMaster1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="../slides/slide{k}.xml"/>
</Relationships>
"#
            );
            owned_entries.push((
                format!("ppt/notesSlides/_rels/notesSlide{k}.xml.rels"),
                notes_rels,
            ));
        }
        slide_rels.push_str("</Relationships>\n");
        owned_entries.push((format!("ppt/slides/_rels/slide{k}.xml.rels"), slide_rels));
    }

    content_types.push_str("</Types>\n");
    pres_rels.push_str("</Relationships>\n");

    let notes_master_lst = notes_master_rid
        .map(|rid| {
            format!(
                r#"  <p:notesMasterIdLst><p:notesMasterId r:id="rId{rid}"/></p:notesMasterIdLst>"#
            )
        })
        .unwrap_or_default();

    let presentation_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" saveSubsetFonts="1">
  <p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
{notes_master_lst}
  <p:sldIdLst>
{sld_id_lst}  </p:sldIdLst>
  <p:sldSz cx="12192000" cy="6858000"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>
"#
    );

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>
"#
    .to_owned();

    let slide_master_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
  <p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
  <p:txStyles>
    <p:titleStyle><a:lvl1pPr algn="l"><a:defRPr sz="4000" b="1"/></a:lvl1pPr></p:titleStyle>
    <p:bodyStyle><a:lvl1pPr algn="l"><a:defRPr sz="1800"/></a:lvl1pPr></p:bodyStyle>
    <p:otherStyle><a:defPPr><a:defRPr lang="ru-RU"/></a:defPPr></p:otherStyle>
  </p:txStyles>
</p:sldMaster>
"#
    .to_owned();

    let slide_master_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>
"#
    .to_owned();

    let slide_layout_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1">
  <p:cSld name="Blank">
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
    </p:spTree>
  </p:cSld>
  <p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sldLayout>
"#
    .to_owned();

    let slide_layout_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>
"#
    .to_owned();

    let pres_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentationPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>
"#
    .to_owned();

    let view_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:viewPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:normalViewPr><p:restoredLeft sz="15620"/><p:restoredTop sz="94660"/></p:normalViewPr>
  <p:slideViewPr>
    <p:cSldViewPr>
      <p:cViewPr varScale="1">
        <p:scale><a:sx n="100" d="100"/><a:sy n="100" d="100"/></p:scale>
        <p:origin x="0" y="0"/>
      </p:cViewPr>
      <p:guideLst/>
    </p:cSldViewPr>
  </p:slideViewPr>
  <p:gridSpacing cx="72008" cy="72008"/>
</p:viewPr>
"#
    .to_owned();

    let table_styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:tblStyleLst xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" def="{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}"/>
"#
    .to_owned();

    let app_props = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">
  <Slides>{}</Slides>
  <Notes>{}</Notes>
  <Application>Hermes</Application>
</Properties>
"#,
        slides.len(),
        notes_written
    );

    let mut all_entries: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types),
        ("_rels/.rels".into(), root_rels),
        ("ppt/presentation.xml".into(), presentation_xml),
        ("ppt/_rels/presentation.xml.rels".into(), pres_rels),
        ("ppt/slideMasters/slideMaster1.xml".into(), slide_master_xml),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
            slide_master_rels,
        ),
        ("ppt/slideLayouts/slideLayout1.xml".into(), slide_layout_xml),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
            slide_layout_rels,
        ),
        ("ppt/theme/theme1.xml".into(), pptx_theme_xml()),
        ("ppt/presProps.xml".into(), pres_props),
        ("ppt/viewProps.xml".into(), view_props),
        ("ppt/tableStyles.xml".into(), table_styles),
        (
            "docProps/core.xml".into(),
            core_props_xml(pres_title, author),
        ),
        ("docProps/app.xml".into(), app_props),
    ];

    if has_any_notes {
        let notes_master_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:notesMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Slide Image Placeholder 1"/><p:cNvSpPr><a:spLocks noGrp="1" noRot="1" noChangeAspect="1"/></p:cNvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="1143000"/><a:ext cx="5486400" cy="3086100"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/><a:ln w="12700"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill></a:ln></p:spPr>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Notes Placeholder 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="685800" y="4572000"/><a:ext cx="5486400" cy="3886200"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
        <p:txBody><a:bodyPr wrap="square" lIns="91440" tIns="45720" rIns="91440" bIns="45720"/><a:lstStyle/><a:p><a:endParaRPr lang="ru-RU"/></a:p></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
  <p:notesStyle>
    <a:lvl1pPr marL="0" algn="l"><a:defRPr sz="1200" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl1pPr>
  </p:notesStyle>
</p:notesMaster>
"#
        .to_owned();
        let notes_master_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>
"#
        .to_owned();
        all_entries.push(("ppt/notesMasters/notesMaster1.xml".into(), notes_master_xml));
        all_entries.push((
            "ppt/notesMasters/_rels/notesMaster1.xml.rels".into(),
            notes_master_rels,
        ));
    }

    all_entries.extend(owned_entries);

    if let Err(err) = write_zip_entries(&resolved, &all_entries) {
        return json!({"error": err});
    }

    json!({
        "status": "ok",
        "path": path_str,
        "slides_written": slides.len(),
        "notes_written": notes_written,
    })
}
