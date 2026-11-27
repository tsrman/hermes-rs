//! Pure-Rust Excel (`.xlsx` / `.xls` / `.xlsb` / `.ods`) helpers exposed to `execute_code`
//! scripts via `from hermes_tools import xlsx_read, xlsx_write`.
//!
//! Runs natively inside `hermes-runtime` on every platform (Windows, Linux GNU/musl, macOS) with
//! zero external Python packages required.

use crate::paths::PathExt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use calamine::{open_workbook_auto, Data, Reader};
use rust_xlsxwriter::{Color, Format, Formula, Workbook};
use serde_json::{json, Map, Value};

use crate::redact::is_secret_file_arg;

const DEFAULT_MAX_ROWS: usize = 5_000;
const HARD_MAX_ROWS: usize = 50_000;

/// Resolve a user/script path safely inside `workspace_root`, rejecting `..` escapes and
/// credential/secret files.
pub(crate) fn resolve_workspace_path(
    workspace_root: &Path,
    raw: &str,
    must_exist: bool,
) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("path cannot be empty".into());
    }
    let canon_root = workspace_root
        .canonical()
        .map_err(|e| format!("invalid workspace root: {e}"))?;
    let requested = Path::new(trimmed);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        canon_root.join(requested)
    };

    // Normalize lexical components so non-existent output paths can still be containment-checked.
    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("path escapes workspace: {trimmed}"));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    let checked = if must_exist {
        normalized
            .canonical()
            .map_err(|_| format!("file not found: {trimmed}"))?
    } else {
        // Canonicalize the deepest existing ancestor so symlinks cannot escape the workspace.
        let mut ancestor = normalized.as_path();
        let mut suffix: Vec<std::ffi::OsString> = Vec::new();
        while !ancestor.exists() {
            let Some(name) = ancestor.file_name() else {
                break;
            };
            suffix.push(name.to_os_string());
            match ancestor.parent() {
                Some(parent) => ancestor = parent,
                None => break,
            }
        }
        let mut resolved = ancestor
            .canonical()
            .map_err(|e| format!("cannot resolve path parent for {trimmed}: {e}"))?;
        for part in suffix.into_iter().rev() {
            resolved.push(part);
        }
        resolved
    };

    if !checked.starts_with(&canon_root) {
        return Err(format!(
            "path must stay inside the active workspace ({}): {trimmed}",
            canon_root.display()
        ));
    }
    if checked.to_str().is_some_and(|p| is_secret_file_arg(p, &[])) {
        return Err(format!(
            "refused to access secret/credential file: {trimmed}"
        ));
    }
    Ok(checked)
}

fn cell_to_json(cell: &Data) -> Value {
    match cell {
        Data::Int(v) => json!(v),
        Data::Float(v) => {
            if v.is_finite() {
                json!(v)
            } else {
                Value::Null
            }
        }
        Data::String(s) => json!(s),
        Data::Bool(b) => json!(b),
        Data::DateTime(dt) => dt
            .as_datetime()
            .map(|d| json!(d.format("%Y-%m-%d %H:%M:%S").to_string()))
            .unwrap_or_else(|| json!(dt.as_f64())),
        Data::DateTimeIso(s) | Data::DurationIso(s) => json!(s),
        Data::Error(err) => json!(format!("{err}")),
        Data::Empty => Value::Null,
    }
}

