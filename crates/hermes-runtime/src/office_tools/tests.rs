use std::fs;
use std::path::Path;

use serde_json::json;

use super::*;
use crate::doc_extract::{extract_document_bytes, parse_xml, ZipArchive, MAX_DOCUMENT_BYTES};
use crate::excel_tools::write_file_atomically;

fn assert_all_xml_parts_well_formed(path: &Path, kind: &str) {
    let data = fs::read(path).expect("read archive");
    let zip = ZipArchive::from_bytes(&data, kind).expect("valid zip");
    for name in zip.entry_names() {
        if name.ends_with(".xml") || name.ends_with(".rels") {
            let bytes = zip.read_entry(name).expect("read zip entry");
            parse_xml(&bytes, name)
                .unwrap_or_else(|e| panic!("invalid XML in {kind} part '{name}': {e}"));
        }
    }
}

#[test]
fn roundtrips_docx_markdown_and_blocks_and_enforces_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let md = "# Финансовый отчёт\n\nФайл: **C:\\Temp\\hsr\\financial_stats_example.xlsx** и формула `=SUM(B2:B10)`.\n\n- Пункт *первый*\n- Пункт **второй**\n\n1. Шаг 1\n2. Шаг 2\n\n| Месяц | Выручка |\n|---|---|\n| Январь | 100 |\n| Февраль | 150 |\n\n> Важное примечание\n\n```python\nx = 1 + 2\n```\n";

    let write_res = docx_write(
        dir.path(),
        &json!({
            "path": "reports/summary.docx",
            "title": "Итоги года",
            "subtitle": "Автоматический отчёт",
            "content": md
        }),
    );
    assert_eq!(write_res["status"], "ok", "{write_res}");
    assert_eq!(write_res["tables_written"], 1);

    let full_path = dir.path().join("reports/summary.docx");
    assert_all_xml_parts_well_formed(&full_path, "DOCX");

    let read_res = docx_read(dir.path(), &json!({"path": "reports/summary.docx"}));
    assert!(read_res.get("error").is_none(), "{read_res}");
    let paras = read_res["paragraphs"].as_array().unwrap();
    assert!(paras.iter().any(|p| p == "Итоги года"), "{paras:?}");
    assert!(
        paras.iter().any(|p| p
            .as_str()
            .is_some_and(|s| s.contains(r"C:\Temp\hsr\financial_stats_example.xlsx"))),
        "{paras:?}"
    );
    assert_eq!(read_res["table_count"], 1);
    assert_eq!(
        read_res["tables"][0],
        json!([["Месяц", "Выручка"], ["Январь", "100"], ["Февраль", "150"]])
    );

    // Also readable via read_file's doc_extract
    let extracted = extract_document_bytes(
        &fs::read(&full_path).unwrap(),
        &full_path,
        "reports/summary.docx",
    )
    .unwrap();
    assert!(extracted.contains("Финансовый отчёт"));
    assert!(extracted.contains("Февраль"));

    // Structured blocks API
    let blocks_res = docx_write(
        dir.path(),
        &json!({
            "path": "reports/structured.docx",
            "blocks": [
                {"type": "heading", "level": 1, "text": "Overview"},
                {"type": "paragraph", "runs": [{"text": "Bold ", "bold": true}, {"text": "Blue", "color": "1F4E79"}]},
                {"type": "bullets", "items": ["Alpha", "Beta"]},
                {"type": "numbered", "items": ["First", "Second"]},
                {"type": "page_break"},
                {"type": "table", "headers": ["K", "V"], "rows": [["A", 1]]}
            ]
        }),
    );
    assert_eq!(blocks_res["status"], "ok", "{blocks_res}");
    assert_all_xml_parts_well_formed(&dir.path().join("reports/structured.docx"), "DOCX");

    // Workspace escape refused
    let escaped = docx_write(
        dir.path(),
        &json!({"path": "../outside.docx", "content": "nope"}),
    );
    assert!(escaped["error"].as_str().is_some());
}