/// Read an Excel / ODS spreadsheet (`.xlsx`, `.xls`, `.xlsb`, `.ods`) into structured JSON rows.
///
/// Arguments:
/// - `path`: workspace-relative or absolute path inside the workspace
/// - `sheet`: optional sheet name (reads all sheets when omitted)
/// - `max_rows`: optional row cap per sheet (default 5000, max 50000)
pub(crate) fn xlsx_read(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "xlsx_read requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, true) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let requested_sheet = args["sheet"].as_str();
    let max_rows = args["max_rows"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(DEFAULT_MAX_ROWS)
        .clamp(1, HARD_MAX_ROWS);

    let mut workbook = match open_workbook_auto(&resolved) {
        Ok(wb) => wb,
        Err(err) => return json!({"error": format!("failed to open spreadsheet: {err}")}),
    };

    let all_names = workbook.sheet_names().to_vec();
    let target_names: Vec<String> = match requested_sheet {
        Some(name) => {
            if !all_names.iter().any(|s| s == name) {
                return json!({
                    "error": format!("sheet '{name}' not found"),
                    "sheet_names": all_names,
                });
            }
            vec![name.to_owned()]
        }
        None => all_names.clone(),
    };

    let mut sheets = Map::new();
    let mut truncated = false;
    for name in target_names {
        let range = match workbook.worksheet_range(&name) {
            Ok(r) => r,
            Err(err) => {
                return json!({
                    "error": format!("failed reading sheet '{name}': {err}"),
                    "sheet_names": all_names,
                });
            }
        };
        let mut rows = Vec::new();
        for (idx, row) in range.rows().enumerate() {
            if idx >= max_rows {
                truncated = true;
                break;
            }
            let mut values: Vec<Value> = row.iter().map(cell_to_json).collect();
            while values.last().is_some_and(Value::is_null) {
                values.pop();
            }
            rows.push(Value::Array(values));
        }
        sheets.insert(name, Value::Array(rows));
    }

    json!({
        "path": path_str,
        "sheet_names": all_names,
        "sheets": sheets,
        "truncated": truncated,
    })
}

struct SheetSpec {
    name: String,
    headers: Option<Vec<Value>>,
    rows: Vec<Vec<Value>>,
    autofit: bool,
    freeze_header: bool,
}

fn parse_sheet_specs(sheets_val: &Value) -> Result<Vec<SheetSpec>, String> {
    match sheets_val {
        Value::Object(map) => {
            if map.is_empty() {
                return Err("'sheets' must contain at least one sheet".into());
            }
            let mut out = Vec::with_capacity(map.len());
            for (name, val) in map {
                let rows_arr = val
                    .as_array()
                    .ok_or_else(|| format!("sheet '{name}' must be a list of rows"))?;
                let mut rows = Vec::with_capacity(rows_arr.len());
                for row in rows_arr {
                    let cells = row
                        .as_array()
                        .ok_or_else(|| format!("each row in sheet '{name}' must be a list"))?;
                    rows.push(cells.clone());
                }
                out.push(SheetSpec {
                    name: name.clone(),
                    headers: None,
                    rows,
                    autofit: true,
                    freeze_header: false,
                });
            }
            Ok(out)
        }
        Value::Array(list) => {
            if list.is_empty() {
                return Err("'sheets' list must contain at least one sheet".into());
            }
            let mut out = Vec::with_capacity(list.len());
            for (idx, item) in list.iter().enumerate() {
                let obj = item
                    .as_object()
                    .ok_or_else(|| format!("sheets[{idx}] must be an object"))?;
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Sheet{}", idx + 1));
                let headers = match obj.get("headers") {
                    Some(Value::Array(h)) => Some(h.clone()),
                    Some(Value::Null) | None => None,
                    _ => return Err(format!("sheet '{name}': 'headers' must be a list")),
                };
                let rows_arr = obj
                    .get("rows")
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("sheet '{name}' requires a 'rows' list"))?;
                let mut rows = Vec::with_capacity(rows_arr.len());
                for row in rows_arr {
                    let cells = row
                        .as_array()
                        .ok_or_else(|| format!("each row in sheet '{name}' must be a list"))?;
                    rows.push(cells.clone());
                }
                let autofit = obj.get("autofit").and_then(Value::as_bool).unwrap_or(true);
                let freeze_header = obj
                    .get("freeze_header")
                    .and_then(Value::as_bool)
                    .unwrap_or(headers.is_some());
                out.push(SheetSpec {
                    name,
                    headers,
                    rows,
                    autofit,
                    freeze_header,
                });
            }
            Ok(out)
        }
        _ => Err("'sheets' must be a dict {sheet_name: [[...]]} or list of sheet objects".into()),
    }
}

fn write_cell(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    val: &Value,
    format: Option<&Format>,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    match val {
        Value::Null => Ok(()),
        Value::Bool(b) => {
            if let Some(fmt) = format {
                worksheet.write_boolean_with_format(row, col, *b, fmt)?;
            } else {
                worksheet.write_boolean(row, col, *b)?;
            }
            Ok(())
        }
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if let Some(fmt) = format {
                    worksheet.write_number_with_format(row, col, f, fmt)?;
                } else {
                    worksheet.write_number(row, col, f)?;
                }
            }
            Ok(())
        }
        Value::String(s) => {
            if s.starts_with('=') && s.len() > 1 {
                let formula = Formula::new(s);
                if let Some(fmt) = format {
                    worksheet.write_formula_with_format(row, col, formula, fmt)?;
                } else {
                    worksheet.write_formula(row, col, formula)?;
                }
            } else if let Some(fmt) = format {
                worksheet.write_string_with_format(row, col, s, fmt)?;
            } else {
                worksheet.write_string(row, col, s)?;
            }
            Ok(())
        }
        other => {
            let text = other.to_string();
            if let Some(fmt) = format {
                worksheet.write_string_with_format(row, col, &text, fmt)?;
            } else {
                worksheet.write_string(row, col, &text)?;
            }
            Ok(())
        }
    }
}

/// Write a new `.xlsx` spreadsheet inside the workspace with formulas, styling, and autofit.
///
/// Arguments:
/// - `path`: workspace-relative or absolute `.xlsx` path inside the workspace
/// - `sheets`: dict `{"Sheet1": [[...], ...]}` or list `[{"name": "...", "headers": [...], "rows": [[...]], "autofit": true}]`
pub(crate) fn xlsx_write(workspace_root: &Path, args: &Value) -> Value {
    let Some(path_str) = args["path"].as_str() else {
        return json!({"error": "xlsx_write requires a 'path' string argument"});
    };
    let resolved = match resolve_workspace_path(workspace_root, path_str, false) {
        Ok(p) => p,
        Err(err) => return json!({"error": err}),
    };
    let specs = match parse_sheet_specs(&args["sheets"]) {
        Ok(s) => s,
        Err(err) => return json!({"error": err}),
    };

    if let Some(parent) = resolved.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            return json!({"error": format!("failed creating parent directory: {err}")});
        }
    }

    let mut workbook = Workbook::new();
    let header_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xE8EEF5));
    let mut total_rows = 0usize;

    for spec in &specs {
        let worksheet = workbook.add_worksheet();
        if let Err(err) = worksheet.set_name(&spec.name) {
            return json!({"error": format!("invalid sheet name '{}': {err}", spec.name)});
        }
        let mut row_idx = 0u32;
        if let Some(headers) = &spec.headers {
            for (col_idx, cell) in headers.iter().enumerate() {
                if let Ok(col) = u16::try_from(col_idx) {
                    if let Err(err) = write_cell(worksheet, row_idx, col, cell, Some(&header_fmt)) {
                        return json!({"error": format!("failed writing header cell: {err}")});
                    }
                }
            }
            row_idx += 1;
            total_rows += 1;
        }
        for row in &spec.rows {
            for (col_idx, cell) in row.iter().enumerate() {
                if let Ok(col) = u16::try_from(col_idx) {
                    if let Err(err) = write_cell(worksheet, row_idx, col, cell, None) {
                        return json!({"error": format!("failed writing cell ({row_idx}, {col}): {err}")});
                    }
                }
            }
            row_idx += 1;
            total_rows += 1;
        }
        if spec.freeze_header {
            let _ = worksheet.set_freeze_panes(1, 0);
        }
        if spec.autofit {
            worksheet.autofit();
        }
    }

    if let Err(err) = workbook.save(&resolved) {
        return json!({"error": format!("failed saving workbook: {err}")});
    }

    json!({
        "status": "ok",
        "path": path_str,
        "sheets_written": specs.len(),
        "rows_written": total_rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_xlsx_write_and_read_with_formulas_and_enforces_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let write_res = xlsx_write(
            dir.path(),
            &json!({
                "path": "reports/sales.xlsx",
                "sheets": [
                    {
                        "name": "Итоги",
                        "headers": ["Товар", "Сумма", "Активен"],
                        "rows": [
                            ["Кофе", 150.5, true],
                            ["Чай", 99.5, false],
                            ["Итого", "=SUM(B2:B3)", null]
                        ]
                    }
                ]
            }),
        );
        assert_eq!(write_res["status"], "ok", "{write_res}");
        assert_eq!(write_res["sheets_written"], 1);
        assert_eq!(write_res["rows_written"], 4);

        let read_res = xlsx_read(dir.path(), &json!({"path": "reports/sales.xlsx"}));
        assert!(read_res.get("error").is_none(), "{read_res}");
        assert_eq!(read_res["sheet_names"], json!(["Итоги"]));
        let rows = read_res["sheets"]["Итоги"].as_array().unwrap();
        assert_eq!(rows[0], json!(["Товар", "Сумма", "Активен"]));
        assert_eq!(rows[1], json!(["Кофе", 150.5, true]));
        assert_eq!(rows[2], json!(["Чай", 99.5, false]));

        // Rejects workspace escape
        let escaped = xlsx_write(
            dir.path(),
            &json!({
                "path": "../outside.xlsx",
                "sheets": {"S": [["x"]]}
            }),
        );
        assert!(escaped["error"].as_str().is_some());
    }
}