#[test]
fn roundtrips_pptx_slides_tables_columns_and_notes_and_enforces_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let write_res = pptx_write(
        dir.path(),
        &json!({
            "path": "decks/q3_review.pptx",
            "title": "Q3 Business Review",
            "theme": "corporate",
            "slides": [
                {
                    "title": "Квартальный отчёт Q3",
                    "subtitle": "Финансовые итоги и планы"
                },
                {
                    "title": "Ключевые метрики",
                    "subtitle": "Рост по всем направлениям",
                    "bullets": [
                        "**Выручка**: +24% г/г",
                        "Файл: `C:\\Temp\\hsr\\financial_stats_example.xlsx`"
                    ],
                    "table": {
                        "headers": ["Месяц", "План", "Факт"],
                        "rows": [
                            ["Июль", 100, 112],
                            ["Август", 110, 128]
                        ]
                    },
                    "notes": "Подчеркнуть перевыполнение плана в августе."
                },
                {
                    "title": "Сравнение вариантов",
                    "left": {
                        "title": "Вариант A",
                        "bullets": ["Быстрый запуск", "Низкий бюджет"]
                    },
                    "right": {
                        "title": "Вариант B",
                        "bullets": ["Полная автоматизация", "Масштабирование"]
                    }
                }
            ]
        }),
    );
    assert_eq!(write_res["status"], "ok", "{write_res}");
    assert_eq!(write_res["slides_written"], 3);
    assert_eq!(write_res["notes_written"], 1);

    let full_path = dir.path().join("decks/q3_review.pptx");
    assert_all_xml_parts_well_formed(&full_path, "PPTX");

    // Verify PowerPoint-critical ECMA-376 invariants (valid viewProps, app.xml order,
    // notesMaster placeholders + notesStyle, no extraneous theme rel on presentation.xml).
    let raw_pptx = fs::read(&full_path).unwrap();
    let pptx_zip = ZipArchive::from_bytes(&raw_pptx, "PPTX").unwrap();
    let view_props_str =
        String::from_utf8(pptx_zip.read_entry("ppt/viewProps.xml").unwrap()).unwrap();
    assert!(view_props_str.contains("<p:cSldViewPr>"));
    let app_xml_str = String::from_utf8(pptx_zip.read_entry("docProps/app.xml").unwrap()).unwrap();
    assert!(
        app_xml_str.find("<Slides>").unwrap() < app_xml_str.find("<Application>").unwrap(),
        "app.xml must place <Slides> before <Application>:\n{app_xml_str}"
    );
    let pres_rels_str = String::from_utf8(
        pptx_zip
            .read_entry("ppt/_rels/presentation.xml.rels")
            .unwrap(),
    )
    .unwrap();
    assert!(!pres_rels_str.contains("relationships/theme"));
    let notes_master_str = String::from_utf8(
        pptx_zip
            .read_entry("ppt/notesMasters/notesMaster1.xml")
            .unwrap(),
    )
    .unwrap();
    assert!(notes_master_str.contains(r#"type="sldImg""#));
    assert!(notes_master_str.contains(r#"type="body" idx="1""#));
    assert!(notes_master_str.contains("<p:notesStyle>"));
    let notes_slide_str = String::from_utf8(
        pptx_zip
            .read_entry("ppt/notesSlides/notesSlide2.xml")
            .unwrap(),
    )
    .unwrap();
    assert!(notes_slide_str.contains(r#"type="sldImg""#));
    assert!(notes_slide_str.contains(r#"type="body" idx="1""#));

    let read_res = pptx_read(dir.path(), &json!({"path": "decks/q3_review.pptx"}));
    assert!(read_res.get("error").is_none(), "{read_res}");
    assert_eq!(read_res["slide_count"], 3);
    let slides = read_res["slides"].as_array().unwrap();
    assert_eq!(slides[0]["title"], "Квартальный отчёт Q3");
    assert_eq!(slides[1]["title"], "Ключевые метрики");
    assert_eq!(
        slides[1]["notes"],
        "Подчеркнуть перевыполнение плана в августе."
    );
    assert_eq!(
        slides[1]["tables"][0],
        json!([
            ["Месяц", "План", "Факт"],
            ["Июль", "100", "112"],
            ["Август", "110", "128"]
        ])
    );
    assert!(slides[2]["texts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t == "Вариант B"));

    // Also readable via read_file's doc_extract
    let extracted = extract_document_bytes(
        &fs::read(&full_path).unwrap(),
        &full_path,
        "decks/q3_review.pptx",
    )
    .unwrap();
    assert!(extracted.contains("# ── Slide 1 ──"));
    assert!(extracted.contains("Квартальный отчёт Q3"));
    assert!(extracted.contains("Сравнение вариантов"));

    // Workspace escape refused
    let escaped = pptx_write(
        dir.path(),
        &json!({"path": "../outside.pptx", "slides": [{"title": "x"}]}),
    );
    assert!(escaped["error"].as_str().is_some());
}

#[test]
fn docx_write_markdown_tables_lists_headings_code_and_quotes_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let md = "\
# Ежеквартальный отчёт

Вводный абзац с **жирным**, *курсивом* и `inline_code`.

## Показатели

| Квартал | Выручка | Рост |
| :--- | ---: | :---: |
| Q1 | 100 | +10% |
| Q2 | 125 | +25% |

- Первый пункт списка
- Второй пункт списка
1. Шаг один
2. Шаг два

> Важная цитата руководства

```rust
fn main() {}
```";
    let res = docx_write(
        dir.path(),
        &json!({
            "path": "reports/markdown_doc.docx",
            "title": "Отчёт",
            "content": md
        }),
    );
    assert_eq!(res["status"], "ok", "{res}");
    let full_path = dir.path().join("reports/markdown_doc.docx");
    assert_all_xml_parts_well_formed(&full_path, "DOCX");

    let read_res = docx_read(dir.path(), &json!({"path": "reports/markdown_doc.docx"}));
    assert!(read_res.get("error").is_none(), "{read_res}");
    let paragraphs = read_res["paragraphs"].as_array().unwrap();
    assert!(paragraphs.iter().any(|p| p == "Ежеквартальный отчёт"));
    assert!(paragraphs.iter().any(|p| p == "Первый пункт списка"));
    assert!(paragraphs.iter().any(|p| p == "Шаг два"));
    assert!(paragraphs.iter().any(|p| p == "Важная цитата руководства"));
    assert!(paragraphs.iter().any(|p| p == "fn main() {}"));
    let tables = read_res["tables"].as_array().unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(
        tables[0],
        json!([
            ["Квартал", "Выручка", "Рост"],
            ["Q1", "100", "+10%"],
            ["Q2", "125", "+25%"]
        ])
    );
}

#[test]
fn office_atomic_write_preserves_existing_file_on_failure_and_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("existing.docx");
    fs::write(&target, b"ORIGINAL-CONTENT").unwrap();

    let err = write_file_atomically(&target, |file| {
        file.write_all(b"PARTIAL-DATA").unwrap();
        Err("simulated mid-write failure".into())
    });
    assert!(err.is_err());
    assert_eq!(fs::read(&target).unwrap(), b"ORIGINAL-CONTENT");

    // No leftover temporary files remain in the directory.
    let names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
        .collect();
    assert_eq!(names, vec!["existing.docx".to_owned()]);

    // Atomic overwrite succeeds cleanly and replaces contents.
    write_file_atomically(&target, |file| {
        file.write_all(b"NEW-CONTENT").map_err(|e| e.to_string())
    })
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"NEW-CONTENT");
}

#[test]
fn office_tools_reject_protected_paths_and_corrupt_or_oversized_or_zipbomb_archives() {
    let dir = tempfile::tempdir().unwrap();

    // 1. Protected repository/credential/profile directories are refused even inside workspace.
    for bad in [
        ".git/config.docx",
        ".ssh/id_rsa.docx",
        ".hsr/config.yaml",
        ".hermes/auth.json",
        ".env",
    ] {
        let w = docx_write(dir.path(), &json!({"path": bad, "paragraphs": ["x"]}));
        assert!(w["error"].as_str().is_some(), "expected refusal for {bad}");
        assert!(!dir.path().join(bad).exists());
        let p = pptx_write(
            dir.path(),
            &json!({"path": bad, "slides": [{"title": "x"}]}),
        );
        assert!(p["error"].as_str().is_some(), "expected refusal for {bad}");
    }

    // 2. Corrupt / truncated archive returns a clean error without panicking.
    let corrupt = dir.path().join("broken.docx");
    fs::write(&corrupt, b"PK\x03\x04not-a-real-zip").unwrap();
    let bad_docx = docx_read(dir.path(), &json!({"path": "broken.docx"}));
    assert!(bad_docx["error"].as_str().is_some());
    let bad_pptx = pptx_read(dir.path(), &json!({"path": "broken.docx"}));
    assert!(bad_pptx["error"].as_str().is_some());

    // 3. Multi-entry zip bomb: 6 slides x 10 MiB of compressible text (~60 MiB uncompressed
    //    total > 50 MiB budget, while compressed ZIP is tiny). Must be rejected!
    let bomb_path = dir.path().join("bomb.pptx");
    let chunk = "A".repeat(10 * 1024 * 1024);
    let entries: Vec<(String, String)> = (1..=6)
        .map(|n| {
            (
                format!("ppt/slides/slide{n}.xml"),
                format!("<p>{chunk}</p>"),
            )
        })
        .collect();
    write_zip_entries(&bomb_path, &entries).unwrap();
    assert!(
        fs::metadata(&bomb_path).unwrap().len() < 500_000,
        "compressed bomb fixture should be small on disk"
    );
    let bomb_res = pptx_read(dir.path(), &json!({"path": "bomb.pptx"}));
    let bomb_err = bomb_res["error"]
        .as_str()
        .expect("zip bomb must be rejected by cumulative decompression budget");
    assert!(
        bomb_err.contains("uncompressed size limit") || bomb_err.contains("exceeds"),
        "unexpected error: {bomb_err}"
    );

    // 4. Oversized file (> MAX_DOCUMENT_BYTES) rejected via metadata before reading into RAM.
    let huge_path = dir.path().join("huge.docx");
    let huge_file = fs::File::create(&huge_path).unwrap();
    huge_file.set_len(MAX_DOCUMENT_BYTES + 1024).unwrap();
    drop(huge_file);
    let huge_res = docx_read(dir.path(), &json!({"path": "huge.docx"}));
    assert!(
        huge_res["error"]
            .as_str()
            .is_some_and(|e| e.contains("exceeds maximum size")),
        "expected size error, got: {huge_res}"
    );
}
